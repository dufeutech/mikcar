//! Email service.
//!
//! Provides a unified HTTP API for sending emails via SMTP.
//! SMTP is the universal email protocol - works with any provider.

use crate::{Error, HealthCheck, HealthResult, Result, Sidecar};

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

// =============================================================================
// Types
// =============================================================================

/// Email message to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Email {
    /// Sender email address.
    pub from: String,
    /// Recipient email addresses.
    pub to: Vec<String>,
    /// CC recipients.
    #[serde(default)]
    pub cc: Vec<String>,
    /// BCC recipients.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Email subject.
    pub subject: String,
    /// Plain text body.
    #[serde(default)]
    pub text: Option<String>,
    /// HTML body.
    #[serde(default)]
    pub html: Option<String>,
    /// Reply-to address.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Custom headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Result of sending an email.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailResult {
    /// Whether the email was sent successfully.
    pub success: bool,
    /// Message ID (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Error message (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl EmailResult {
    /// Create a successful result.
    #[must_use]
    pub fn success(message_id: Option<String>) -> Self {
        Self {
            success: true,
            message_id,
            error: None,
        }
    }

    /// Create a failed result.
    #[must_use]
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            message_id: None,
            error: Some(error.into()),
        }
    }
}

// =============================================================================
// Backend Trait
// =============================================================================

/// Backend trait for email providers.
#[async_trait::async_trait]
pub trait EmailBackend: Send + Sync {
    /// Backend name for health checks and logging.
    fn name(&self) -> &'static str;

    /// Send a single email.
    async fn send(&self, email: &Email) -> Result<EmailResult>;

    /// Send multiple emails (batch).
    async fn send_batch(&self, emails: &[Email]) -> Result<Vec<EmailResult>> {
        let mut results = Vec::with_capacity(emails.len());
        for email in emails {
            results.push(self.send(email).await?);
        }
        Ok(results)
    }

    /// Health check.
    async fn health(&self) -> Result<()>;
}

// =============================================================================
// Memory Backend (for testing)
// =============================================================================

/// In-memory email backend (for testing).
#[derive(Default)]
pub struct MemoryBackend {
    sent: RwLock<Vec<Email>>,
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBackend")
            .field("sent", &"<RwLock<Vec<Email>>>")
            .finish()
    }
}

impl MemoryBackend {
    /// Create a new in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all sent emails (for testing).
    pub async fn sent_emails(&self) -> Vec<Email> {
        self.sent.read().await.clone()
    }

    /// Clear sent emails.
    pub async fn clear(&self) {
        self.sent.write().await.clear();
    }
}

#[async_trait::async_trait]
impl EmailBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    async fn send(&self, email: &Email) -> Result<EmailResult> {
        let message_id = uuid::Uuid::new_v4().to_string();
        self.sent.write().await.push(email.clone());
        Ok(EmailResult::success(Some(message_id)))
    }

    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

// =============================================================================
// SMTP Backend (using lettre)
// =============================================================================

#[cfg(feature = "email-smtp")]
mod smtp {
    use super::{Email, EmailBackend, EmailResult, Error, Result, info};
    use lettre::{
        AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
        message::{Mailbox, MultiPart, SinglePart, header::ContentType},
        transport::smtp::authentication::Credentials,
    };

    /// SMTP email backend using `lettre`.
    pub struct SmtpBackend {
        transport: AsyncSmtpTransport<Tokio1Executor>,
    }

