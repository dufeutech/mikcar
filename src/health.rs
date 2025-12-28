//! Health check endpoint.
//!
//! Provides health check functionality with optional backend connectivity verification.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for async health checks.
const DEFAULT_HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Health check response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Service status.
    pub status: &'static str,
    /// Service name.
    pub service: String,
    /// Additional details (e.g., error message).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Result of a health check.
#[derive(Debug, Clone)]
pub struct HealthResult {
    /// Whether the service is healthy.
    pub healthy: bool,
    /// Optional details about the health status.
    pub details: Option<String>,
}

impl HealthResult {
    /// Create a healthy result.
    #[must_use]
    pub fn healthy() -> Self {
        Self {
            healthy: true,
            details: None,
        }
    }

    /// Create an unhealthy result with details.
    pub fn unhealthy(details: impl Into<String>) -> Self {
        Self {
            healthy: false,
            details: Some(details.into()),
        }
    }
}

/// Async health check function type.
pub type AsyncHealthCheckFn =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = HealthResult> + Send>> + Send + Sync>;

/// Health check state supporting both sync and async checks.
pub struct HealthCheck {
    /// Service name.
    pub name: String,
    /// Synchronous health check function.
    sync_check: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Async health check function (for backend connectivity).
    async_check: Option<AsyncHealthCheckFn>,
    /// Timeout for async checks.
    timeout: Duration,
}

impl std::fmt::Debug for HealthCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthCheck")
            .field("name", &self.name)
            .field("sync_check", &self.sync_check.as_ref().map(|_| "<fn>"))
            .field("async_check", &self.async_check.as_ref().map(|_| "<async fn>"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl HealthCheck {
    /// Create a new health check with a sync function.
    pub fn new(
        name: impl Into<String>,
        check_fn: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            sync_check: Some(Box::new(check_fn)),
            async_check: None,
            timeout: DEFAULT_HEALTH_CHECK_TIMEOUT,
        }
    }

    /// Create a health check with an async function for backend connectivity.
    ///
    /// This is preferred for services that need to verify database/cache connections.
    pub fn with_async<F, Fut>(name: impl Into<String>, check_fn: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HealthResult> + Send + 'static,
    {
        Self {
            name: name.into(),
            sync_check: None,
            async_check: Some(Box::new(move || Box::pin(check_fn()))),
            timeout: DEFAULT_HEALTH_CHECK_TIMEOUT,
        }
    }

    /// Create a health check that always passes.
    pub fn always_healthy(name: impl Into<String>) -> Self {
        Self::new(name, || true)
    }

    /// Set the timeout for async health checks.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Perform the health check (sync version for backward compatibility).
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        if let Some(ref check) = self.sync_check {
            check()
        } else {
            true // If only async check is configured, sync check passes
        }
    }

    /// Perform the health check asynchronously.
    ///
    /// This will:
    /// 1. Run the async check if configured (with timeout)
    /// 2. Fall back to sync check if no async check
    /// 3. Return healthy if neither is configured
    pub async fn check_async(&self) -> HealthResult {
        if let Some(ref async_check) = self.async_check {
            let check_future = async_check();

            match tokio::time::timeout(self.timeout, check_future).await {
                Ok(result) => result,
                Err(_) => HealthResult::unhealthy(format!(
                    "Health check timed out after {:?}",
                    self.timeout
                )),
            }
        } else if let Some(ref sync_check) = self.sync_check {
            if sync_check() {
                HealthResult::healthy()
            } else {
                HealthResult::unhealthy("Sync health check failed")
            }
        } else {
            HealthResult::healthy()
        }
    }
}

/// Health check handler with async connectivity verification.
pub async fn health_handler(State(health): State<Arc<HealthCheck>>) -> impl IntoResponse {
    let result = health.check_async().await;

    let response = HealthResponse {
        status: if result.healthy { "healthy" } else { "unhealthy" },
        service: health.name.clone(),
        details: result.details,
    };

    let status = if result.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(response))
}

/// Simple health handler without state (always returns healthy).
///
/// Use this for basic liveness checks. For readiness checks that verify
/// backend connectivity, use `health_handler` with a configured `HealthCheck`.
pub async fn simple_health_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy"
    }))
}
