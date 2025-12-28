//! Object storage service.
//!
//! Provides a unified HTTP API for object storage backends:
//! - Local filesystem
//! - AWS S3
//! - Google Cloud Storage
//! - Azure Blob Storage
//! - `MinIO` (S3-compatible)

use crate::{Error, Result, Sidecar};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, head, put},
};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, aws::AmazonS3Builder};
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use std::sync::Arc;

/// Validate a storage path for security.
///
/// Ensures the path:
/// - Is not empty
/// - Does not start with `/` or `\` (absolute paths)
/// - Does not contain null bytes
/// - Does not contain `..` components (path traversal)
fn validate_storage_path(path: &str) -> Result<()> {
    // Reject empty paths
    if path.is_empty() {
        return Err(Error::BadRequest("Path cannot be empty".into()));
    }

    // Reject absolute paths
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(Error::BadRequest("Absolute paths not allowed".into()));
    }

    // Reject null bytes
    if path.contains('\0') {
        return Err(Error::BadRequest("Null bytes not allowed in path".into()));
    }

    // Check each component for traversal attempts
    for component in StdPath::new(path).components() {
        if let std::path::Component::ParentDir = component {
            return Err(Error::BadRequest("Path traversal not allowed".into()));
        }
    }

    Ok(())
}

/// Storage service configuration.
#[derive(Clone)]
pub struct StorageService {
    store: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for StorageService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageService")
            .field("store", &"<ObjectStore>")
            .finish()
    }
}

impl StorageService {
    /// Create a new storage service with the given backend.
    pub fn new(store: impl ObjectStore + 'static) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Create storage service from environment configuration.
    ///
    /// Reads `STORAGE_URL` environment variable.
    /// Supported URL schemes:
    /// - `file:///path/to/dir` - Local filesystem
    /// - `s3://bucket` - AWS S3
    /// - `gs://bucket` - Google Cloud Storage
    /// - `az://container` - Azure Blob Storage
    /// - `memory://` - In-memory (for testing)
    ///
    /// # Errors
    ///
    /// Returns an error if `STORAGE_URL` is not set or contains an invalid URL.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::StorageService;
    ///
    /// // Set STORAGE_URL=s3://my-bucket?endpoint=http://localhost:9000
    /// let storage = StorageService::from_env()?;
    /// ```
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("STORAGE_URL")
            .map_err(|_| Error::Config("STORAGE_URL environment variable not set".to_string()))?;

        Self::from_url(&url)
    }

    /// Create storage service from a URL.
    ///
    /// For S3-compatible stores (`MinIO`, etc.), use:
    /// `s3://bucket?endpoint=http://localhost:9000&access_key_id=xxx&secret_access_key=xxx`
    ///
    /// # Errors
    ///
    /// Returns an error if the URL is invalid or the backend cannot be initialized.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::StorageService;
    ///
    /// // Local filesystem storage
    /// let storage = StorageService::from_url("file:///tmp/storage")?;
    ///
    /// // S3 with MinIO
    /// let storage = StorageService::from_url(
    ///     "s3://mybucket?endpoint=http://localhost:9000&access_key_id=minioadmin&secret_access_key=minioadmin"
    /// )?;
    /// ```
    pub fn from_url(url: &str) -> Result<Self> {
        let parsed =
            url::Url::parse(url).map_err(|e| Error::Config(format!("Invalid STORAGE_URL: {e}")))?;

        let store: Arc<dyn ObjectStore> = if parsed.scheme() == "s3" {
            // Use builder for S3 to properly configure credentials and disable IMDS
            let bucket = parsed.host_str().unwrap_or("default");
            let mut builder = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_allow_http(true); // Allow non-HTTPS for local dev

            // Parse query params for configuration
            for (key, value) in parsed.query_pairs() {
                match key.as_ref() {
                    "endpoint" => builder = builder.with_endpoint(value.as_ref()),
                    "access_key_id" => builder = builder.with_access_key_id(value.as_ref()),
                    "secret_access_key" => builder = builder.with_secret_access_key(value.as_ref()),
                    "region" => builder = builder.with_region(value.as_ref()),
                    "allow_http" => {
                        if value == "true" {
                            builder = builder.with_allow_http(true);
                        }
                    }
                    _ => {} // Ignore unknown params
                }
            }

            // Check for AWS env vars as fallback
            if let Ok(key) = std::env::var("AWS_ACCESS_KEY_ID") {
                builder = builder.with_access_key_id(key);
            }
            if let Ok(secret) = std::env::var("AWS_SECRET_ACCESS_KEY") {
                builder = builder.with_secret_access_key(secret);
            }
            if let Ok(region) = std::env::var("AWS_REGION") {
                builder = builder.with_region(region);
            }

            Arc::new(
                builder
                    .build()
                    .map_err(|e| Error::Config(format!("Failed to create S3 store: {e}")))?,
            )
        } else {
            // Use parse_url for other backends (file://, gs://, az://, memory://)
            let (store, _) = object_store::parse_url(&parsed)
                .map_err(|e| Error::Config(format!("Failed to create store: {e}")))?;
            Arc::from(store)
        };

        Ok(Self { store })
    }

