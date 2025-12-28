//! Message queue service.
//!
//! ## Design Philosophy
//!
//! mikcar acts as an **HTTP proxy** to queue infrastructure, not a reimplementation.
//!
//! ```text
//! WASM Handler → HTTP → mikcar (proxy) → Redis/RabbitMQ
//! ```
//!
//! ## Platform Support
//!
//! - **Linux/macOS**: Redis Streams and `RabbitMQ` via omniqueue
//! - **Windows**: In-memory only (omniqueue requires cmake)
//!
//! ## Supported Backends (Linux/macOS)
//!
//! - `memory://` - In-memory (tokio mpsc) - development only
//! - `redis://host:port/queue_key` - Redis Streams - production ready
//! - `amqp://user:pass@host:port/queue` - `RabbitMQ` - production ready
//!
//! ## Windows Limitation
//!
//! Windows builds only support `memory://` backend. For production on Windows,
//! deploy mikcar in a Linux container.
//!
//! ## API Design
//!
//! ### Work Queues (At-least-once delivery)
//! - POST /push/{queue} - Push message to queue
//! - GET /pop/{queue}?timeout=30 - Pop message from queue (long-poll)
//! - GET /len/{queue} - Get queue length (backend-dependent)
//!
//! ### Pub/Sub (Topic-based) - In-memory only
//! - POST /publish/{topic} - Publish message to topic
//! - `GET /subscribe/{topic}?timeout=30&max_messages=10` - Long-poll for messages
//! - POST /ack/{topic}/{id} - Acknowledge message
//!
//! Note: For production pub/sub, use native client libraries or dedicated services.
//! omniqueue is optimized for work queue semantics (each message to one consumer).

use crate::{Error, Result, Sidecar};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

// ============================================================================
// In-memory backend types (always available)
// ============================================================================

type InMemoryProducer = tokio::sync::mpsc::UnboundedSender<serde_json::Value>;
type InMemoryConsumer = Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>>;

/// In-memory queue for a single topic/queue.
#[derive(Clone)]
struct InMemoryQueue {
    producer: InMemoryProducer,
    consumer: InMemoryConsumer,
}

// ============================================================================
// Backend abstraction
// ============================================================================

/// Queue backend type.
#[derive(Clone)]
enum QueueBackend {
    /// In-memory backend (development only)
    InMemory {
        topics: Arc<Mutex<HashMap<String, InMemoryQueue>>>,
        queues: Arc<Mutex<HashMap<String, InMemoryQueue>>>,
    },
    /// omniqueue-based backend (Linux/macOS only)
    #[cfg(not(target_os = "windows"))]
    Omniqueue(OmniqueueBackend),
}

/// omniqueue backend wrapper.
#[cfg(not(target_os = "windows"))]
#[derive(Clone)]
struct OmniqueueBackend {
    backend_type: OmniqueueType,
    // Dynamic producers/consumers per queue name
    producers: Arc<Mutex<HashMap<String, Arc<omniqueue::DynProducer>>>>,
    consumers: Arc<Mutex<HashMap<String, Arc<Mutex<omniqueue::DynConsumer>>>>>,
}

#[cfg(not(target_os = "windows"))]
#[derive(Clone, Debug)]
enum OmniqueueType {
    Redis { dsn: String },
    RabbitMq { uri: String },
}

// ============================================================================
// Queue Service
// ============================================================================

/// Queue service configuration.
#[derive(Clone)]
pub struct QueueService {
    backend: QueueBackend,
}

impl QueueService {
    /// Create a new in-memory queue service (for testing/development).
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            backend: QueueBackend::InMemory {
                topics: Arc::new(Mutex::new(HashMap::new())),
                queues: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }

    /// Create queue service from environment configuration.
    ///
    /// Reads `QUEUE_URL` environment variable.
    /// Supported URL schemes:
    /// - `memory://` - In-memory (tokio mpsc)
    /// - `redis://host:port/queue_key` - Redis Streams
    /// - `amqp://user:pass@host:port/queue` - `RabbitMQ`
    /// - `sqs://region/queue_name` - AWS SQS
    /// - `gcp://project/topic/subscription` - GCP Pub/Sub
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is unsupported or connection fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::QueueService;
    ///
    /// // Set QUEUE_URL=redis://localhost:6379
    /// let queue = QueueService::from_env().await?;
    /// ```
    pub async fn from_env() -> Result<Self> {
        let url = std::env::var("QUEUE_URL")
            .unwrap_or_else(|_| "memory://".to_string());

        Self::from_url(&url).await
    }

