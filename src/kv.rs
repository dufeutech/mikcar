//! Key-Value store service.
//!
//! Provides an embedded key-value store using redb (pure Rust, no dependencies).
//!
//! ## URL Schemes
//!
//! ```text
//! redb:///data/kv.redb           # Embedded database at path
//! redb://./kv.redb               # Relative path
//! memory://                      # In-memory (testing only)
//! ```

use crate::{Error, HealthCheck, HealthResult, Result, Sidecar};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ============================================================================
// Backend Trait
// ============================================================================

/// Boxed future type for trait methods.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Backend trait for KV operations.
pub trait KvBackend: Send + Sync + 'static {
    /// Backend name for health checks.
    fn name(&self) -> &'static str;

    /// Get a value by key.
    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<String>>>;

    /// Set a value with optional TTL.
    fn set(&self, key: &str, value: &str, ttl: Option<i64>) -> BoxFuture<'_, Result<()>>;

    /// Delete a key.
    fn del(&self, key: &str) -> BoxFuture<'_, Result<i64>>;

    /// List keys matching a pattern.
    fn keys(&self, pattern: &str) -> BoxFuture<'_, Result<Vec<String>>>;

    /// Increment a key atomically.
    fn incr(&self, key: &str) -> BoxFuture<'_, Result<i64>>;

    /// Get multiple keys at once.
    fn mget(&self, keys: &[String]) -> BoxFuture<'_, Result<Vec<Option<String>>>>;

    /// Set multiple keys at once.
    fn mset(&self, pairs: &[(String, String)]) -> BoxFuture<'_, Result<()>>;

    /// Check if a key exists.
    fn exists(&self, key: &str) -> BoxFuture<'_, Result<bool>>;

    /// Get TTL of a key (-1 if no TTL, -2 if key doesn't exist).
    fn ttl(&self, key: &str) -> BoxFuture<'_, Result<i64>>;

    /// Set expiration on a key.
    fn expire(&self, key: &str, seconds: i64) -> BoxFuture<'_, Result<bool>>;

    /// Health check.
    fn health(&self) -> BoxFuture<'_, HealthResult>;
}

// ============================================================================
// KV Service
// ============================================================================

/// KV service wrapping a backend.
pub struct KvService {
    backend: Arc<dyn KvBackend>,
}

impl std::fmt::Debug for KvService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvService")
            .field("backend", &"<KvBackend>")
            .finish()
    }
}

impl KvService {
    /// Create a new KV service with a specific backend.
    pub fn new<B: KvBackend>(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Create KV service from a URL.
    ///
    /// Supported schemes:
    /// - `redb://path` - Embedded redb database
    /// - `memory://` - In-memory (testing)
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is unknown or the database cannot be opened.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::KvService;
    ///
    /// // In-memory store for testing
    /// let kv = KvService::from_url("memory://")?;
    ///
    /// // Embedded redb database
    /// let kv = KvService::from_url("redb://./data/kv.redb")?;
    /// ```
    pub fn from_url(url: &str) -> Result<Self> {
        if url.starts_with("memory://") {
            Ok(Self::new(MemoryBackend::new()))
        } else if url.starts_with("redb://") {
            let path = url.strip_prefix("redb://").unwrap();
            Ok(Self::new(RedbBackend::new(path)?))
        } else {
            Err(Error::Config(format!(
                "Unknown KV URL scheme: {url}. Use redb:// or memory://"
            )))
        }
    }

    /// Create KV service from environment configuration.
    ///
    /// Reads `KV_URL` environment variable.
    ///
    /// # Errors
    ///
    /// Returns an error if `KV_URL` is not set or contains an invalid URL.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::KvService;
    ///
    /// // Set KV_URL=redb://./data/kv.redb
    /// let kv = KvService::from_env()?;
    /// ```
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("KV_URL").map_err(|_| {
            Error::Config("KV_URL environment variable not set".to_string())
        })?;

        Self::from_url(&url)
    }

