//! Error types for mikcar services.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Result type alias for mikcar operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Error types for sidecar operations.
///
/// New variants may be added in future versions without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),

    /// Authentication failed.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Resource not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Bad request.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Internal server error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Storage backend error.
    #[cfg(feature = "storage")]
    #[error("storage error: {0}")]
    Storage(#[from] object_store::Error),

    /// SQL error.
    #[cfg(feature = "sql")]
    #[error("sql error: {0}")]
    Sql(#[from] sqlx::Error),

    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Determine status code and client-safe message
        // Internal errors are logged but not exposed to clients
        let (status, client_message, error_code) = match &self {
            Error::Config(_) => {
                tracing::error!(error = %self, "Configuration error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error",
                    "CONFIG_ERROR",
                )
            }
            Error::Unauthorized(msg) => {
                // Auth errors can be shown to client (no sensitive details)
                (StatusCode::UNAUTHORIZED, msg.as_str(), "UNAUTHORIZED")
            }
            Error::NotFound(msg) => {
                // Not found messages are generally safe
                (StatusCode::NOT_FOUND, msg.as_str(), "NOT_FOUND")
            }
            Error::BadRequest(msg) => {
                // Bad request messages are client-facing
                (StatusCode::BAD_REQUEST, msg.as_str(), "BAD_REQUEST")
            }
            Error::Internal(_) => {
                tracing::error!(error = %self, "Internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error",
                    "INTERNAL_ERROR",
                )
            }
            #[cfg(feature = "storage")]
            Error::Storage(_) => {
                tracing::error!(error = %self, "Storage backend error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Storage operation failed",
                    "STORAGE_ERROR",
                )
            }
            #[cfg(feature = "sql")]
            Error::Sql(_) => {
                tracing::error!(error = %self, "SQL error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database operation failed",
                    "SQL_ERROR",
                )
            }
            Error::Io(_) => {
                tracing::error!(error = %self, "IO error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error",
                    "IO_ERROR",
                )
            }
            Error::Json(_) => {
                tracing::warn!(error = %self, "JSON parsing error");
                (
                    StatusCode::BAD_REQUEST,
                    "Invalid JSON format",
                    "JSON_ERROR",
                )
            }
        };

        let body = serde_json::json!({
            "error": client_message,
            "code": error_code,
            "status": status.as_u16()
        });

        (status, axum::Json(body)).into_response()
    }
}