    /// Create queue service from a URL.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is unsupported or connection fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::QueueService;
    ///
    /// // In-memory queue (for testing)
    /// let queue = QueueService::from_url("memory://").await?;
    ///
    /// // Redis Streams (Linux/macOS only)
    /// let queue = QueueService::from_url("redis://localhost:6379").await?;
    ///
    /// // RabbitMQ (Linux/macOS only)
    /// let queue = QueueService::from_url("amqp://guest:guest@localhost:5672").await?;
    /// ```
    #[allow(clippy::unused_async)] // Async used on non-Windows platforms
    pub async fn from_url(url: &str) -> Result<Self> {
        if url.starts_with("memory://") {
            return Ok(Self::in_memory());
        }

        #[cfg(target_os = "windows")]
        {
            Err(Error::Config(format!(
                "Backend '{}' not available on Windows. Use memory:// or deploy in Linux container.",
                url.split("://").next().unwrap_or("unknown")
            )))
        }

        #[cfg(not(target_os = "windows"))]
        {
            Self::from_omniqueue_url(url).await
        }
    }

    /// Create omniqueue-based backend from URL (Linux/macOS only).
    #[cfg(not(target_os = "windows"))]
    async fn from_omniqueue_url(url: &str) -> Result<Self> {
        let backend_type = if url.starts_with("redis://") {
            tracing::info!(url = %url, "Configuring Redis queue backend");
            OmniqueueType::Redis { dsn: url.to_string() }
        } else if url.starts_with("amqp://") {
            tracing::info!(url = %url, "Configuring RabbitMQ queue backend");
            OmniqueueType::RabbitMq { uri: url.to_string() }
        } else {
            return Err(Error::Config(format!(
                "Unknown queue backend: {url}. Supported: memory://, redis://, amqp://"
            )));
        };

        Ok(Self {
            backend: QueueBackend::Omniqueue(OmniqueueBackend {
                backend_type,
                producers: Arc::new(Mutex::new(HashMap::new())),
                consumers: Arc::new(Mutex::new(HashMap::new())),
            }),
        })
    }