    /// Create a health check.
    #[must_use]
    pub fn health_checker(&self) -> HealthCheck {
        let backend = self.backend.clone();
        HealthCheck::with_async("kv", move || {
            let backend = backend.clone();
            async move { backend.health().await }
        })
    }
}

impl Sidecar for KvService {
    fn name(&self) -> &'static str {
        "kv"
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/get/{key}", get(get_key))
            .route("/get/batch", post(mget_keys))
            .route("/set/{key}", post(set_key))
            .route("/set/batch", post(mset_keys))
            .route("/del/{key}", delete(del_key))
            .route("/keys/{pattern}", get(list_keys))
            .route("/increment/{key}", post(incr_key))
            .route("/exists/{key}", get(exists_key))
            .route("/ttl/{key}", get(get_ttl))
            .route("/expire/{key}", post(set_expire))
            .with_state(self.backend.clone())
    }

    fn health_check(&self) -> bool {
        // Use block_in_place to run the async health check synchronously.
        // This avoids blocking the async runtime thread pool.
        let backend = self.backend.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // We're in a tokio context, use block_in_place
                tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        backend.health().await.healthy
                    })
                })
            }
            Err(_) => {
                // Not in a tokio context, assume healthy (sync fallback)
                true
            }
        }
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// Query parameters for set operation.
#[derive(Deserialize)]
struct SetQuery {
    #[serde(default)]
    ttl: Option<i64>,
}

async fn get_key(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse> {
    match backend.get(&key).await? {
        Some(v) => Ok((StatusCode::OK, v)),
        None => Err(Error::NotFound(key)),
    }
}

async fn set_key(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
    Query(query): Query<SetQuery>,
    body: Bytes,
) -> Result<impl IntoResponse> {
    let value = String::from_utf8_lossy(&body).to_string();
    backend.set(&key, &value, query.ttl).await?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "key": key
        })),
    ))
}

async fn del_key(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse> {
    let deleted = backend.del(&key).await?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "deleted": deleted
    })))
}

async fn list_keys(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(pattern): Path<String>,
) -> Result<impl IntoResponse> {
    let keys = backend.keys(&pattern).await?;

    Ok(Json(serde_json::json!({
        "keys": keys
    })))
}

async fn incr_key(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse> {
    let value = backend.incr(&key).await?;

    Ok(Json(serde_json::json!({
        "value": value
    })))
}

#[derive(Deserialize)]
struct MgetRequest {
    keys: Vec<String>,
}

async fn mget_keys(
    State(backend): State<Arc<dyn KvBackend>>,
    Json(body): Json<MgetRequest>,
) -> Result<impl IntoResponse> {
    let values = backend.mget(&body.keys).await?;
    let result: HashMap<String, Option<String>> = body.keys.into_iter().zip(values).collect();

    Ok(Json(result))
}

async fn mset_keys(
    State(backend): State<Arc<dyn KvBackend>>,
    Json(body): Json<HashMap<String, String>>,
) -> Result<impl IntoResponse> {
    let pairs: Vec<(String, String)> = body.into_iter().collect();
    backend.mset(&pairs).await?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "count": pairs.len()
    })))
}

