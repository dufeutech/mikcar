//! Sidecar service builder.
//!
//! Provides a fluent API for configuring and running sidecars.

use crate::auth::{auth_middleware, TokenAuth};
use crate::health::simple_health_handler;
use crate::{Result, Sidecar};

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{header::HeaderName, Method, Request, Response, StatusCode},
    middleware::{self, Next},
    routing::get,
    Router,
};
use std::net::SocketAddr;
use std::time::Duration;
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};
use tracing::{info, warn};
use uuid::Uuid;

// =============================================================================
// Trace ID Support
// =============================================================================

/// Extension type for storing trace ID in request extensions.
#[derive(Clone, Debug)]
pub struct TraceId(pub String);

/// Middleware that extracts or generates a trace ID for distributed tracing.
///
/// - Extracts `x-trace-id` header from incoming request if present
/// - Generates a new UUID if no trace ID header exists
/// - Stores trace ID in request extensions (accessible via `Extension<TraceId>`)
/// - Adds `X-Trace-ID` header to response
/// - Creates a tracing span with the trace ID for log correlation
async fn trace_id_middleware(req: Request<Body>, next: Next) -> Response<Body> {
    // Extract trace_id from incoming header or generate new one
    let trace_id = req
        .headers()
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| Uuid::new_v4().to_string(), String::from);

    // Create span with trace_id for log correlation
    let span = tracing::info_span!(
        "request",
        trace_id = %trace_id,
        method = %req.method(),
        path = %req.uri().path()
    );
    let _guard = span.enter();

    // Store trace_id in extensions for handlers to access
    let mut req = req;
    req.extensions_mut().insert(TraceId(trace_id.clone()));

    // Call next middleware/handler
    let mut response = next.run(req).await;

    // Add trace ID to response headers
    if let Ok(header_value) = trace_id.parse() {
        response.headers_mut().insert("x-trace-id", header_value);
    }

    response
}

/// Default maximum request body size: 10 MB
const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// CORS configuration for sidecar services.
///
/// By default, CORS is disabled (no cross-origin requests allowed).
/// Use the builder methods to configure allowed origins, methods, and headers.
#[derive(Clone, Debug, Default)]
pub struct CorsConfig {
    /// Whether CORS is enabled
    enabled: bool,
    /// Allowed origins (empty = allow any if enabled)
    origins: Vec<String>,
    /// Allowed methods (empty = allow any if enabled)
    methods: Vec<Method>,
    /// Allowed headers (empty = allow any if enabled)
    headers: Vec<HeaderName>,
}

impl CorsConfig {
    /// Create a new disabled CORS configuration.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Create a permissive CORS configuration that allows any origin.
    ///
    /// **Warning:** This is insecure for production. Only use for local development.
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            enabled: true,
            origins: Vec::new(),
            methods: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Create a CORS configuration with specific allowed origins.
    ///
    /// # Example
    /// ```
    /// use mikcar::CorsConfig;
    ///
    /// let cors = CorsConfig::with_origins(["https://example.com", "https://app.example.com"]);
    /// ```
    #[must_use]
    pub fn with_origins<I, S>(origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            enabled: true,
            origins: origins.into_iter().map(Into::into).collect(),
            methods: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Set allowed HTTP methods.
    #[must_use]
    pub fn methods<I>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = Method>,
    {
        self.methods = methods.into_iter().collect();
        self
    }

    /// Set allowed headers.
    #[must_use]
    pub fn headers<I>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = HeaderName>,
    {
        self.headers = headers.into_iter().collect();
        self
    }

    /// Build a tower-http `CorsLayer` from this configuration.
    fn into_layer(self) -> Option<CorsLayer> {
        if !self.enabled {
            return None;
        }

        let mut layer = CorsLayer::new();

        // Configure origins
        if self.origins.is_empty() {
            warn!("CORS configured with allow_any_origin - this is insecure for production");
            layer = layer.allow_origin(AllowOrigin::any());
        } else {
            let origins: Vec<_> = self
                .origins
                .iter()
                .filter_map(|o| o.parse().ok())
                .collect();
            layer = layer.allow_origin(origins);
        }

        // Configure methods
        if self.methods.is_empty() {
            layer = layer.allow_methods(AllowMethods::any());
        } else {
            layer = layer.allow_methods(self.methods);
        }

        // Configure headers
        if self.headers.is_empty() {
            layer = layer.allow_headers(AllowHeaders::any());
        } else {
            layer = layer.allow_headers(self.headers);
        }

        Some(layer)
    }
}

/// Builder for configuring and running a sidecar service.
#[derive(Debug)]
pub struct SidecarBuilder {
    port: u16,
    auth: TokenAuth,
    cors_config: CorsConfig,
    enable_compression: bool,
    timeout_secs: u64,
    /// Maximum request body size in bytes.
    max_body_size: usize,
}

impl Default for SidecarBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SidecarBuilder {
    /// Create a new sidecar builder with default settings.
    ///
    /// **Note:** CORS is disabled by default. Use `.cors()` or `.cors_permissive()`
    /// to enable cross-origin requests.
    #[must_use]
    pub fn new() -> Self {
        Self {
            port: 3001,
            auth: TokenAuth::from_env(),
            cors_config: CorsConfig::disabled(),
            enable_compression: true,
            timeout_secs: 30,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
        }
    }