    /// Create an in-memory storage service (for testing).
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            store: Arc::new(object_store::memory::InMemory::new()),
        }
    }
}

impl Sidecar for StorageService {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/object/{*path}", get(get_object))
            .route("/object/{*path}", put(put_object))
            .route("/object/{*path}", delete(delete_object))
            .route("/object/{*path}", head(head_object))
            .route("/list/{*prefix}", get(list_objects))
            .route("/list", get(list_objects_root))
            .with_state(self.store.clone())
    }

    fn health_check(&self) -> bool {
        // Verify we can list objects from the storage backend.
        // Uses block_in_place to avoid blocking the async runtime.
        use futures::StreamExt;

        let store = self.store.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        // Try to list a single object to verify connectivity
                        let mut stream = store.list(None);
                        match stream.next().await {
                            Some(Ok(_)) | None => true, // Success (either got item or empty)
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "Storage health check failed");
                                false
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

/// Query parameters for list operation.
#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    /// Cursor for pagination - pass the `next_offset` from previous response
    #[serde(default)]
    offset: Option<String>,
}

/// Object metadata response.
#[derive(Serialize)]
struct ObjectMeta {
    location: String,
    size: u64,
    last_modified: String,
    e_tag: Option<String>,
}

/// List response.
#[derive(Serialize)]
struct ListResponse {
    objects: Vec<ObjectMeta>,
    next_offset: Option<String>,
}

/// Get object contents.
async fn get_object(
    State(store): State<Arc<dyn ObjectStore>>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse> {
    validate_storage_path(&path)?;
    let location = object_store::path::Path::from(path.as_str());

    let result = store.get(&location).await.map_err(|e| match e {
        object_store::Error::NotFound { .. } => Error::NotFound(path.clone()),
        e => Error::Storage(e),
    })?;

    let bytes = result.bytes().await?;

    Ok((StatusCode::OK, bytes))
}

/// Put object contents.
async fn put_object(
    State(store): State<Arc<dyn ObjectStore>>,
    Path(path): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse> {
    validate_storage_path(&path)?;
    let location = object_store::path::Path::from(path.as_str());

    let payload = PutPayload::from_bytes(body);
    store.put(&location, payload).await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "status": "created",
            "path": path
        })),
    ))
}

/// Delete object.
async fn delete_object(
    State(store): State<Arc<dyn ObjectStore>>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse> {
    validate_storage_path(&path)?;
    let location = object_store::path::Path::from(path.as_str());

    store.delete(&location).await.map_err(|e| match e {
        object_store::Error::NotFound { .. } => Error::NotFound(path.clone()),
        e => Error::Storage(e),
    })?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "deleted",
            "path": path
        })),
    ))
}