    impl std::fmt::Debug for SmtpBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SmtpBackend")
                .field("transport", &"<AsyncSmtpTransport>")
                .finish()
        }
    }

    impl SmtpBackend {
        /// Create from URL: `smtp://user:pass@host:port` or `smtps://user:pass@host:port`
        ///
        /// For plain SMTP without TLS (e.g., Mailpit testing), use port 25 or 1025:
        /// `smtp://localhost:1025`
        pub fn from_url(url: &str) -> Result<Self> {
            let parsed = url::Url::parse(url)
                .map_err(|e| Error::Config(format!("Invalid SMTP URL: {e}")))?;

            let host = parsed.host_str().unwrap_or("localhost");
            let port = parsed
                .port()
                .unwrap_or(if parsed.scheme() == "smtps" { 465 } else { 587 });
            let username = parsed.username();
            let password = parsed.password().unwrap_or("");

            // Determine transport type based on scheme and port
            // Ports 25, 1025, 2525 are typically plain SMTP (no TLS)
            // Port 465 is SMTPS (implicit TLS)
            // Port 587 is submission (STARTTLS)
            let is_plain = matches!(port, 25 | 1025 | 2525);

            let transport = if parsed.scheme() == "smtps" {
                // Implicit TLS (port 465)
                let builder = AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                    .map_err(|e| Error::Config(format!("SMTP relay error: {e}")))?
                    .port(port);
                if username.is_empty() {
                    builder.build()
                } else {
                    builder
                        .credentials(Credentials::new(username.to_string(), password.to_string()))
                        .build()
                }
            } else if is_plain {
                // Plain SMTP (no TLS) - for local testing like Mailpit
                let builder =
                    AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host).port(port);
                if username.is_empty() {
                    builder.build()
                } else {
                    builder
                        .credentials(Credentials::new(username.to_string(), password.to_string()))
                        .build()
                }
            } else {
                // STARTTLS (port 587)
                let builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                    .map_err(|e| Error::Config(format!("SMTP STARTTLS error: {e}")))?
                    .port(port);
                if username.is_empty() {
                    builder.build()
                } else {
                    builder
                        .credentials(Credentials::new(username.to_string(), password.to_string()))
                        .build()
                }
            };

            let mode = if parsed.scheme() == "smtps" {
                "TLS"
            } else if is_plain {
                "plain"
            } else {
                "STARTTLS"
            };
            info!(host = %host, port = %port, mode = %mode, "Connected to SMTP server");

            Ok(Self { transport })
        }

        fn build_message(email: &Email) -> Result<Message> {
            let from: Mailbox = email
                .from
                .parse()
                .map_err(|e| Error::BadRequest(format!("Invalid from address: {e}")))?;

            let mut builder = Message::builder().from(from);

            for to in &email.to {
                let mailbox: Mailbox = to
                    .parse()
                    .map_err(|e| Error::BadRequest(format!("Invalid to address: {e}")))?;
                builder = builder.to(mailbox);
            }

            for cc in &email.cc {
                let mailbox: Mailbox = cc
                    .parse()
                    .map_err(|e| Error::BadRequest(format!("Invalid cc address: {e}")))?;
                builder = builder.cc(mailbox);
            }

            for bcc in &email.bcc {
                let mailbox: Mailbox = bcc
                    .parse()
                    .map_err(|e| Error::BadRequest(format!("Invalid bcc address: {e}")))?;
                builder = builder.bcc(mailbox);
            }

            if let Some(ref reply_to) = email.reply_to {
                let mailbox: Mailbox = reply_to
                    .parse()
                    .map_err(|e| Error::BadRequest(format!("Invalid reply-to address: {e}")))?;
                builder = builder.reply_to(mailbox);
            }

            builder = builder.subject(&email.subject);

            // Build body
            let message = match (&email.text, &email.html) {
                (Some(text), Some(html)) => builder.multipart(
                    MultiPart::alternative()
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_PLAIN)
                                .body(text.clone()),
                        )
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_HTML)
                                .body(html.clone()),
                        ),
                ),
                (Some(text), None) => builder.body(text.clone()),
                (None, Some(html)) => builder.header(ContentType::TEXT_HTML).body(html.clone()),
                (None, None) => builder.body(String::new()),
            }
            .map_err(|e| Error::Internal(format!("Failed to build email: {e}")))?;

            Ok(message)
        }
    }

    #[async_trait::async_trait]
    impl EmailBackend for SmtpBackend {
        fn name(&self) -> &'static str {
            "smtp"
        }

        async fn send(&self, email: &Email) -> Result<EmailResult> {
            let message = Self::build_message(email)?;

            match self.transport.send(message).await {
                Ok(response) => {
                    // Get first message from response as message ID
                    let message_id = response.message().next().map(ToString::to_string);
                    Ok(EmailResult::success(message_id))
                }
                Err(e) => Ok(EmailResult::failure(e.to_string())),
            }
        }

        async fn health(&self) -> Result<()> {
            self.transport
                .test_connection()
                .await
                .map_err(|e| Error::Internal(format!("SMTP health check failed: {e}")))?;
            Ok(())
        }
    }
}

#[cfg(feature = "email-smtp")]
pub use smtp::SmtpBackend;

// =============================================================================
// Email Service
// =============================================================================

/// Email service with pluggable backends.
#[derive(Clone)]
pub struct EmailService {
    backend: Arc<dyn EmailBackend>,
}

impl std::fmt::Debug for EmailService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailService")
            .field("backend", &"<EmailBackend>")
            .finish()
    }
}

impl EmailService {
    /// Create a new email service with the given backend.
    pub fn new(backend: impl EmailBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Create email service from environment configuration.
    ///
    /// Reads `EMAIL_URL` environment variable.
    #[cfg(feature = "email-smtp")]
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("EMAIL_URL")
            .map_err(|_| Error::Config("EMAIL_URL environment variable not set".to_string()))?;

        Self::from_url(&url)
    }