    /// Get or create an in-memory topic queue.
    async fn get_or_create_inmemory_topic(&self, topics: &Arc<Mutex<HashMap<String, InMemoryQueue>>>, topic: &str) -> Result<InMemoryQueue> {
        let mut topics = topics.lock().await;

        if let Some(queue) = topics.get(topic) {
            return Ok(queue.clone());
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let queue = InMemoryQueue {
            producer: tx,
            consumer: Arc::new(Mutex::new(rx)),
        };

        topics.insert(topic.to_string(), queue.clone());
        Ok(queue)
    }

    /// Get or create an in-memory work queue.
    async fn get_or_create_inmemory_queue(&self, queues: &Arc<Mutex<HashMap<String, InMemoryQueue>>>, name: &str) -> Result<InMemoryQueue> {
        let mut queues = queues.lock().await;

        if let Some(queue) = queues.get(name) {
            return Ok(queue.clone());
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let queue = InMemoryQueue {
            producer: tx,
            consumer: Arc::new(Mutex::new(rx)),
        };

        queues.insert(name.to_string(), queue.clone());
        Ok(queue)
    }

    /// Ensure Redis consumer group exists (creates stream + group if needed).
    #[cfg(not(target_os = "windows"))]
    async fn ensure_redis_consumer_group(dsn: &str, queue_name: &str, group_name: &str) -> Result<()> {
        use redis::AsyncCommands;

        let client = redis::Client::open(dsn)
            .map_err(|e| Error::Internal(format!("Redis connection error: {e}")))?;
        let mut conn = client.get_multiplexed_async_connection().await
            .map_err(|e| Error::Internal(format!("Redis connection error: {e}")))?;

        // XGROUP CREATE <stream> <group> $ MKSTREAM
        // Creates both stream and consumer group if they don't exist
        let result: redis::RedisResult<String> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(queue_name)
            .arg(group_name)
            .arg("$")
            .arg("MKSTREAM")
            .query_async(&mut conn)
            .await;

        match result {
            Ok(_) => {
                tracing::info!(queue = %queue_name, group = %group_name, "Created Redis consumer group");
            }
            Err(e) if e.to_string().contains("BUSYGROUP") => {
                // Group already exists, that's fine
            }
            Err(e) => {
                return Err(Error::Internal(format!("Failed to create consumer group: {e}")));
            }
        }

        Ok(())
    }

    /// Ensure RabbitMQ queue exists (declares queue if needed).
    #[cfg(not(target_os = "windows"))]
    async fn ensure_rabbitmq_queue(uri: &str, queue_name: &str) -> Result<()> {
        use lapin::{Connection, ConnectionProperties, options::QueueDeclareOptions, types::FieldTable};

        let conn = Connection::connect(uri, ConnectionProperties::default())
            .await
            .map_err(|e| Error::Internal(format!("RabbitMQ connection error: {e}")))?;

        let channel = conn.create_channel()
            .await
            .map_err(|e| Error::Internal(format!("RabbitMQ channel error: {e}")))?;

        // Declare queue (creates if doesn't exist, idempotent if exists)
        channel.queue_declare(
            queue_name,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .map_err(|e| Error::Internal(format!("Failed to declare RabbitMQ queue: {e}")))?;

        tracing::info!(queue = %queue_name, "Ensured RabbitMQ queue exists");
        Ok(())
    }

    /// Get or create an omniqueue producer for a queue name.
    #[cfg(not(target_os = "windows"))]
    async fn get_or_create_producer(&self, backend: &OmniqueueBackend, queue_name: &str) -> Result<Arc<omniqueue::DynProducer>> {
        use omniqueue::backends::{RedisBackend, RabbitMqBackend};
        use omniqueue::backends::redis::RedisConfig;
        use omniqueue::backends::rabbitmq::RabbitMqConfig;

        let mut producers = backend.producers.lock().await;

        if let Some(producer) = producers.get(queue_name) {
            return Ok(Arc::clone(producer));
        }

        let producer: omniqueue::DynProducer = match &backend.backend_type {
            OmniqueueType::Redis { dsn } => {
                // Auto-create consumer group before first use
                Self::ensure_redis_consumer_group(dsn, queue_name, "mikcar").await?;
                let config = RedisConfig {
                    dsn: dsn.clone(),
                    max_connections: 8,
                    reinsert_on_nack: true,
                    queue_key: queue_name.to_string(),
                    delayed_queue_key: format!("{queue_name}:delayed"),
                    delayed_lock_key: format!("{queue_name}:delayed_lock"),
                    consumer_group: "mikcar".to_string(),
                    consumer_name: format!("mikcar-{}", uuid::Uuid::new_v4()),
                    payload_key: "payload".to_string(),
                    ack_deadline_ms: 30_000,
                    dlq_config: None,
                    sentinel_config: None,
                };
                RedisBackend::builder(config)
                    .make_dynamic()
                    .build_producer()
                    .await
                    .map_err(|e| Error::Internal(format!("Failed to create Redis producer: {e}")))?
            }
            OmniqueueType::RabbitMq { uri } => {
                // Auto-create queue before first use
                Self::ensure_rabbitmq_queue(uri, queue_name).await?;
                let config = RabbitMqConfig {
                    uri: uri.clone(),
                    connection_properties: Default::default(),
                    publish_exchange: "".to_string(),
                    publish_routing_key: queue_name.to_string(),
                    publish_options: Default::default(),
                    publish_properties: Default::default(),
                    consume_queue: queue_name.to_string(),
                    consumer_tag: format!("mikcar-{}", uuid::Uuid::new_v4()),
                    consume_options: Default::default(),
                    consume_arguments: Default::default(),
                    consume_prefetch_count: Some(10),
                    requeue_on_nack: true,
                };
                RabbitMqBackend::builder(config)
                    .make_dynamic()
                    .build_producer()
                    .await
                    .map_err(|e| Error::Internal(format!("Failed to create RabbitMQ producer: {e}")))?
            }
        };

        let producer = Arc::new(producer);
        producers.insert(queue_name.to_string(), Arc::clone(&producer));
        Ok(producer)
    }

    /// Get or create an omniqueue consumer for a queue name.
    #[cfg(not(target_os = "windows"))]
    async fn get_or_create_consumer(&self, backend: &OmniqueueBackend, queue_name: &str) -> Result<Arc<Mutex<omniqueue::DynConsumer>>> {
        use omniqueue::backends::{RedisBackend, RabbitMqBackend};
        use omniqueue::backends::redis::RedisConfig;
        use omniqueue::backends::rabbitmq::RabbitMqConfig;

        let mut consumers = backend.consumers.lock().await;

        if let Some(consumer) = consumers.get(queue_name) {
            return Ok(Arc::clone(consumer));
        }

        let consumer: omniqueue::DynConsumer = match &backend.backend_type {
            OmniqueueType::Redis { dsn } => {
                let config = RedisConfig {
                    dsn: dsn.clone(),
                    max_connections: 8,
                    reinsert_on_nack: true,
                    queue_key: queue_name.to_string(),
                    delayed_queue_key: format!("{queue_name}:delayed"),
                    delayed_lock_key: format!("{queue_name}:delayed_lock"),
                    consumer_group: "mikcar".to_string(),
                    consumer_name: format!("mikcar-{}", uuid::Uuid::new_v4()),
                    payload_key: "payload".to_string(),
                    ack_deadline_ms: 30_000,
                    dlq_config: None,
                    sentinel_config: None,
                };
                RedisBackend::builder(config)
                    .make_dynamic()
                    .build_consumer()
                    .await
                    .map_err(|e| Error::Internal(format!("Failed to create Redis consumer: {e}")))?
            }
            OmniqueueType::RabbitMq { uri } => {
                let config = RabbitMqConfig {
                    uri: uri.clone(),
                    connection_properties: Default::default(),
                    publish_exchange: "".to_string(),
                    publish_routing_key: queue_name.to_string(),
                    publish_options: Default::default(),
                    publish_properties: Default::default(),
                    consume_queue: queue_name.to_string(),
                    consumer_tag: format!("mikcar-{}", uuid::Uuid::new_v4()),
                    consume_options: Default::default(),
                    consume_arguments: Default::default(),
                    consume_prefetch_count: Some(10),
                    requeue_on_nack: true,
                };
                RabbitMqBackend::builder(config)
                    .make_dynamic()
                    .build_consumer()
                    .await
                    .map_err(|e| Error::Internal(format!("Failed to create RabbitMQ consumer: {e}")))?
            }
        };

        let consumer = Arc::new(Mutex::new(consumer));
        consumers.insert(queue_name.to_string(), Arc::clone(&consumer));
        Ok(consumer)
    }
}

impl Sidecar for QueueService {
    fn name(&self) -> &'static str {
        "queue"
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/publish/{topic}", post(publish_message))
            .route("/subscribe/{topic}", get(subscribe_messages))
            .route("/ack/{topic}/{id}", post(ack_message))
            .route("/push/{queue}", post(push_to_queue))
            .route("/pop/{queue}", get(pop_from_queue))
            .route("/len/{queue}", get(queue_length))
            .with_state(Arc::new(self.clone()))
    }

    fn health_check(&self) -> bool {
        match &self.backend {
            QueueBackend::InMemory { .. } => {
                // In-memory backend is always healthy
                true
            }
            #[cfg(not(target_os = "windows"))]
            QueueBackend::Omniqueue(omni) => {
                // For omniqueue backends, try to verify the connection
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        let backend_type = omni.backend_type.clone();
                        tokio::task::block_in_place(|| {
                            handle.block_on(async {
                                match backend_type {
                                    OmniqueueType::Redis { ref dsn } => {
                                        // Try a simple Redis PING
                                        match redis::Client::open(dsn.as_str()) {
                                            Ok(client) => {
                                                match client.get_multiplexed_async_connection().await {
                                                    Ok(mut conn) => {
                                                        match redis::cmd("PING")
                                                            .query_async::<String>(&mut conn)
                                                            .await
                                                        {
                                                            Ok(_) => true,
                                                            Err(e) => {
                                                                tracing::warn!(error = %e, "Queue Redis health check failed");
                                                                false
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(error = %e, "Queue Redis connection failed");
                                                        false
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(error = %e, "Queue Redis client creation failed");
                                                false
                                            }
                                        }
                                    }
                                    OmniqueueType::RabbitMq { ref uri } => {
                                        // For RabbitMQ, try to connect
                                        match lapin::Connection::connect(uri, lapin::ConnectionProperties::default()).await {
                                            Ok(_conn) => true,
                                            Err(e) => {
                                                tracing::warn!(error = %e, "Queue RabbitMQ health check failed");
                                                false
                                            }
                                        }
                                    }
                                }
                            })
                        })
                    }
                    Err(_) => {
                        // Not in a tokio context, assume healthy
                        true
                    }
                }
            }
        }
    }
}

// ============================================================================
// HTTP Handlers
// ============================================================================

/// Message response.
#[derive(Serialize)]
struct MessageResponse {
    id: String,
    payload: serde_json::Value,
    received_at: String,
}

/// Subscribe/pop query parameters.
#[derive(Deserialize)]
struct SubscribeQuery {
    #[serde(default = "default_timeout")]
    timeout: u32,
    #[serde(default = "default_max_messages")]
    max_messages: u32,
}

fn default_timeout() -> u32 {
    30
}

fn default_max_messages() -> u32 {
    10
}

/// Publish a message to a topic (in-memory pub/sub only).
async fn publish_message(
    State(service): State<Arc<QueueService>>,
    Path(topic): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse> {
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| serde_json::json!({
            "data": String::from_utf8_lossy(&body).to_string()
        }));

    match &service.backend {
        QueueBackend::InMemory { topics, .. } => {
            let queue = service.get_or_create_inmemory_topic(topics, &topic).await?;
            queue
                .producer
                .send(payload)
                .map_err(|e| Error::Internal(format!("Failed to publish message: {e}")))?;
        }
        #[cfg(not(target_os = "windows"))]
        QueueBackend::Omniqueue(_) => {
            return Err(Error::Config(
                "Pub/sub not supported with omniqueue backends. Use /push/{queue} for work queues, or use native pub/sub clients.".to_string()
            ));
        }
    }

    let message_id = uuid::Uuid::new_v4().to_string();

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "published",
            "topic": topic,
            "message_id": message_id
        })),
    ))
}

