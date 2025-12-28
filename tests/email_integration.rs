//! Email backend integration tests.
//!
//! These tests require Docker services to be running:
//! ```bash
//! docker-compose up -d mailpit
//! ```
//!
//! Run these tests with:
//! ```bash
//! cargo test --test email_integration -- --ignored
//! ```

use std::time::Duration;

/// Helper to check if Mailpit is available
async fn is_mailpit_available() -> bool {
    let client = reqwest::Client::new();
    match tokio::time::timeout(
        Duration::from_secs(2),
        client.get("http://localhost:8025/api/v1/messages").send(),
    )
    .await
    {
        Ok(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

// ============================================================================
// SMTP tests via Mailpit (all platforms)
// ============================================================================

mod smtp_email {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use mikcar::Sidecar;
    use mikcar::email::EmailService;
    use tower::ServiceExt;

    // Mailpit SMTP - no auth required
    const MAILPIT_URL: &str = "smtp://localhost:1025";

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d mailpit"]
    async fn test_smtp_email_service_creation() {
        if !is_mailpit_available().await {
            eprintln!("Skipping: Mailpit not available at localhost:8025");
            return;
        }

        let service =
            EmailService::from_url(MAILPIT_URL).expect("Failed to create SMTP email service");

        assert_eq!(service.name(), "email");
        println!("SMTP email service created successfully");
    }

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d mailpit"]
    async fn test_smtp_send_email() {
        if !is_mailpit_available().await {
            eprintln!("Skipping: Mailpit not available at localhost:8025");
            return;
        }

        let service =
            EmailService::from_url(MAILPIT_URL).expect("Failed to create SMTP email service");

        let router = service.router();

        // Send an email
        let send_request = Request::builder()
            .method("POST")
            .uri("/send")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                "from": "test@example.com",
                "to": ["recipient@example.com"],
                "subject": "Test Email from mikcar",
                "text": "This is a test email sent via Mailpit.",
                "html": "<h1>Test</h1><p>This is a test email sent via Mailpit.</p>"
            }"#,
            ))
            .unwrap();

        let response = router.clone().oneshot(send_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("id").is_some());
        println!("SMTP send succeeded: {:?}", json);

        // Verify email was received in Mailpit
        tokio::time::sleep(Duration::from_millis(500)).await;
        let client = reqwest::Client::new();
        let mailpit_response = client
            .get("http://localhost:8025/api/v1/messages")
            .send()
            .await
            .expect("Failed to query Mailpit");

        let mailpit_json: serde_json::Value = mailpit_response.json().await.unwrap();
        let messages = mailpit_json["messages"].as_array().unwrap();

        // Find our test email
        let found = messages
            .iter()
            .any(|msg| msg["Subject"].as_str() == Some("Test Email from mikcar"));

        assert!(found, "Test email not found in Mailpit");
        println!("Email verified in Mailpit inbox");
    }

    #[tokio::test]
    #[ignore = "requires Docker: docker-compose up -d mailpit"]
    async fn test_smtp_batch_send() {
        if !is_mailpit_available().await {
            eprintln!("Skipping: Mailpit not available at localhost:8025");
            return;
        }

        let service =
            EmailService::from_url(MAILPIT_URL).expect("Failed to create SMTP email service");

        let router = service.router();

        // Send batch of emails
        let batch_request = Request::builder()
            .method("POST")
            .uri("/send/batch")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                "emails": [
                    {
                        "from": "batch@example.com",
                        "to": ["user1@example.com"],
                        "subject": "Batch Email 1",
                        "text": "First batch email"
                    },
                    {
                        "from": "batch@example.com",
                        "to": ["user2@example.com"],
                        "subject": "Batch Email 2",
                        "text": "Second batch email"
                    }
                ]
            }"#,
            ))
            .unwrap();

        let response = router.oneshot(batch_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["sent"], 2);
        assert_eq!(json["failed"], 0);
        println!("SMTP batch send succeeded: {:?}", json);
    }
}

// ============================================================================
// In-memory tests (all platforms)
// ============================================================================

mod memory_email {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use mikcar::Sidecar;
    use mikcar::email::EmailService;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_memory_email_send() {
        let service =
            EmailService::from_url("memory://").expect("Failed to create in-memory email service");

        let router = service.router();

        // Send an email
        let send_request = Request::builder()
            .method("POST")
            .uri("/send")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                "from": "test@example.com",
                "to": ["recipient@example.com"],
                "subject": "Test Email",
                "text": "This is a test email."
            }"#,
            ))
            .unwrap();

        let response = router.clone().oneshot(send_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("id").is_some());
        println!("Memory email send succeeded: {:?}", json);
    }

    #[tokio::test]
    async fn test_memory_email_batch() {
        let service =
            EmailService::from_url("memory://").expect("Failed to create in-memory email service");

        let router = service.router();

        // Send batch
        let batch_request = Request::builder()
            .method("POST")
            .uri("/send/batch")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                "emails": [
                    {
                        "from": "test@example.com",
                        "to": ["user1@example.com"],
                        "subject": "Email 1",
                        "text": "First email"
                    },
                    {
                        "from": "test@example.com",
                        "to": ["user2@example.com"],
                        "subject": "Email 2",
                        "text": "Second email"
                    },
                    {
                        "from": "test@example.com",
                        "to": ["user3@example.com"],
                        "subject": "Email 3",
                        "text": "Third email"
                    }
                ]
            }"#,
            ))
            .unwrap();

        let response = router.oneshot(batch_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["sent"], 3);
        assert_eq!(json["failed"], 0);
        println!("Memory email batch send succeeded: {:?}", json);
    }

    #[tokio::test]
    async fn test_memory_email_with_all_fields() {
        let service =
            EmailService::from_url("memory://").expect("Failed to create in-memory email service");

        let router = service.router();

        // Send an email with all fields populated
        let send_request = Request::builder()
            .method("POST")
            .uri("/send")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                "from": "sender@example.com",
                "to": ["recipient1@example.com", "recipient2@example.com"],
                "cc": ["cc@example.com"],
                "bcc": ["bcc@example.com"],
                "subject": "Full Featured Email",
                "text": "Plain text version",
                "html": "<h1>HTML Version</h1>",
                "reply_to": "reply@example.com",
                "headers": {"X-Custom-Header": "custom-value"}
            }"#,
            ))
            .unwrap();

        let response = router.oneshot(send_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("id").is_some());
        println!("Memory email with all fields succeeded: {:?}", json);
    }
}