    /// Create email service from a URL.
    ///
    /// Supported URL schemes:
    /// - `memory://` - In-memory (testing)
    /// - `smtp://user:pass@host:port` - SMTP (STARTTLS on port 587, plain on 25/1025)
    /// - `smtps://user:pass@host:port` - SMTP over TLS (port 465)
    ///
    /// SMTP works with any email provider:
    /// - Gmail: `smtps://user:app-password@smtp.gmail.com:465`
    /// - `SendGrid`: `smtps://apikey:SG.xxx@smtp.sendgrid.net:465`
    /// - AWS SES: `smtps://AKIA...:secret@email-smtp.us-east-1.amazonaws.com:465`
    /// - Resend: `smtps://resend:re_xxx@smtp.resend.com:465`
    /// - Mailpit (local): `smtp://localhost:1025`
    #[cfg(feature = "email-smtp")]
    pub fn from_url(url: &str) -> Result<Self> {
        info!(url = %url, "Initializing email backend");

        if url.starts_with("memory://") {
            return Ok(Self::new(MemoryBackend::new()));
        }

        if url.starts_with("smtp://") || url.starts_with("smtps://") {
            return Ok(Self::new(SmtpBackend::from_url(url)?));
        }

        Err(Error::Config(format!(
            "Unknown email URL scheme: {url}. Supported: memory://, smtp://, smtps://"
        )))
    }

    /// Create an in-memory email service (for testing).
    #[must_use]
    pub fn in_memory() -> Self {
        Self::new(MemoryBackend::new())
    }

    /// Create a health check for this service.
    #[must_use]
    pub fn health_checker(&self) -> HealthCheck {
        let backend = self.backend.clone();
        HealthCheck::with_async("email", move || {
            let backend = backend.clone();
            async move {
                match backend.health().await {
                    Ok(()) => HealthResult::healthy(),
                    Err(e) => {
                        tracing::warn!(error = %e, "Email health check failed");
                        HealthResult::unhealthy(e.to_string())
                    }
                }
            }
        })
    }
}

impl Sidecar for EmailService {
    fn name(&self) -> &'static str {
        "email"
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/send", post(send_email))
            .route("/send/batch", post(send_batch))
            .with_state(self.backend.clone())
    }
}

// =============================================================================
// HTTP Handlers
// =============================================================================

/// Send a single email.
async fn send_email(
    State(backend): State<Arc<dyn EmailBackend>>,
    Json(email): Json<Email>,
) -> Result<impl IntoResponse> {
    let result = backend.send(&email).await?;

    let status = if result.success {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };

    // Return a simplified response with id field
    let response = if result.success {
        serde_json::json!({
            "id": result.message_id,
            "success": true
        })
    } else {
        serde_json::json!({
            "success": false,
            "error": result.error
        })
    };

    Ok((status, Json(response)))
}

/// Batch request format.
#[derive(Debug, Deserialize)]
struct BatchRequest {
    emails: Vec<Email>,
}

/// Send multiple emails.
async fn send_batch(
    State(backend): State<Arc<dyn EmailBackend>>,
    Json(request): Json<BatchRequest>,
) -> Result<impl IntoResponse> {
    let results = backend.send_batch(&request.emails).await?;

    let sent = results.iter().filter(|r| r.success).count();
    let failed = results.iter().filter(|r| !r.success).count();

    Ok(Json(serde_json::json!({
        "sent": sent,
        "failed": failed,
        "results": results
    })))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_backend_send() {
        let backend = MemoryBackend::new();

        let email = Email {
            from: "sender@example.com".to_string(),
            to: vec!["recipient@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Test Subject".to_string(),
            text: Some("Hello, World!".to_string()),
            html: None,
            reply_to: None,
            headers: HashMap::new(),
        };

        let result = backend.send(&email).await.unwrap();
        assert!(result.success);
        assert!(result.message_id.is_some());

        let sent = backend.sent_emails().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].subject, "Test Subject");
    }

    #[tokio::test]
    async fn test_memory_backend_batch() {
        let backend = MemoryBackend::new();

        let emails = vec![
            Email {
                from: "sender@example.com".to_string(),
                to: vec!["recipient1@example.com".to_string()],
                cc: vec![],
                bcc: vec![],
                subject: "Email 1".to_string(),
                text: Some("Body 1".to_string()),
                html: None,
                reply_to: None,
                headers: HashMap::new(),
            },
            Email {
                from: "sender@example.com".to_string(),
                to: vec!["recipient2@example.com".to_string()],
                cc: vec![],
                bcc: vec![],
                subject: "Email 2".to_string(),
                text: Some("Body 2".to_string()),
                html: None,
                reply_to: None,
                headers: HashMap::new(),
            },
        ];

        let results = backend.send_batch(&emails).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.success));

        let sent = backend.sent_emails().await;
        assert_eq!(sent.len(), 2);
    }

    #[tokio::test]
    async fn test_email_service_in_memory() {
        let service = EmailService::in_memory();
        assert_eq!(service.name(), "email");

        let _router = service.router();
    }
}