/// Subscribe to messages from a topic (in-memory pub/sub only).
async fn subscribe_messages(
    State(service): State<Arc<QueueService>>,
    Path(topic): Path<String>,
    Query(query): Query<SubscribeQuery>,
) -> Result<impl IntoResponse> {
    match &service.backend {
        QueueBackend::InMemory { topics, .. } => {
            let queue = service.get_or_create_inmemory_topic(topics, &topic).await?;
            let timeout = Duration::from_secs(u64::from(query.timeout));
            let max_messages = query.max_messages;

            let mut messages = Vec::new();
            let mut consumer = queue.consumer.lock().await;

            let deadline = tokio::time::Instant::now() + timeout;

            while messages.len() < max_messages as usize {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }

                match tokio::time::timeout(remaining, consumer.recv()).await {
                    Ok(Some(payload)) => {
                        messages.push(MessageResponse {
                            id: uuid::Uuid::new_v4().to_string(),
                            payload,
                            received_at: chrono::Utc::now().to_rfc3339(),
                        });
                    }
                    Ok(None) | Err(_) => break,
                }
            }

            Ok(Json(serde_json::json!({
                "messages": messages,
                "topic": topic,
                "count": messages.len()
            })))
        }
        #[cfg(not(target_os = "windows"))]
        QueueBackend::Omniqueue(_) => {
            Err(Error::Config(
                "Pub/sub not supported with omniqueue backends. Use /pop/{queue} for work queues, or use native pub/sub clients.".to_string()
            ))
        }
    }
}