async fn exists_key(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse> {
    let exists = backend.exists(&key).await?;

    Ok(Json(serde_json::json!({
        "exists": exists
    })))
}

async fn get_ttl(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse> {
    let ttl = backend.ttl(&key).await?;

    Ok(Json(serde_json::json!({
        "ttl": ttl
    })))
}

#[derive(Deserialize)]
struct ExpireRequest {
    seconds: i64,
}

async fn set_expire(
    State(backend): State<Arc<dyn KvBackend>>,
    Path(key): Path<String>,
    Json(body): Json<ExpireRequest>,
) -> Result<impl IntoResponse> {
    let result = backend.expire(&key, body.seconds).await?;

    Ok(Json(serde_json::json!({
        "status": if result { "ok" } else { "key_not_found" }
    })))
}

// ============================================================================
// Memory Backend (for testing)
// ============================================================================

use std::sync::RwLock;
use std::time::{Duration, Instant};

struct MemoryEntry {
    value: String,
    expires_at: Option<Instant>,
}

/// In-memory KV backend for testing.
pub struct MemoryBackend {
    data: RwLock<HashMap<String, MemoryEntry>>,
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBackend")
            .field("data", &"<RwLock<HashMap>>")
            .finish()
    }
}

impl MemoryBackend {
    /// Create a new in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }

    fn is_expired(entry: &MemoryEntry) -> bool {
        entry
            .expires_at
            .is_some_and(|expires| Instant::now() >= expires)
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl KvBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<String>>> {
        let key = key.to_string();
        Box::pin(async move {
            let data = self.data.read().unwrap();
            match data.get(&key) {
                Some(entry) if !Self::is_expired(entry) => Ok(Some(entry.value.clone())),
                _ => Ok(None),
            }
        })
    }

    fn set(&self, key: &str, value: &str, ttl: Option<i64>) -> BoxFuture<'_, Result<()>> {
        let key = key.to_string();
        let value = value.to_string();
        Box::pin(async move {
            #[allow(clippy::cast_sign_loss)]
            let expires_at = ttl
                .filter(|&t| t > 0)
                .map(|t| Instant::now() + Duration::from_secs(t as u64));

            let mut data = self.data.write().unwrap();
            data.insert(key, MemoryEntry { value, expires_at });
            Ok(())
        })
    }

    fn del(&self, key: &str) -> BoxFuture<'_, Result<i64>> {
        let key = key.to_string();
        Box::pin(async move {
            let mut data = self.data.write().unwrap();
            Ok(i64::from(data.remove(&key).is_some()))
        })
    }

    fn keys(&self, pattern: &str) -> BoxFuture<'_, Result<Vec<String>>> {
        let pattern = pattern.replace('*', ".*").replace('?', ".");
        Box::pin(async move {
            let data = self.data.read().unwrap();
            Ok(data
                .iter()
                .filter(|(k, v)| {
                    !Self::is_expired(v)
                        && (pattern == ".*" || k.contains(&pattern.replace(".*", "")))
                })
                .map(|(k, _)| k.clone())
                .collect())
        })
    }

    fn incr(&self, key: &str) -> BoxFuture<'_, Result<i64>> {
        let key = key.to_string();
        Box::pin(async move {
            let mut data = self.data.write().unwrap();
            let current = data
                .get(&key)
                .filter(|e| !Self::is_expired(e))
                .and_then(|e| e.value.parse::<i64>().ok())
                .unwrap_or(0);

            let new_value = current + 1;
            data.insert(
                key,
                MemoryEntry {
                    value: new_value.to_string(),
                    expires_at: None,
                },
            );
            Ok(new_value)
        })
    }

    fn mget(&self, keys: &[String]) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let keys = keys.to_vec();
        Box::pin(async move {
            let data = self.data.read().unwrap();
            Ok(keys
                .iter()
                .map(|k| {
                    data.get(k)
                        .filter(|e| !Self::is_expired(e))
                        .map(|e| e.value.clone())
                })
                .collect())
        })
    }

    fn mset(&self, pairs: &[(String, String)]) -> BoxFuture<'_, Result<()>> {
        let pairs = pairs.to_vec();
        Box::pin(async move {
            let mut data = self.data.write().unwrap();
            for (k, v) in pairs {
                data.insert(
                    k,
                    MemoryEntry {
                        value: v,
                        expires_at: None,
                    },
                );
            }
            Ok(())
        })
    }

    fn exists(&self, key: &str) -> BoxFuture<'_, Result<bool>> {
        let key = key.to_string();
        Box::pin(async move {
            let data = self.data.read().unwrap();
            Ok(data.get(&key).is_some_and(|e| !Self::is_expired(e)))
        })
    }

    fn ttl(&self, key: &str) -> BoxFuture<'_, Result<i64>> {
        let key = key.to_string();
        Box::pin(async move {
            let data = self.data.read().unwrap();
            match data.get(&key) {
                Some(entry) if !Self::is_expired(entry) => match entry.expires_at {
                    Some(expires) => {
                        let remaining = expires.saturating_duration_since(Instant::now());
                        #[allow(clippy::cast_possible_wrap)]
                        Ok(remaining.as_secs() as i64)
                    }
                    None => Ok(-1), // No TTL
                },
                _ => Ok(-2), // Key doesn't exist
            }
        })
    }

    fn expire(&self, key: &str, seconds: i64) -> BoxFuture<'_, Result<bool>> {
        let key = key.to_string();
        Box::pin(async move {
            let mut data = self.data.write().unwrap();
            if let Some(entry) = data.get_mut(&key) {
                if !Self::is_expired(entry) {
                    #[allow(clippy::cast_sign_loss)]
                    let expires_at = Instant::now() + Duration::from_secs(seconds as u64);
                    entry.expires_at = Some(expires_at);
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    fn health(&self) -> BoxFuture<'_, HealthResult> {
        Box::pin(async move { HealthResult::healthy() })
    }
}

// ============================================================================
// Redb Backend (embedded, pure Rust)
// ============================================================================

const KV_TABLE: TableDefinition<'_, &str, &str> = TableDefinition::new("kv");
const TTL_TABLE: TableDefinition<'_, &str, u64> = TableDefinition::new("ttl");

/// Embedded redb backend (pure Rust, no external dependencies).
/// Uses `spawn_blocking` for disk I/O to avoid blocking the async executor.
pub struct RedbBackend {
    db: Arc<Database>,
}

impl std::fmt::Debug for RedbBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbBackend")
            .field("db", &"<Database>")
            .finish()
    }
}