    /// Set the port to listen on.
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the authentication token.
    #[must_use]
    pub fn auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth = TokenAuth::new(token);
        self
    }

    /// Load authentication token from environment.
    #[must_use]
    pub fn auth_token_from_env(mut self) -> Self {
        self.auth = TokenAuth::from_env();
        self
    }

    /// Disable authentication.
    #[must_use]
    pub fn no_auth(mut self) -> Self {
        self.auth = TokenAuth::disabled();
        self
    }

    /// Configure CORS with a custom configuration.
    ///
    /// # Example
    /// ```
    /// use mikcar::{SidecarBuilder, CorsConfig};
    ///
    /// let builder = SidecarBuilder::new()
    ///     .cors(CorsConfig::with_origins(["https://example.com"]));
    /// ```
    #[must_use]
    pub fn cors(mut self, config: CorsConfig) -> Self {
        self.cors_config = config;
        self
    }

    /// Enable permissive CORS that allows any origin.
    ///
    /// **Warning:** This is insecure for production. Only use for local development
    /// or internal services behind a firewall.
    #[must_use]
    pub fn cors_permissive(mut self) -> Self {
        self.cors_config = CorsConfig::permissive();
        self
    }

    /// Disable CORS entirely (no cross-origin requests allowed).
    #[must_use]
    pub fn cors_disabled(mut self) -> Self {
        self.cors_config = CorsConfig::disabled();
        self
    }

    /// Enable or disable compression.
    #[must_use]
    pub fn compression(mut self, enabled: bool) -> Self {
        self.enable_compression = enabled;
        self
    }