/// Acknowledge a message (in-memory: no-op, omniqueue: handled at pop).
async fn ack_message(
    State(_service): State<Arc<QueueService>>,
    Path((topic, id)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
    Ok(Json(serde_json::json!({
        "status": "acknowledged",
        "topic": topic,
        "message_id": id
    })))
}

/// Push a message to a work queue.
async fn push_to_queue(
    State(service): State<Arc<QueueService>>,
    Path(queue_name): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse> {
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| serde_json::json!({
            "data": String::from_utf8_lossy(&body).to_string()
        }));

    let message_id = uuid::Uuid::new_v4().to_string();

    match &service.backend {
        QueueBackend::InMemory { queues, .. } => {
            let queue = service.get_or_create_inmemory_queue(queues, &queue_name).await?;
            queue
                .producer
                .send(payload.clone())
                .map_err(|e| Error::Internal(format!("Failed to push message: {e}")))?;
        }
        #[cfg(not(target_os = "windows"))]
        QueueBackend::Omniqueue(backend) => {
            let producer = service.get_or_create_producer(backend, &queue_name).await?;
            let payload_bytes = serde_json::to_vec(&payload)
                .map_err(|e| Error::Internal(format!("Failed to serialize payload: {e}")))?;
            producer
                .send_raw(&payload_bytes)
                .await
                .map_err(|e| Error::Internal(format!("Failed to push message: {e}")))?;
        }
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "pushed",
            "queue": queue_name,
            "message_id": message_id
        })),
    ))
}