/// Check if object exists (HEAD request).
async fn head_object(
    State(store): State<Arc<dyn ObjectStore>>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse> {
    validate_storage_path(&path)?;
    let location = object_store::path::Path::from(path.as_str());

    let meta = store.head(&location).await.map_err(|e| match e {
        object_store::Error::NotFound { .. } => Error::NotFound(path.clone()),
        e => Error::Storage(e),
    })?;

    Ok((
        StatusCode::OK,
        [
            ("x-object-size", meta.size.to_string()),
            ("x-last-modified", meta.last_modified.to_rfc3339()),
        ],
    ))
}

/// List objects with prefix.
async fn list_objects(
    State(store): State<Arc<dyn ObjectStore>>,
    Path(prefix): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse> {
    validate_storage_path(&prefix)?;
    list_objects_impl(store, Some(prefix), query).await
}

/// List objects at root.
async fn list_objects_root(
    State(store): State<Arc<dyn ObjectStore>>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse> {
    list_objects_impl(store, None, query).await
}

/// Implementation of list objects.
async fn list_objects_impl(
    store: Arc<dyn ObjectStore>,
    prefix: Option<String>,
    query: ListQuery,
) -> Result<impl IntoResponse> {
    use futures::StreamExt;

    let prefix_path = prefix
        .as_ref()
        .map(|p| object_store::path::Path::from(p.as_str()));
    let limit = query.limit.unwrap_or(1000);

    // Use offset for cursor-based pagination if provided
    let stream = if let Some(ref offset) = query.offset {
        let offset_path = object_store::path::Path::from(offset.as_str());
        store.list_with_offset(prefix_path.as_ref(), &offset_path)
    } else {
        store.list(prefix_path.as_ref())
    };

    // Take limit + 1 to determine if there are more results
    let results: Vec<_> = stream.take(limit + 1).collect().await;

    let mut objects = Vec::new();
    let has_more = results.len() > limit;

    for result in results.into_iter().take(limit) {
        match result {
            Ok(meta) => {
                objects.push(ObjectMeta {
                    location: meta.location.to_string(),
                    size: meta.size,
                    last_modified: meta.last_modified.to_rfc3339(),
                    e_tag: meta.e_tag.clone(),
                });
            }
            Err(e) => return Err(Error::Storage(e)),
        }
    }

    // Return last object's path as cursor for next page
    let next_offset = if has_more {
        objects.last().map(|o| o.location.clone())
    } else {
        None
    };

    let response = ListResponse {
        objects,
        next_offset,
    };

    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_storage() {
        let service = StorageService::in_memory();
        let _router = service.router();

        // Test would use axum::test utilities
        assert_eq!(service.name(), "storage");
    }

    #[test]
    fn test_validate_storage_path_valid() {
        // Valid paths should pass
        assert!(validate_storage_path("file.txt").is_ok());
        assert!(validate_storage_path("folder/file.txt").is_ok());
        assert!(validate_storage_path("a/b/c/d.txt").is_ok());
        assert!(validate_storage_path("my-file_name.json").is_ok());
    }

    #[test]
    fn test_validate_storage_path_empty() {
        let result = validate_storage_path("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("empty")));
    }

    #[test]
    fn test_validate_storage_path_absolute() {
        // Unix-style absolute path
        let result = validate_storage_path("/etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("Absolute")));

        // Windows-style absolute path
        let result = validate_storage_path("\\Windows\\System32");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("Absolute")));
    }

    #[test]
    fn test_validate_storage_path_null_bytes() {
        let result = validate_storage_path("file\0.txt");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("Null")));
    }

    #[test]
    fn test_validate_storage_path_traversal() {
        // Direct parent traversal
        let result = validate_storage_path("../secret.txt");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("traversal")));

        // Nested traversal
        let result = validate_storage_path("folder/../../../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("traversal")));

        // Traversal in the middle
        let result = validate_storage_path("a/b/../c/d");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BadRequest(msg) if msg.contains("traversal")));
    }

    #[test]
    fn test_validate_storage_path_current_dir_allowed() {
        // Current directory (.) should be allowed - it's harmless
        assert!(validate_storage_path("./file.txt").is_ok());
        assert!(validate_storage_path("a/./b/c.txt").is_ok());
    }
}