    /// Set request timeout in seconds.
    #[must_use]
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Set maximum request body size in bytes.
    ///
    /// Default is 10 MB. Use this to limit memory usage from large payloads.
    #[must_use]
    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.max_body_size = bytes;
        self
    }

    /// Set maximum request body size in megabytes.
    ///
    /// Convenience method for `max_body_size(mb * 1024 * 1024)`.
    #[must_use]
    pub fn max_body_size_mb(self, mb: usize) -> Self {
        self.max_body_size(mb * 1024 * 1024)
    }

    /// Serve a sidecar service.
    ///
    /// This starts the HTTP server and blocks until shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The TCP listener cannot bind to the configured port
    /// - The server encounters a fatal error during operation
    pub async fn serve<S: Sidecar>(self, sidecar: S) -> Result<()> {
        let service_name = sidecar.name();

        // Build the router
        let mut app = sidecar
            .router()
            .route("/health", get(simple_health_handler));

        // Add middleware layers
        // NOTE: Tower layers are applied in reverse order (last added = first to run)
        // Desired execution order: Trace -> Timeout -> BodyLimit -> CORS -> Compression -> Auth
        // So we add them in reverse: Auth -> Compression -> CORS -> BodyLimit -> Timeout -> Trace

        // 1. Auth middleware (innermost - runs last)
        let auth = self.auth.clone();
        app = app.layer(middleware::from_fn(move |req, next| {
            let auth = auth.clone();
            auth_middleware(auth, req, next)
        }));

        // 2. Compression (runs before auth)
        if self.enable_compression {
            app = app.layer(CompressionLayer::new());
        }

        // 3. CORS layer (runs BEFORE auth to handle preflight OPTIONS requests)
        // This is critical: preflight requests must not require authentication
        let cors_enabled = self.cors_config.enabled;
        if let Some(cors_layer) = self.cors_config.clone().into_layer() {
            app = app.layer(cors_layer);
        }

        // 4. Body size limits, timeout, and tracing (outermost layers)
        app = app
            .layer(DefaultBodyLimit::max(self.max_body_size))
            .layer(RequestBodyLimitLayer::new(self.max_body_size))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(self.timeout_secs),
            ))
            .layer(TraceLayer::new_for_http())
            // 5. Trace ID extraction (outermost - runs first)
            .layer(middleware::from_fn(trace_id_middleware));

        // Start server
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener = tokio::net::TcpListener::bind(addr).await?;

        info!(
            service = service_name,
            port = self.port,
            auth = self.auth.is_enabled(),
            cors = cors_enabled,
            max_body_size = self.max_body_size,
            "Starting sidecar"
        );

        axum::serve(listener, app).await?;

        Ok(())
    }

    /// Serve multiple sidecars on the same port (supercar mode).
    ///
    /// Each sidecar is mounted at its own path prefix.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The TCP listener cannot bind to the configured port
    /// - The server encounters a fatal error during operation
    pub async fn serve_multi(self, sidecars: Vec<(&str, Box<dyn Sidecar>)>) -> Result<()> {
        let service_count = sidecars.len();
        let mut app = Router::new().route("/health", get(simple_health_handler));

        // Mount each sidecar at its path prefix
        for (prefix, sidecar) in sidecars {
            info!(service = sidecar.name(), prefix = prefix, "Mounting service");
            app = app.nest(prefix, sidecar.router());
        }

        // Add middleware layers
        // NOTE: Tower layers are applied in reverse order (last added = first to run)
        // Desired execution order: Trace -> Timeout -> BodyLimit -> CORS -> Compression -> Auth
        // So we add them in reverse: Auth -> Compression -> CORS -> BodyLimit -> Timeout -> Trace

        // 1. Auth middleware (innermost - runs last)
        let auth = self.auth.clone();
        app = app.layer(middleware::from_fn(move |req, next| {
            let auth = auth.clone();
            auth_middleware(auth, req, next)
        }));

        // 2. Compression (runs before auth)
        if self.enable_compression {
            app = app.layer(CompressionLayer::new());
        }

        // 3. CORS layer (runs BEFORE auth to handle preflight OPTIONS requests)
        // This is critical: preflight requests must not require authentication
        let cors_enabled = self.cors_config.enabled;
        if let Some(cors_layer) = self.cors_config.into_layer() {
            app = app.layer(cors_layer);
        }

        // 4. Body size limits, timeout, and tracing (outermost layers)
        app = app
            .layer(DefaultBodyLimit::max(self.max_body_size))
            .layer(RequestBodyLimitLayer::new(self.max_body_size))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(self.timeout_secs),
            ))
            .layer(TraceLayer::new_for_http())
            // 5. Trace ID extraction (outermost - runs first)
            .layer(middleware::from_fn(trace_id_middleware));

        // Start server
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener = tokio::net::TcpListener::bind(addr).await?;

        info!(
            port = self.port,
            services = service_count,
            cors = cors_enabled,
            max_body_size = self.max_body_size,
            "Starting supercar (multi-service mode)"
        );

        axum::serve(listener, app).await?;

        Ok(())
    }
}
