//! Token-based authentication middleware.
//!
//! Simple bearer token auth for internal sidecar communication.
//! In production, sidecars should also be network-isolated.
//!
//! Uses constant-time comparison to prevent timing attacks.

use axum::{
    body::Body,
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

/// Token authentication configuration.
#[derive(Clone)]
pub struct TokenAuth {
    /// Expected token value.
    token: Option<String>,
}

impl std::fmt::Debug for TokenAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenAuth")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl TokenAuth {
    /// Create auth with a specific token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: Some(token.into()),
        }
    }

    /// Create auth from environment variable.
    ///
    /// Reads `SIDECAR_TOKEN` or `AUTH_TOKEN` from environment.
    /// If neither is set, authentication is disabled.
    #[must_use]
    pub fn from_env() -> Self {
        let token = std::env::var("SIDECAR_TOKEN")
            .or_else(|_| std::env::var("AUTH_TOKEN"))
            .ok();
        Self { token }
    }

    /// Create disabled auth (no token required).
    #[must_use]
    pub fn disabled() -> Self {
        Self { token: None }
    }

    /// Check if authentication is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.token.is_some()
    }

    /// Validate a token using constant-time comparison.
    ///
    /// Uses constant-time comparison to prevent timing attacks where an
    /// attacker could infer the correct token by measuring response times.
    #[must_use]
    pub fn validate(&self, provided: &str) -> bool {
        match &self.token {
            Some(expected) => {
                // Use constant-time comparison to prevent timing attacks.
                // Both strings must be compared in their entirety regardless
                // of where they first differ.
                let expected_bytes = expected.as_bytes();
                let provided_bytes = provided.as_bytes();

                // Length check must also be constant-time to avoid leaking length info
                if expected_bytes.len() != provided_bytes.len() {
                    // Still do a comparison to maintain constant time
                    let _ = expected_bytes.ct_eq(expected_bytes);
                    return false;
                }

                expected_bytes.ct_eq(provided_bytes).into()
            }
            None => true, // No auth required
        }
    }
}

/// Middleware function for token authentication.
///
/// Expects `Authorization: Bearer <token>` header.
pub async fn auth_middleware(
    auth: TokenAuth,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Skip auth if disabled
    if !auth.is_enabled() {
        return Ok(next.run(request).await);
    }

    // Skip auth for health endpoint
    if request.uri().path() == "/health" {
        return Ok(next.run(request).await);
    }

    // Extract Authorization header
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..];
            if auth.validate(token) {
                Ok(next.run(request).await)
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        Some(_) | None => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_auth_validation() {
        let auth = TokenAuth::new("secret123");
        assert!(auth.validate("secret123"));
        assert!(!auth.validate("wrong"));
    }

    #[test]
    fn test_disabled_auth() {
        let auth = TokenAuth::disabled();
        assert!(!auth.is_enabled());
        assert!(auth.validate("anything"));
    }
}
