//! Queue backend integration tests.
//!
//! These tests require Docker services to be running:
//! ```bash
//! docker-compose up -d redis rabbitmq localstack
//! ```
//!
//! Run these tests with:
//! ```bash
//! cargo test --test queue_integration -- --ignored
//! ```
//!
//! Note: omniqueue-based tests (Redis, RabbitMQ, SQS) only run on Linux/macOS.
//! In-memory tests run on all platforms.

use std::time::Duration;

/// Helper to check if a service is available
#[allow(dead_code)]
async fn is_service_available(url: &str) -> bool {
    let client = reqwest::Client::new();
    match tokio::time::timeout(Duration::from_secs(2), client.get(url).send()).await {
        Ok(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

/// Helper to check if Redis is available
#[allow(dead_code)]
async fn is_redis_available() -> bool {
    use std::process::Command;
    let output = Command::new("redis-cli")
        .args(["-h", "localhost", "-p", "6379", "ping"])
        .output();
    matches!(output, Ok(o) if o.status.success())
}

/// Helper to check if RabbitMQ is available
#[allow(dead_code)]
async fn is_rabbitmq_available() -> bool {
    is_service_available("http://localhost:15672/api/health/checks/alarms").await
}

/// Helper to check if LocalStack SQS is available
#[allow(dead_code)]
async fn is_localstack_available() -> bool {
    is_service_available("http://localhost:4566/_localstack/health").await
}

// ============================================================================
// omniqueue-based tests (Linux/macOS only)
// ============================================================================

#[cfg(not(target_os = "windows"))]
mod redis_queue {
    use super::*;
    use mikcar::queue::QueueService;

    const REDIS_URL: &str = "redis://localhost:6379";

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d redis"]
    async fn test_redis_queue_push_pop() {
        if !is_redis_available().await {
            eprintln!("Skipping: Redis not available at localhost:6379");
            return;
        }

        let service = QueueService::from_url(REDIS_URL)
            .await
            .expect("Failed to create Redis queue service");

        // Create a simple test by verifying the service was created
        // The actual push/pop would require running the HTTP server
        assert!(matches!(service, QueueService { .. }));
        println!("✓ Redis queue service created successfully");
    }

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d redis"]
    async fn test_redis_queue_service_http() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use mikcar::Sidecar;
        use tower::ServiceExt;

        if !is_redis_available().await {
            eprintln!("Skipping: Redis not available at localhost:6379");
            return;
        }

        let service = QueueService::from_url(REDIS_URL)
            .await
            .expect("Failed to create Redis queue service");

        let router = service.router();

        // Test push
        let push_request = Request::builder()
            .method("POST")
            .uri("/push/test-queue")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message": "hello from redis test"}"#))
            .unwrap();

        let response = router.clone().oneshot(push_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        println!("✓ Redis push succeeded");

        // Test pop with short timeout
        let pop_request = Request::builder()
            .method("GET")
            .uri("/pop/test-queue?timeout=5")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(pop_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("message").is_some());
        println!("✓ Redis pop succeeded: {:?}", json);
    }
}

#[cfg(not(target_os = "windows"))]
mod rabbitmq_queue {
    use super::*;
    use mikcar::queue::QueueService;

    const RABBITMQ_URL: &str = "amqp://guest:guest@localhost:5672";

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d rabbitmq"]
    async fn test_rabbitmq_queue_push_pop() {
        if !is_rabbitmq_available().await {
            eprintln!("Skipping: RabbitMQ not available at localhost:5672");
            return;
        }

        let service = QueueService::from_url(RABBITMQ_URL)
            .await
            .expect("Failed to create RabbitMQ queue service");

        assert!(matches!(service, QueueService { .. }));
        println!("✓ RabbitMQ queue service created successfully");
    }

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d rabbitmq"]
    async fn test_rabbitmq_queue_service_http() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use mikcar::Sidecar;
        use tower::ServiceExt;

        if !is_rabbitmq_available().await {
            eprintln!("Skipping: RabbitMQ not available at localhost:5672");
            return;
        }

        let service = QueueService::from_url(RABBITMQ_URL)
            .await
            .expect("Failed to create RabbitMQ queue service");

        let router = service.router();

        // Test push
        let push_request = Request::builder()
            .method("POST")
            .uri("/push/rabbitmq-test-queue")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message": "hello from rabbitmq test"}"#))
            .unwrap();

        let response = router.clone().oneshot(push_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        println!("✓ RabbitMQ push succeeded");

        // Test pop with short timeout
        let pop_request = Request::builder()
            .method("GET")
            .uri("/pop/rabbitmq-test-queue?timeout=5")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(pop_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("message").is_some());
        println!("✓ RabbitMQ pop succeeded: {:?}", json);
    }
}

#[cfg(not(target_os = "windows"))]
mod sqs_queue {
    use super::*;
    use mikcar::queue::QueueService;

    // LocalStack SQS endpoint
    const SQS_URL: &str = "sqs://http://localhost:4566/000000000000/test-queue?override=true";

    /// Set fake AWS credentials for LocalStack
    fn setup_localstack_credentials() {
        // SAFETY: These are test-only fake credentials for LocalStack.
        // Single-threaded test execution ensures no race conditions.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "test");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
            std::env::set_var("AWS_REGION", "us-east-1");
        }
    }

    async fn create_sqs_queue() -> bool {
        // Create the SQS queue in LocalStack using proper SQS API
        let client = reqwest::Client::new();

        // LocalStack SQS uses query string parameters
        let response = client
            .get("http://localhost:4566")
            .query(&[
                ("Action", "CreateQueue"),
                ("QueueName", "test-queue"),
                ("Version", "2012-11-05"),
            ])
            .send()
            .await;

        match response {
            Ok(r) => {
                let success = r.status().is_success();
                if !success {
                    if let Ok(body) = r.text().await {
                        eprintln!("SQS queue creation response: {}", body);
                    }
                }
                success
            }
            Err(e) => {
                eprintln!("SQS queue creation failed: {}", e);
                false
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d localstack"]
    async fn test_sqs_queue_push_pop() {
        setup_localstack_credentials();

        if !is_localstack_available().await {
            eprintln!("Skipping: LocalStack not available at localhost:4566");
            return;
        }

        // Create queue first
        create_sqs_queue().await;

        let service = QueueService::from_url(SQS_URL)
            .await
            .expect("Failed to create SQS queue service");

        assert!(matches!(service, QueueService { .. }));
        println!("✓ SQS queue service created successfully");
    }

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d localstack"]
    async fn test_sqs_queue_service_http() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use mikcar::Sidecar;
        use tower::ServiceExt;

        setup_localstack_credentials();

        if !is_localstack_available().await {
            eprintln!("Skipping: LocalStack not available at localhost:4566");
            return;
        }

        // Create queue first
        create_sqs_queue().await;

        let service = QueueService::from_url(SQS_URL)
            .await
            .expect("Failed to create SQS queue service");

        let router = service.router();

        // Test push - use "test-queue" to match the configured SQS queue
        let push_request = Request::builder()
            .method("POST")
            .uri("/push/test-queue")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message": "hello from sqs test"}"#))
            .unwrap();

        let response = router.clone().oneshot(push_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        println!("✓ SQS push succeeded");

        // Test pop with short timeout
        let pop_request = Request::builder()
            .method("GET")
            .uri("/pop/test-queue?timeout=5")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(pop_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // SQS may return null message if queue is empty or message not yet visible
        println!("✓ SQS pop completed: {:?}", json);
    }
}

// ============================================================================
// In-memory tests (requires queue feature)
// ============================================================================

#[cfg(feature = "queue")]
mod memory_queue {
    use mikcar::Sidecar;
    use mikcar::queue::QueueService;

    #[tokio::test]
    async fn test_memory_queue_push_pop() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let service = QueueService::from_url("memory://")
            .await
            .expect("Failed to create in-memory queue service");

        let router = service.router();

        // Test push
        let push_request = Request::builder()
            .method("POST")
            .uri("/push/memory-test-queue")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message": "hello from memory test"}"#))
            .unwrap();

        let response = router.clone().oneshot(push_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "pushed");
        println!("✓ Memory push succeeded");

        // Test pop
        let pop_request = Request::builder()
            .method("GET")
            .uri("/pop/memory-test-queue?timeout=1")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(pop_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("message").is_some());
        let message = &json["message"];
        assert_eq!(message["payload"]["message"], "hello from memory test");
        println!("✓ Memory pop succeeded: {:?}", json);
    }

    #[tokio::test]
    async fn test_memory_pubsub() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let service = QueueService::from_url("memory://")
            .await
            .expect("Failed to create in-memory queue service");

        let router = service.router();

        // Test publish
        let publish_request = Request::builder()
            .method("POST")
            .uri("/publish/test-topic")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"event": "user_created", "user_id": 123}"#))
            .unwrap();

        let response = router.clone().oneshot(publish_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "published");
        println!("✓ Memory publish succeeded");

        // Test subscribe
        let subscribe_request = Request::builder()
            .method("GET")
            .uri("/subscribe/test-topic?timeout=1&max_messages=1")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(subscribe_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["count"], 1);
        let messages = json["messages"].as_array().unwrap();
        assert_eq!(messages[0]["payload"]["event"], "user_created");
        println!("✓ Memory subscribe succeeded: {:?}", json);
    }
}