/// Pop a message from a work queue.
async fn pop_from_queue(
    State(service): State<Arc<QueueService>>,
    Path(queue_name): Path<String>,
    Query(query): Query<SubscribeQuery>,
) -> Result<impl IntoResponse> {
    let timeout = Duration::from_secs(u64::from(query.timeout));

    match &service.backend {
        QueueBackend::InMemory { queues, .. } => {
            let queue = service.get_or_create_inmemory_queue(queues, &queue_name).await?;
            let mut consumer = queue.consumer.lock().await;

            match tokio::time::timeout(timeout, consumer.recv()).await {
                Ok(Some(payload)) => {
                    let message = MessageResponse {
                        id: uuid::Uuid::new_v4().to_string(),
                        payload,
                        received_at: chrono::Utc::now().to_rfc3339(),
                    };

                    Ok(Json(serde_json::json!({
                        "message": message,
                        "queue": queue_name
                    })))
                }
                Ok(None) => Err(Error::Internal("Queue channel closed".to_string())),
                Err(_) => Ok(Json(serde_json::json!({
                    "message": null,
                    "queue": queue_name
                }))),
            }
        }
        #[cfg(not(target_os = "windows"))]
        QueueBackend::Omniqueue(backend) => {
            let consumer = service.get_or_create_consumer(backend, &queue_name).await?;
            let mut consumer = consumer.lock().await;

            match tokio::time::timeout(timeout, consumer.receive()).await {
                Ok(Ok(delivery)) => {
                    let payload: serde_json::Value = delivery
                        .payload_serde_json()
                        .unwrap_or_else(|_| {
                            // Fallback if deserialization fails
                            None
                        })
                        .unwrap_or_else(|| {
                            // Fallback for None/missing payload
                            serde_json::json!({
                                "data": String::from_utf8_lossy(delivery.borrow_payload().unwrap_or(&[])).to_string()
                            })
                        });

                    // Auto-ack the message
                    if let Err(e) = delivery.ack().await {
                        tracing::warn!(error = %e.0, "Failed to ack message");
                    }

                    let message = MessageResponse {
                        id: uuid::Uuid::new_v4().to_string(),
                        payload,
                        received_at: chrono::Utc::now().to_rfc3339(),
                    };

                    Ok(Json(serde_json::json!({
                        "message": message,
                        "queue": queue_name
                    })))
                }
                Ok(Err(e)) => Err(Error::Internal(format!("Failed to receive message: {e}"))),
                Err(_) => Ok(Json(serde_json::json!({
                    "message": null,
                    "queue": queue_name
                }))),
            }
        }
    }
}

/// Get the length of a queue.
async fn queue_length(
    State(_service): State<Arc<QueueService>>,
    Path(queue): Path<String>,
) -> Result<impl IntoResponse> {
    // Queue length is not directly supported by omniqueue
    // Would need backend-specific queries
    Ok(Json(serde_json::json!({
        "queue": queue,
        "length": null,
        "note": "Queue length requires backend-specific queries"
    })))
}
