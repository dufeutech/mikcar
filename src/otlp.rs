//! OpenTelemetry OTLP export for mikcar (distributed tracing).
//!
//! Enables trace export to backends like Jaeger, Tempo, or Zipkin.
//!
//! # Feature Flag
//!
//! This module is only available when the `otlp` feature is enabled:
//!
//! ```bash
//! cargo build -p mikcar --features otlp
//! ```
//!
//! # Usage
//!
//! ```bash
//! mikcar --otlp-endpoint http://localhost:4317 --service-name my-sidecar --storage
//! ```

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::OnceLock;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Global tracer provider for shutdown.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// OTLP exporter configuration.
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// OTLP endpoint (default: http://localhost:4317)
    pub endpoint: String,
    /// Service name for traces (default: mikcar)
    pub service_name: String,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4317".to_string(),
            service_name: "mikcar".to_string(),
        }
    }
}

impl OtlpConfig {
    /// Create a new OTLP config with the given endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }

    /// Set the service name.
    #[must_use]
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }
}

/// Error initializing OTLP.
#[derive(Debug)]
pub enum OtlpError {
    /// Failed to initialize exporter
    ExporterInit(String),
    /// Already initialized
    AlreadyInitialized,
}

impl std::fmt::Display for OtlpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExporterInit(e) => write!(f, "failed to init OTLP exporter: {e}"),
            Self::AlreadyInitialized => write!(f, "OTLP already initialized"),
        }
    }
}

impl std::error::Error for OtlpError {}

/// Initialize the OpenTelemetry tracer provider.
fn init_tracer_provider(config: &OtlpConfig) -> Result<SdkTracerProvider, OtlpError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.endpoint)
        .build()
        .map_err(|e| OtlpError::ExporterInit(e.to_string()))?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", config.service_name.clone())])
                .build(),
        )
        .build();

    Ok(provider)
}

/// Initialize tracing with OTLP export layer.
///
/// This sets up both stdout logging and OTLP trace export.
/// Spans will be sent to the configured OTLP endpoint.
pub fn init_with_otlp(config: &OtlpConfig) -> Result<(), OtlpError> {
    let provider = init_tracer_provider(config)?;
    let tracer = provider.tracer("mikcar");

    // Store provider for shutdown
    TRACER_PROVIDER
        .set(provider)
        .map_err(|_| OtlpError::AlreadyInitialized)?;

    // Build the OpenTelemetry layer
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Build filter from RUST_LOG env or default
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mikcar=info,tower_http=debug"));

    // Combine fmt layer + otel layer
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .with(otel_layer)
        .init();

    tracing::info!(
        endpoint = %config.endpoint,
        service = %config.service_name,
        "OTLP tracing enabled"
    );

    Ok(())
}

/// Shutdown the OTLP tracer (flush pending spans).
///
/// Call this before application exit to ensure all spans are exported.
pub fn shutdown() {
    if let Some(provider) = TRACER_PROVIDER.get()
        && let Err(e) = provider.shutdown()
    {
        tracing::warn!("Failed to shutdown tracer provider: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_otlp_config_default() {
        let config = OtlpConfig::default();
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.service_name, "mikcar");
    }

    #[test]
    fn test_otlp_config_builder() {
        let config = OtlpConfig::new("http://jaeger:4317").with_service_name("my-sidecar");

        assert_eq!(config.endpoint, "http://jaeger:4317");
        assert_eq!(config.service_name, "my-sidecar");
    }
}
