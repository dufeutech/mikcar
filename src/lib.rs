// =============================================================================
// Lint Configuration
// =============================================================================

// Safety: No unsafe code allowed in this crate
#![forbid(unsafe_code)]

// Correctness: Must handle all fallible operations
#![deny(unused_must_use)]

// Quality: Pedantic but pragmatic
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]

// Allowed with documented reasons
#![allow(clippy::missing_errors_doc)]      // Error returns self-documenting via type
#![allow(clippy::missing_panics_doc)]      // Panics documented in main entry points
#![allow(clippy::module_name_repetitions)] // e.g., runtime::RuntimeConfig is clearer
#![allow(clippy::doc_markdown)]            // Too many false positives in code docs
#![allow(clippy::must_use_candidate)]      // Not all returned values need annotation
#![allow(clippy::cast_possible_truncation)] // Intentional in HTTP size calculations
#![allow(clippy::cast_sign_loss)]          // Intentional in size calculations

//! # mikcar - Sidecar Infrastructure for mikrozen
//!
//! mikcar provides HTTP-based infrastructure services (storage, key-value, SQL, queues)
//! for WASM handlers running in mik. Same API works locally and in production.
//!
//! ## Features
//!
//! Enable only what you need via Cargo features:
//!
//! - `storage` - Object storage (S3, GCS, `MinIO`, local filesystem)
//! - `kv` - Key-value store (defaults to embedded redb)
//! - `kv-redb` - Embedded key-value store (pure Rust, musl compatible)
//! - `kv-redis` - Redis backend (requires external Redis server)
//! - `sql` - SQL database proxy (Postgres, `MySQL`, `SQLite`)
//! - `queue` - Message queues (SQS, `RabbitMQ`, Redis Streams)
//! - `email` - Email sending (SMTP, Resend, `SendGrid`, AWS SES)
//! - `all` - Enable all services (supercar mode)
//!
//! ## Example
//!
//! ```rust,ignore
//! use mikcar::{SidecarBuilder, StorageService};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let storage = StorageService::from_env()?;
//!
//!     SidecarBuilder::new()
//!         .port(3001)
//!         .auth_token_from_env()
//!         .serve(storage)
//!         .await
//! }
//! ```

pub mod auth;
pub mod builder;
pub mod error;
pub mod health;

// Service modules (feature-gated)
#[cfg(feature = "storage")]
pub mod storage;

#[cfg(feature = "kv")]
pub mod kv;

#[cfg(feature = "sql")]
pub mod sql;

#[cfg(feature = "queue")]
pub mod queue;

#[cfg(feature = "secrets")]
pub mod secrets;

#[cfg(feature = "email-smtp")]
pub mod email;

#[cfg(feature = "otlp")]
pub mod otlp;

// Re-exports
pub use auth::TokenAuth;
pub use builder::{CorsConfig, SidecarBuilder, TraceId};
pub use error::{Error, Result};
pub use health::{HealthCheck, HealthResult};

// Service re-exports
#[cfg(feature = "storage")]
pub use storage::StorageService;

#[cfg(feature = "kv")]
pub use kv::KvService;

#[cfg(feature = "sql")]
pub use sql::SqlService;

#[cfg(feature = "queue")]
pub use queue::QueueService;

#[cfg(feature = "secrets")]
pub use secrets::SecretsService;

#[cfg(feature = "email-smtp")]
pub use email::EmailService;

// Re-export axum for convenience
pub use axum;
pub use tower;
pub use tower_http;

/// Trait for implementing sidecar services.
///
/// Each service provides its own router and health check.
pub trait Sidecar: Send + Sync + 'static {
    /// Service name for logging/metrics.
    fn name(&self) -> &'static str;

    /// Build the axum router for this service.
    fn router(&self) -> axum::Router;

    /// Health check (default: always healthy).
    fn health_check(&self) -> bool {
        true
    }
}

// Implement Sidecar for Box<dyn Sidecar> to allow dynamic dispatch
impl Sidecar for Box<dyn Sidecar> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn router(&self) -> axum::Router {
        (**self).router()
    }

    fn health_check(&self) -> bool {
        (**self).health_check()
    }
}