impl RedbBackend {
    /// Create a new redb backend at the given path.
    pub fn new<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        // Create parent directories if they don't exist
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::Config(format!("Failed to create database directory: {e}"))
                })?;
            }
        }

        let db = Database::create(path)
            .map_err(|e| Error::Config(format!("Failed to create redb database: {e}")))?;

        // Initialize tables
        let write_txn = db
            .begin_write()
            .map_err(|e| Error::Internal(format!("Failed to begin transaction: {e}")))?;
        {
            let _ = write_txn.open_table(KV_TABLE);
            let _ = write_txn.open_table(TTL_TABLE);
        }
        write_txn
            .commit()
            .map_err(|e| Error::Internal(format!("Failed to commit: {e}")))?;

        Ok(Self { db: Arc::new(db) })
    }

}

// Free functions for spawn_blocking (can't capture &self)
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn is_key_expired(db: &Database, key: &str) -> bool {
    let Ok(read_txn) = db.begin_read() else {
        return false;
    };
    let Ok(ttl_table) = read_txn.open_table(TTL_TABLE) else {
        return false;
    };
    if let Ok(Some(expires)) = ttl_table.get(key) {
        return current_timestamp() >= expires.value();
    }
    false
}

fn get_sync(db: &Database, key: &str) -> Result<Option<String>> {
    if is_key_expired(db, key) {
        return Ok(None);
    }

    let read_txn = db
        .begin_read()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let table = read_txn
        .open_table(KV_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;

    match table.get(key) {
        Ok(Some(value)) => Ok(Some(value.value().to_string())),
        Ok(None) => Ok(None),
        Err(e) => Err(Error::Internal(e.to_string())),
    }
}

fn set_sync(db: &Database, key: &str, value: &str, ttl: Option<i64>) -> Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| Error::Internal(e.to_string()))?;
    {
        let mut table = write_txn
            .open_table(KV_TABLE)
            .map_err(|e| Error::Internal(e.to_string()))?;
        table
            .insert(key, value)
            .map_err(|e| Error::Internal(e.to_string()))?;

        if let Some(ttl) = ttl.filter(|&t| t > 0) {
            let mut ttl_table = write_txn
                .open_table(TTL_TABLE)
                .map_err(|e| Error::Internal(e.to_string()))?;
            #[allow(clippy::cast_sign_loss)]
            let expires_at = current_timestamp() + ttl as u64;
            ttl_table
                .insert(key, expires_at)
                .map_err(|e| Error::Internal(e.to_string()))?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(())
}

fn del_sync(db: &Database, key: &str) -> Result<i64> {
    let write_txn = db
        .begin_write()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let deleted = {
        let mut table = write_txn
            .open_table(KV_TABLE)
            .map_err(|e| Error::Internal(e.to_string()))?;
        let existed = table
            .remove(key)
            .map_err(|e| Error::Internal(e.to_string()))?
            .is_some();

        if let Ok(mut ttl_table) = write_txn.open_table(TTL_TABLE) {
            let _ = ttl_table.remove(key);
        }

        i64::from(existed)
    };
    write_txn
        .commit()
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(deleted)
}

fn keys_sync(db: &Database, pattern: &str) -> Result<Vec<String>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let table = read_txn
        .open_table(KV_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;

    let pattern_str = pattern.replace('*', "");
    let mut keys = Vec::new();

    for entry in table.iter().map_err(|e| Error::Internal(e.to_string()))? {
        let (key, _) = entry.map_err(|e| Error::Internal(e.to_string()))?;
        let key_str = key.value();
        if (pattern == "*" || key_str.contains(&pattern_str)) && !is_key_expired(db, key_str) {
            keys.push(key_str.to_string());
        }
    }

    Ok(keys)
}

fn incr_sync(db: &Database, key: &str) -> Result<i64> {
    let write_txn = db
        .begin_write()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let new_value = {
        let mut table = write_txn
            .open_table(KV_TABLE)
            .map_err(|e| Error::Internal(e.to_string()))?;

        let current = table
            .get(key)
            .map_err(|e| Error::Internal(e.to_string()))?
            .and_then(|v| v.value().parse::<i64>().ok())
            .unwrap_or(0);

        let new_value = current + 1;
        let value_str = new_value.to_string();
        table
            .insert(key, value_str.as_str())
            .map_err(|e| Error::Internal(e.to_string()))?;
        new_value
    };
    write_txn
        .commit()
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(new_value)
}

fn mget_sync(db: &Database, keys: &[String]) -> Result<Vec<Option<String>>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let table = read_txn
        .open_table(KV_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;

    let mut results = Vec::with_capacity(keys.len());
    for key in keys {
        if is_key_expired(db, key) {
            results.push(None);
            continue;
        }
        match table.get(key.as_str()) {
            Ok(Some(v)) => results.push(Some(v.value().to_string())),
            _ => results.push(None),
        }
    }
    Ok(results)
}

fn mset_sync(db: &Database, pairs: &[(String, String)]) -> Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| Error::Internal(e.to_string()))?;
    {
        let mut table = write_txn
            .open_table(KV_TABLE)
            .map_err(|e| Error::Internal(e.to_string()))?;
        for (k, v) in pairs {
            table
                .insert(k.as_str(), v.as_str())
                .map_err(|e| Error::Internal(e.to_string()))?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(())
}

fn exists_sync(db: &Database, key: &str) -> Result<bool> {
    if is_key_expired(db, key) {
        return Ok(false);
    }

    let read_txn = db
        .begin_read()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let table = read_txn
        .open_table(KV_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;

    Ok(table
        .get(key)
        .map_err(|e| Error::Internal(e.to_string()))?
        .is_some())
}

fn ttl_sync(db: &Database, key: &str) -> Result<i64> {
    let read_txn = db
        .begin_read()
        .map_err(|e| Error::Internal(e.to_string()))?;

    let table = read_txn
        .open_table(KV_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;
    if table
        .get(key)
        .map_err(|e| Error::Internal(e.to_string()))?
        .is_none()
    {
        return Ok(-2);
    }

    let ttl_table = read_txn
        .open_table(TTL_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;
    match ttl_table.get(key) {
        Ok(Some(expires)) => {
            let now = current_timestamp();
            let expires_at = expires.value();
            if now >= expires_at {
                Ok(-2)
            } else {
                #[allow(clippy::cast_possible_wrap)]
                Ok((expires_at - now) as i64)
            }
        }
        _ => Ok(-1),
    }
}

fn expire_sync(db: &Database, key: &str, seconds: i64) -> Result<bool> {
    let read_txn = db
        .begin_read()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let table = read_txn
        .open_table(KV_TABLE)
        .map_err(|e| Error::Internal(e.to_string()))?;
    if table
        .get(key)
        .map_err(|e| Error::Internal(e.to_string()))?
        .is_none()
    {
        return Ok(false);
    }
    drop(read_txn);

    let write_txn = db
        .begin_write()
        .map_err(|e| Error::Internal(e.to_string()))?;
    {
        let mut ttl_table = write_txn
            .open_table(TTL_TABLE)
            .map_err(|e| Error::Internal(e.to_string()))?;
        #[allow(clippy::cast_sign_loss)]
        let expires_at = current_timestamp() + seconds as u64;
        ttl_table
            .insert(key, expires_at)
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    write_txn
        .commit()
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(true)
}

impl KvBackend for RedbBackend {
    fn name(&self) -> &'static str {
        "redb"
    }

    fn get(&self, key: &str) -> BoxFuture<'_, Result<Option<String>>> {
        let db = self.db.clone();
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || get_sync(&db, &key))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn set(&self, key: &str, value: &str, ttl: Option<i64>) -> BoxFuture<'_, Result<()>> {
        let db = self.db.clone();
        let key = key.to_string();
        let value = value.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || set_sync(&db, &key, &value, ttl))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn del(&self, key: &str) -> BoxFuture<'_, Result<i64>> {
        let db = self.db.clone();
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || del_sync(&db, &key))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn keys(&self, pattern: &str) -> BoxFuture<'_, Result<Vec<String>>> {
        let db = self.db.clone();
        let pattern = pattern.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || keys_sync(&db, &pattern))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn incr(&self, key: &str) -> BoxFuture<'_, Result<i64>> {
        let db = self.db.clone();
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || incr_sync(&db, &key))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn mget(&self, keys: &[String]) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let db = self.db.clone();
        let keys = keys.to_vec();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || mget_sync(&db, &keys))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn mset(&self, pairs: &[(String, String)]) -> BoxFuture<'_, Result<()>> {
        let db = self.db.clone();
        let pairs = pairs.to_vec();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || mset_sync(&db, &pairs))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn exists(&self, key: &str) -> BoxFuture<'_, Result<bool>> {
        let db = self.db.clone();
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || exists_sync(&db, &key))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn ttl(&self, key: &str) -> BoxFuture<'_, Result<i64>> {
        let db = self.db.clone();
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || ttl_sync(&db, &key))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn expire(&self, key: &str, seconds: i64) -> BoxFuture<'_, Result<bool>> {
        let db = self.db.clone();
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || expire_sync(&db, &key, seconds))
                .await
                .map_err(|e| Error::Internal(format!("Task join error: {e}")))?
        })
    }

    fn health(&self) -> BoxFuture<'_, HealthResult> {
        let db = self.db.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || db.begin_read()).await {
                Ok(Ok(_)) => HealthResult::healthy(),
                Ok(Err(e)) => HealthResult::unhealthy(format!("Database error: {e}")),
                Err(e) => HealthResult::unhealthy(format!("Task join error: {e}")),
            }
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // MemoryBackend Tests
    // ========================================================================

    #[tokio::test]
    async fn test_memory_set_and_get() {
        let backend = MemoryBackend::new();
        backend.set("key1", "value1", None).await.unwrap();
        let value = backend.get("key1").await.unwrap();
        assert_eq!(value, Some("value1".to_string()));
    }

    #[tokio::test]
    async fn test_memory_get_nonexistent() {
        let backend = MemoryBackend::new();
        let value = backend.get("nonexistent").await.unwrap();
        assert!(value.is_none());
    }

    #[tokio::test]
    async fn test_memory_delete() {
        let backend = MemoryBackend::new();
        backend.set("key1", "value1", None).await.unwrap();
        let deleted = backend.del("key1").await.unwrap();
        assert_eq!(deleted, 1);
        let value = backend.get("key1").await.unwrap();
        assert!(value.is_none());
    }

    #[tokio::test]
    async fn test_memory_delete_nonexistent() {
        let backend = MemoryBackend::new();
        let deleted = backend.del("nonexistent").await.unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn test_memory_exists() {
        let backend = MemoryBackend::new();
        assert!(!backend.exists("key1").await.unwrap());
        backend.set("key1", "value1", None).await.unwrap();
        assert!(backend.exists("key1").await.unwrap());
        backend.del("key1").await.unwrap();
        assert!(!backend.exists("key1").await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_keys() {
        let backend = MemoryBackend::new();
        backend.set("user:1", "alice", None).await.unwrap();
        backend.set("user:2", "bob", None).await.unwrap();
        backend.set("session:abc", "data", None).await.unwrap();
        let all_keys = backend.keys("*").await.unwrap();
        assert_eq!(all_keys.len(), 3);
        let user_keys = backend.keys("user").await.unwrap();
        assert_eq!(user_keys.len(), 2);
    }

    #[tokio::test]
    async fn test_memory_increment() {
        let backend = MemoryBackend::new();
        let val = backend.incr("counter").await.unwrap();
        assert_eq!(val, 1);
        let val = backend.incr("counter").await.unwrap();
        assert_eq!(val, 2);
        let val = backend.incr("counter").await.unwrap();
        assert_eq!(val, 3);
    }

    #[tokio::test]
    async fn test_memory_mget() {
        let backend = MemoryBackend::new();
        backend.set("key1", "value1", None).await.unwrap();
        backend.set("key2", "value2", None).await.unwrap();
        let values = backend
            .mget(&["key1".to_string(), "key2".to_string(), "key3".to_string()])
            .await
            .unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0], Some("value1".to_string()));
        assert_eq!(values[1], Some("value2".to_string()));
        assert_eq!(values[2], None);
    }

    #[tokio::test]
    async fn test_memory_mset() {
        let backend = MemoryBackend::new();
        backend
            .mset(&[
                ("key1".to_string(), "value1".to_string()),
                ("key2".to_string(), "value2".to_string()),
            ])
            .await
            .unwrap();
        assert_eq!(
            backend.get("key1").await.unwrap(),
            Some("value1".to_string())
        );
        assert_eq!(
            backend.get("key2").await.unwrap(),
            Some("value2".to_string())
        );
    }

    #[tokio::test]
    async fn test_memory_ttl_query() {
        let backend = MemoryBackend::new();
        backend.set("permanent", "value", None).await.unwrap();
        assert_eq!(backend.ttl("permanent").await.unwrap(), -1);
        backend.set("temp", "value", Some(3600)).await.unwrap();
        let ttl = backend.ttl("temp").await.unwrap();
        assert!(ttl > 0 && ttl <= 3600);
        assert_eq!(backend.ttl("nonexistent").await.unwrap(), -2);
    }

    #[tokio::test]
    async fn test_memory_expire() {
        let backend = MemoryBackend::new();
        backend.set("key", "value", None).await.unwrap();
        let result = backend.expire("key", 3600).await.unwrap();
        assert!(result);
        let ttl = backend.ttl("key").await.unwrap();
        assert!(ttl > 0);
        let result = backend.expire("nonexistent", 3600).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_memory_health() {
        let backend = MemoryBackend::new();
        let health = backend.health().await;
        assert!(health.healthy);
    }

    #[tokio::test]
    async fn test_memory_overwrite() {
        let backend = MemoryBackend::new();
        backend.set("key", "value1", None).await.unwrap();
        backend.set("key", "value2", None).await.unwrap();
        assert_eq!(
            backend.get("key").await.unwrap(),
            Some("value2".to_string())
        );
    }

    // ========================================================================
    // RedbBackend Tests
    // ========================================================================

    #[tokio::test]
    async fn test_redb_set_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        backend.set("key1", "value1", None).await.unwrap();
        let value = backend.get("key1").await.unwrap();
        assert_eq!(value, Some("value1".to_string()));
    }

    #[tokio::test]
    async fn test_redb_get_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        let value = backend.get("nonexistent").await.unwrap();
        assert!(value.is_none());
    }

    #[tokio::test]
    async fn test_redb_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        backend.set("key1", "value1", None).await.unwrap();
        let deleted = backend.del("key1").await.unwrap();
        assert_eq!(deleted, 1);
        let value = backend.get("key1").await.unwrap();
        assert!(value.is_none());
    }

    #[tokio::test]
    async fn test_redb_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        assert!(!backend.exists("key1").await.unwrap());
        backend.set("key1", "value1", None).await.unwrap();
        assert!(backend.exists("key1").await.unwrap());
    }

    #[tokio::test]
    async fn test_redb_increment() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        let val = backend.incr("counter").await.unwrap();
        assert_eq!(val, 1);
        let val = backend.incr("counter").await.unwrap();
        assert_eq!(val, 2);
    }

    #[tokio::test]
    async fn test_redb_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        backend.set("user:1", "alice", None).await.unwrap();
        backend.set("user:2", "bob", None).await.unwrap();
        backend.set("session:abc", "data", None).await.unwrap();
        let all_keys = backend.keys("*").await.unwrap();
        assert_eq!(all_keys.len(), 3);
    }

    #[tokio::test]
    async fn test_redb_mget_mset() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        backend
            .mset(&[
                ("key1".to_string(), "value1".to_string()),
                ("key2".to_string(), "value2".to_string()),
            ])
            .await
            .unwrap();
        let values = backend
            .mget(&["key1".to_string(), "key2".to_string(), "key3".to_string()])
            .await
            .unwrap();
        assert_eq!(values[0], Some("value1".to_string()));
        assert_eq!(values[1], Some("value2".to_string()));
        assert_eq!(values[2], None);
    }

    #[tokio::test]
    async fn test_redb_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        backend.set("permanent", "value", None).await.unwrap();
        assert_eq!(backend.ttl("permanent").await.unwrap(), -1);
        backend.set("temp", "value", Some(3600)).await.unwrap();
        let ttl = backend.ttl("temp").await.unwrap();
        assert!(ttl > 0 && ttl <= 3600);
        assert_eq!(backend.ttl("nonexistent").await.unwrap(), -2);
    }

    #[tokio::test]
    async fn test_redb_expire() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        backend.set("key", "value", None).await.unwrap();
        let result = backend.expire("key", 3600).await.unwrap();
        assert!(result);
        let ttl = backend.ttl("key").await.unwrap();
        assert!(ttl > 0);
    }

    #[tokio::test]
    async fn test_redb_health() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let backend = RedbBackend::new(&db_path).unwrap();
        let health = backend.health().await;
        assert!(health.healthy);
    }

    // ========================================================================
    // KvService Tests
    // ========================================================================

    #[test]
    fn test_kv_service_from_url_memory() {
        let service = KvService::from_url("memory://").unwrap();
        assert_eq!(service.backend.name(), "memory");
    }

    #[test]
    fn test_kv_service_from_url_redb() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.redb");
        let url = format!("redb://{}", db_path.display());
        let service = KvService::from_url(&url).unwrap();
        assert_eq!(service.backend.name(), "redb");
    }

    #[test]
    fn test_kv_service_from_url_invalid() {
        let result = KvService::from_url("invalid://something");
        assert!(result.is_err());
    }

    #[test]
    fn test_kv_service_name() {
        let service = KvService::from_url("memory://").unwrap();
        assert_eq!(service.name(), "kv");
    }

    #[test]
    fn test_kv_service_router() {
        let service = KvService::from_url("memory://").unwrap();
        let _router = service.router();
    }
}
