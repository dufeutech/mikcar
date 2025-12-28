//! Secrets Manager service.
//!
//! Provides a unified HTTP API for secrets management backends:
//! - `HashiCorp` Vault
//! - AWS Secrets Manager
//! - GCP Secret Manager
//! - Environment variables (dev/testing)
//! - File-based (encrypted JSON, dev/testing)

use crate::{Error, HealthCheck, HealthResult, Result, Sidecar};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use base64::Engine;
use tracing::{info, warn};

// ============================================================================
// Types
// ============================================================================

/// Secret value with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secret {
    /// The secret value.
    pub value: String,
    /// Version identifier.
    pub version: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
    /// Optional labels/tags.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Secret info (without value) for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretInfo {
    /// Secret name/path.
    pub name: String,
    /// Version identifier.
    pub version: String,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
}

/// Request to create or update a secret.
#[derive(Debug, Deserialize)]
pub struct SecretRequest {
    /// The secret value.
    pub value: String,
    /// Optional labels/tags.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

// ============================================================================
// Backend Trait
// ============================================================================

/// Backend trait for secrets storage.
#[async_trait::async_trait]
pub trait SecretsBackend: Send + Sync {
    /// Get a secret by name.
    async fn get(&self, name: &str) -> Result<Option<Secret>>;

    /// Create a new secret.
    async fn create(&self, name: &str, value: &str, labels: HashMap<String, String>) -> Result<()>;

    /// Update an existing secret.
    async fn update(&self, name: &str, value: &str) -> Result<()>;

    /// Delete a secret.
    async fn delete(&self, name: &str) -> Result<bool>;

    /// List secrets (names only, optionally filtered by prefix).
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretInfo>>;

    /// Check if a secret exists.
    async fn exists(&self, name: &str) -> Result<bool>;

    /// Health check.
    async fn health(&self) -> Result<()>;
}

// ============================================================================
// In-Memory Backend (for testing/development)
// ============================================================================

/// In-memory secrets backend (for testing).
#[derive(Default)]
pub struct MemoryBackend {
    secrets: RwLock<HashMap<String, Secret>>,
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBackend")
            .field("secrets", &"<RwLock<HashMap>>")
            .finish()
    }
}

impl MemoryBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SecretsBackend for MemoryBackend {
    async fn get(&self, name: &str) -> Result<Option<Secret>> {
        let secrets = self.secrets.read().await;
        Ok(secrets.get(name).cloned())
    }

    async fn create(&self, name: &str, value: &str, labels: HashMap<String, String>) -> Result<()> {
        let mut secrets = self.secrets.write().await;

        if secrets.contains_key(name) {
            return Err(Error::BadRequest(format!("Secret '{name}' already exists")));
        }

        let now = chrono::Utc::now().to_rfc3339();
        let secret = Secret {
            value: value.to_string(),
            version: "1".to_string(),
            created_at: now.clone(),
            updated_at: now,
            labels,
        };

        secrets.insert(name.to_string(), secret);
        Ok(())
    }

    async fn update(&self, name: &str, value: &str) -> Result<()> {
        let mut secrets = self.secrets.write().await;

        let secret = secrets
            .get_mut(name)
            .ok_or_else(|| Error::NotFound(format!("Secret '{name}' not found")))?;

        secret.value = value.to_string();
        secret.updated_at = chrono::Utc::now().to_rfc3339();
        // Increment version
        let version: u64 = secret.version.parse().unwrap_or(0);
        secret.version = (version + 1).to_string();

        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let mut secrets = self.secrets.write().await;
        Ok(secrets.remove(name).is_some())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretInfo>> {
        let secrets = self.secrets.read().await;

        let infos: Vec<SecretInfo> = secrets
            .iter()
            .filter(|(k, _)| {
                prefix.is_none_or(|p| k.starts_with(p))
            })
            .map(|(k, v)| SecretInfo {
                name: k.clone(),
                version: v.version.clone(),
                created_at: v.created_at.clone(),
                updated_at: v.updated_at.clone(),
            })
            .collect();

        Ok(infos)
    }

    async fn exists(&self, name: &str) -> Result<bool> {
        let secrets = self.secrets.read().await;
        Ok(secrets.contains_key(name))
    }

    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

// ============================================================================
// Environment Backend (read-only, for testing)
// ============================================================================

/// Environment variables backend (read-only).
///
/// Reads secrets from environment variables with a configurable prefix.
/// Example: `SECRETS_DB_PASSWORD` with prefix `SECRETS_` → secret name `db_password`
#[derive(Debug)]
pub struct EnvBackend {
    prefix: String,
}

impl EnvBackend {
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl SecretsBackend for EnvBackend {
    async fn get(&self, name: &str) -> Result<Option<Secret>> {
        let env_name = format!("{}{}", self.prefix, name.to_uppercase().replace('/', "_"));

        match std::env::var(&env_name) {
            Ok(value) => Ok(Some(Secret {
                value,
                version: "env".to_string(),
                created_at: "N/A".to_string(),
                updated_at: "N/A".to_string(),
                labels: HashMap::new(),
            })),
            Err(_) => Ok(None),
        }
    }

    async fn create(&self, _name: &str, _value: &str, _labels: HashMap<String, String>) -> Result<()> {
        Err(Error::BadRequest("Environment backend is read-only".to_string()))
    }

    async fn update(&self, _name: &str, _value: &str) -> Result<()> {
        Err(Error::BadRequest("Environment backend is read-only".to_string()))
    }

    async fn delete(&self, _name: &str) -> Result<bool> {
        Err(Error::BadRequest("Environment backend is read-only".to_string()))
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretInfo>> {
        let mut infos = Vec::new();

        for (key, _) in std::env::vars() {
            if key.starts_with(&self.prefix) {
                let name = key
                    .strip_prefix(&self.prefix)
                    .unwrap_or(&key)
                    .to_lowercase()
                    .replace('_', "/");

                if prefix.is_none_or(|p| name.starts_with(p)) {
                    infos.push(SecretInfo {
                        name,
                        version: "env".to_string(),
                        created_at: "N/A".to_string(),
                        updated_at: "N/A".to_string(),
                    });
                }
            }
        }

        Ok(infos)
    }

    async fn exists(&self, name: &str) -> Result<bool> {
        let env_name = format!("{}{}", self.prefix, name.to_uppercase().replace('/', "_"));
        Ok(std::env::var(&env_name).is_ok())
    }

    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

// ============================================================================
// HashiCorp Vault Backend
// ============================================================================

/// `HashiCorp` Vault backend.
///
/// Connects to Vault using the HTTP API.
/// URL format: `vault://host:port?token=TOKEN&mount=secret`
pub struct VaultBackend {
    client: reqwest::Client,
    base_url: String,
    token: String,
    mount: String,
}

impl std::fmt::Debug for VaultBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultBackend")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .field("mount", &self.mount)
            .finish_non_exhaustive()
    }
}

impl VaultBackend {
    /// Create a new Vault backend from a URL.
    ///
    /// URL format: `vault://host:port?token=TOKEN&mount=secret`
    /// - `token` - Vault token (can also be set via `VAULT_TOKEN` env var)
    /// - `mount` - KV secrets engine mount point (default: `secret`)
    pub fn from_url(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Config(format!("Invalid Vault URL: {e}")))?;

        let host = parsed.host_str().unwrap_or("localhost");
        let port = parsed.port().unwrap_or(8200);

        // Parse query params
        let mut token = None;
        let mut mount = "secret".to_string();

        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "token" => token = Some(value.to_string()),
                "mount" => mount = value.to_string(),
                _ => {}
            }
        }

        // Token can come from URL or environment
        let token = token
            .or_else(|| std::env::var("VAULT_TOKEN").ok())
            .ok_or_else(|| {
                Error::Config("Vault token not provided. Set ?token=... in URL or VAULT_TOKEN env var".to_string())
            })?;

        let base_url = format!("http://{host}:{port}");

        info!(base_url = %base_url, mount = %mount, "Connecting to Vault");

        Ok(Self {
            client: reqwest::Client::new(),
            base_url,
            token,
            mount,
        })
    }
}

#[async_trait::async_trait]
impl SecretsBackend for VaultBackend {
    async fn get(&self, name: &str) -> Result<Option<Secret>> {
        let url = format!("{}/v1/{}/data/{}", self.base_url, self.mount, name);

        let response = self.client
            .get(&url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("Vault request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("Vault error {status}: {body}")));
        }

        let body: serde_json::Value = response.json().await
            .map_err(|e| Error::Internal(format!("Failed to parse Vault response: {e}")))?;

        // Vault KV v2 response structure:
        // { "data": { "data": { ... }, "metadata": { ... } } }
        let data = body
            .get("data")
            .and_then(|d| d.get("data"))
            .ok_or_else(|| Error::Internal("Invalid Vault response structure".to_string()))?;

        let metadata = body
            .get("data")
            .and_then(|d| d.get("metadata"));

        // Extract value - if there's a single "value" key, use it; otherwise serialize all data
        let value = if let Some(v) = data.get("value") {
            v.as_str().unwrap_or_default().to_string()
        } else {
            serde_json::to_string(data).unwrap_or_default()
        };

        let version = metadata
            .and_then(|m| m.get("version"))
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "1".to_string(), |v| v.to_string());

        let created_at = metadata
            .and_then(|m| m.get("created_time"))
            .and_then(|v| v.as_str())
            .unwrap_or("N/A")
            .to_string();

        Ok(Some(Secret {
            value,
            version,
            created_at: created_at.clone(),
            updated_at: created_at,
            labels: HashMap::new(),
        }))
    }

    async fn create(&self, name: &str, value: &str, _labels: HashMap<String, String>) -> Result<()> {
        let url = format!("{}/v1/{}/data/{}", self.base_url, self.mount, name);

        let body = serde_json::json!({
            "data": {
                "value": value
            }
        });

        let response = self.client
            .post(&url)
            .header("X-Vault-Token", &self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("Vault request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("Vault error {status}: {body}")));
        }

        Ok(())
    }

    async fn update(&self, name: &str, value: &str) -> Result<()> {
        // In Vault KV v2, create and update are the same operation
        self.create(name, value, HashMap::new()).await
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let url = format!("{}/v1/{}/data/{}", self.base_url, self.mount, name);

        let response = self.client
            .delete(&url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("Vault request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("Vault error {status}: {body}")));
        }

        Ok(true)
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretInfo>> {
        let path = prefix.unwrap_or("");
        let url = format!("{}/v1/{}/metadata/{}", self.base_url, self.mount, path);

        let response = self.client
            .request(reqwest::Method::from_bytes(b"LIST").unwrap(), &url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("Vault request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("Vault error {status}: {body}")));
        }

        let body: serde_json::Value = response.json().await
            .map_err(|e| Error::Internal(format!("Failed to parse Vault response: {e}")))?;

        let keys = body
            .get("data")
            .and_then(|d| d.get("keys"))
            .and_then(|k| k.as_array())
            .cloned()
            .unwrap_or_default();

        let infos: Vec<SecretInfo> = keys
            .into_iter()
            .filter_map(|k| k.as_str().map(String::from))
            .map(|name| SecretInfo {
                name: format!("{path}{name}"),
                version: "N/A".to_string(),
                created_at: "N/A".to_string(),
                updated_at: "N/A".to_string(),
            })
            .collect();

        Ok(infos)
    }

    async fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.get(name).await?.is_some())
    }

    async fn health(&self) -> Result<()> {
        let url = format!("{}/v1/sys/health", self.base_url);

        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("Vault health check failed: {e}")))?;

        // Vault returns 200 for healthy, 429/472/473/501/503 for various states
        if response.status().is_success() || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            Ok(())
        } else {
            Err(Error::Internal(format!("Vault unhealthy: status {}", response.status())))
        }
    }
}

// ============================================================================
// AWS Secrets Manager Backend
// ============================================================================

/// AWS Secrets Manager backend.
///
/// Connects to AWS Secrets Manager using the HTTP API.
/// URL format: `aws-sm://region?endpoint=http://localhost:4566` (endpoint optional, for `LocalStack`)
///
/// Authentication uses standard AWS credential chain:
/// - Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
/// - Shared credentials file (~/.aws/credentials)
/// - IAM role (when running on AWS)
pub struct AwsSecretsManagerBackend {
    client: reqwest::Client,
    region: String,
    endpoint: String,
    access_key_id: String,
    secret_access_key: String,
}

impl std::fmt::Debug for AwsSecretsManagerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsSecretsManagerBackend")
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl AwsSecretsManagerBackend {
    /// Create a new AWS Secrets Manager backend from a URL.
    ///
    /// URL format: `aws-sm://region?endpoint=http://localhost:4566`
    /// - `region` - AWS region (e.g., us-east-1)
    /// - `endpoint` - Optional custom endpoint (for LocalStack/testing)
    pub fn from_url(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Config(format!("Invalid AWS SM URL: {e}")))?;

        let region = parsed.host_str().unwrap_or("us-east-1").to_string();

        // Parse query params
        let mut endpoint = None;

        for (key, value) in parsed.query_pairs() {
            if key == "endpoint" {
                endpoint = Some(value.to_string());
            }
        }

        // Default endpoint or custom (for LocalStack)
        let endpoint = endpoint.unwrap_or_else(|| {
            format!("https://secretsmanager.{region}.amazonaws.com")
        });

        // Get credentials from environment
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
            Error::Config("AWS_ACCESS_KEY_ID environment variable not set".to_string())
        })?;

        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
            Error::Config("AWS_SECRET_ACCESS_KEY environment variable not set".to_string())
        })?;

        info!(region = %region, endpoint = %endpoint, "Connecting to AWS Secrets Manager");

        Ok(Self {
            client: reqwest::Client::new(),
            region,
            endpoint,
            access_key_id,
            secret_access_key,
        })
    }

    /// Sign a request with AWS Signature Version 4.
    #[allow(clippy::unnecessary_wraps)] // Keeps consistent API for potential future error cases
    fn sign_request(
        &self,
        method: &str,
        body: &str,
        target: &str,
    ) -> Result<Vec<(String, String)>> {
        use chrono::Utc;

        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        let host = self.endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("secretsmanager.us-east-1.amazonaws.com");

        // Create canonical request
        let canonical_uri = "/";
        let canonical_querystring = "";
        let payload_hash = sha256_hex(body);

        let canonical_headers = format!(
            "content-type:application/x-amz-json-1.1\nhost:{host}\nx-amz-date:{amz_date}\nx-amz-target:{target}\n"
        );
        let signed_headers = "content-type;host;x-amz-date;x-amz-target";

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_querystring}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );

        // Create string to sign
        let algorithm = "AWS4-HMAC-SHA256";
        let credential_scope = format!("{date_stamp}/{}/secretsmanager/aws4_request", self.region);
        let string_to_sign = format!(
            "{algorithm}\n{amz_date}\n{credential_scope}\n{}",
            sha256_hex(&canonical_request)
        );

        // Calculate signature
        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"secretsmanager");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        let authorization = format!(
            "{algorithm} Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key_id
        );

        Ok(vec![
            ("Content-Type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("X-Amz-Date".to_string(), amz_date),
            ("X-Amz-Target".to_string(), target.to_string()),
            ("Authorization".to_string(), authorization),
        ])
    }
}

/// SHA-256 hash as hex string.
fn sha256_hex(data: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// HMAC-SHA256.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[async_trait::async_trait]
impl SecretsBackend for AwsSecretsManagerBackend {
    async fn get(&self, name: &str) -> Result<Option<Secret>> {
        let body = serde_json::json!({
            "SecretId": name
        }).to_string();

        let headers = self.sign_request("POST", &body, "secretsmanager.GetSecretValue")?;

        let mut req = self.client.post(&self.endpoint);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let response = req
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("AWS SM request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND
            || response.status() == reqwest::StatusCode::BAD_REQUEST
        {
            // AWS returns 400 for ResourceNotFoundException
            let body = response.text().await.unwrap_or_default();
            if body.contains("ResourceNotFoundException") {
                return Ok(None);
            }
            return Err(Error::Internal(format!("AWS SM error: {body}")));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("AWS SM error {status}: {body}")));
        }

        let body: serde_json::Value = response.json().await
            .map_err(|e| Error::Internal(format!("Failed to parse AWS SM response: {e}")))?;

        let value = body
            .get("SecretString")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let version = body
            .get("VersionId")
            .and_then(|v| v.as_str())
            .unwrap_or("1")
            .to_string();

        #[allow(clippy::cast_possible_truncation)] // Timestamp is within i64 range
        let created_at = body
            .get("CreatedDate")
            .and_then(serde_json::Value::as_f64)
            .map_or_else(
                || "N/A".to_string(),
                |ts| {
                    chrono::DateTime::from_timestamp(ts as i64, 0)
                        .map_or_else(|| "N/A".to_string(), |dt| dt.to_rfc3339())
                },
            );

        Ok(Some(Secret {
            value,
            version,
            created_at: created_at.clone(),
            updated_at: created_at,
            labels: HashMap::new(),
        }))
    }

    async fn create(&self, name: &str, value: &str, _labels: HashMap<String, String>) -> Result<()> {
        let body = serde_json::json!({
            "Name": name,
            "SecretString": value,
            "ClientRequestToken": uuid::Uuid::new_v4().to_string()
        }).to_string();

        let headers = self.sign_request("POST", &body, "secretsmanager.CreateSecret")?;

        let mut req = self.client.post(&self.endpoint);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let response = req
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("AWS SM request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            if body.contains("ResourceExistsException") {
                return Err(Error::BadRequest(format!("Secret '{name}' already exists")));
            }

            return Err(Error::Internal(format!("AWS SM error {status}: {body}")));
        }

        Ok(())
    }

    async fn update(&self, name: &str, value: &str) -> Result<()> {
        let body = serde_json::json!({
            "SecretId": name,
            "SecretString": value,
            "ClientRequestToken": uuid::Uuid::new_v4().to_string()
        }).to_string();

        let headers = self.sign_request("POST", &body, "secretsmanager.PutSecretValue")?;

        let mut req = self.client.post(&self.endpoint);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let response = req
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("AWS SM request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            if body.contains("ResourceNotFoundException") {
                return Err(Error::NotFound(format!("Secret '{name}' not found")));
            }

            return Err(Error::Internal(format!("AWS SM error {status}: {body}")));
        }

        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let body = serde_json::json!({
            "SecretId": name,
            "ForceDeleteWithoutRecovery": true
        }).to_string();

        let headers = self.sign_request("POST", &body, "secretsmanager.DeleteSecret")?;

        let mut req = self.client.post(&self.endpoint);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let response = req
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("AWS SM request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            let body = response.text().await.unwrap_or_default();
            if body.contains("ResourceNotFoundException") {
                return Ok(false);
            }
            return Err(Error::Internal(format!("AWS SM error: {body}")));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("AWS SM error {status}: {body}")));
        }

        Ok(true)
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretInfo>> {
        let mut body_json = serde_json::json!({});

        if let Some(p) = prefix {
            body_json["Filters"] = serde_json::json!([{
                "Key": "name",
                "Values": [p]
            }]);
        }

        let body = body_json.to_string();
        let headers = self.sign_request("POST", &body, "secretsmanager.ListSecrets")?;

        let mut req = self.client.post(&self.endpoint);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let response = req
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("AWS SM request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("AWS SM error {status}: {body}")));
        }

        let body: serde_json::Value = response.json().await
            .map_err(|e| Error::Internal(format!("Failed to parse AWS SM response: {e}")))?;

        let secrets = body
            .get("SecretList")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let infos: Vec<SecretInfo> = secrets
            .into_iter()
            .filter_map(|s| {
                let name = s.get("Name")?.as_str()?.to_string();
                let version = s.get("VersionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("N/A")
                    .to_string();
                #[allow(clippy::cast_possible_truncation)] // Timestamp is within i64 range
                let created_at = s.get("CreatedDate")
                    .and_then(serde_json::Value::as_f64)
                    .map_or_else(
                        || "N/A".to_string(),
                        |ts| {
                            chrono::DateTime::from_timestamp(ts as i64, 0)
                                .map_or_else(|| "N/A".to_string(), |dt| dt.to_rfc3339())
                        },
                    );
                #[allow(clippy::cast_possible_truncation)] // Timestamp is within i64 range
                let updated_at = s.get("LastChangedDate")
                    .and_then(serde_json::Value::as_f64)
                    .map_or_else(
                        || created_at.clone(),
                        |ts| {
                            chrono::DateTime::from_timestamp(ts as i64, 0)
                                .map_or_else(|| "N/A".to_string(), |dt| dt.to_rfc3339())
                        },
                    );

                Some(SecretInfo {
                    name,
                    version,
                    created_at,
                    updated_at,
                })
            })
            .collect();

        Ok(infos)
    }

    async fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.get(name).await?.is_some())
    }

    async fn health(&self) -> Result<()> {
        // List secrets with max 1 result as health check
        let body = serde_json::json!({
            "MaxResults": 1
        }).to_string();

        let headers = self.sign_request("POST", &body, "secretsmanager.ListSecrets")?;

        let mut req = self.client.post(&self.endpoint);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        let response = req
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("AWS SM health check failed: {e}")))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::Internal(format!(
                "AWS SM unhealthy: status {}",
                response.status()
            )))
        }
    }
}

// ============================================================================
// GCP Secret Manager Backend
// ============================================================================

/// GCP Secret Manager backend.
///
/// Connects to GCP Secret Manager using the HTTP API.
/// URL format: `gcp-sm://project-id`
///
/// Authentication uses standard GCP credential chain:
/// - `GOOGLE_APPLICATION_CREDENTIALS` environment variable
/// - Default service account (when running on GCP)
pub struct GcpSecretManagerBackend {
    client: reqwest::Client,
    project_id: String,
    access_token: String,
}

impl std::fmt::Debug for GcpSecretManagerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpSecretManagerBackend")
            .field("project_id", &self.project_id)
            .field("access_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl GcpSecretManagerBackend {
    /// Create a new GCP Secret Manager backend from a URL.
    ///
    /// URL format: `gcp-sm://project-id`
    pub fn from_url(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Config(format!("Invalid GCP SM URL: {e}")))?;

        let project_id = parsed.host_str()
            .ok_or_else(|| Error::Config("GCP project ID required in URL".to_string()))?
            .to_string();

        // Get access token from environment or metadata server
        let access_token = std::env::var("GCP_ACCESS_TOKEN")
            .or_else(|_| std::env::var("GOOGLE_ACCESS_TOKEN"))
            .map_err(|_| {
                Error::Config(
                    "GCP access token not found. Set GCP_ACCESS_TOKEN or GOOGLE_ACCESS_TOKEN env var".to_string()
                )
            })?;

        info!(project_id = %project_id, "Connecting to GCP Secret Manager");

        Ok(Self {
            client: reqwest::Client::new(),
            project_id,
            access_token,
        })
    }

    fn base_url(&self) -> String {
        format!(
            "https://secretmanager.googleapis.com/v1/projects/{}/secrets",
            self.project_id
        )
    }
}

#[async_trait::async_trait]
impl SecretsBackend for GcpSecretManagerBackend {
    async fn get(&self, name: &str) -> Result<Option<Secret>> {
        // GCP Secret Manager requires accessing the latest version
        let url = format!(
            "{}/{}/versions/latest:access",
            self.base_url(),
            name
        );

        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("GCP SM error {status}: {body}")));
        }

        let body: serde_json::Value = response.json().await
            .map_err(|e| Error::Internal(format!("Failed to parse GCP SM response: {e}")))?;

        // GCP returns base64-encoded payload
        let payload = body
            .get("payload")
            .and_then(|p| p.get("data"))
            .and_then(|d| d.as_str())
            .unwrap_or_default();

        // Decode base64
        let value = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            .unwrap_or_default();

        let version = body
            .get("name")
            .and_then(|n| n.as_str())
            .and_then(|n| n.rsplit('/').next())
            .unwrap_or("1")
            .to_string();

        Ok(Some(Secret {
            value,
            version,
            created_at: "N/A".to_string(),
            updated_at: "N/A".to_string(),
            labels: HashMap::new(),
        }))
    }

    async fn create(&self, name: &str, value: &str, _labels: HashMap<String, String>) -> Result<()> {
        // Step 1: Create the secret (metadata)
        let create_url = format!("{}?secretId={}", self.base_url(), name);

        let create_body = serde_json::json!({
            "replication": {
                "automatic": {}
            }
        });

        let create_response = self.client
            .post(&create_url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&create_body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        if !create_response.status().is_success() {
            let status = create_response.status();
            let body = create_response.text().await.unwrap_or_default();

            if body.contains("ALREADY_EXISTS") {
                return Err(Error::BadRequest(format!("Secret '{name}' already exists")));
            }

            return Err(Error::Internal(format!("GCP SM error {status}: {body}")));
        }

        // Step 2: Add a secret version with the value
        let version_url = format!("{}/{}/versions:add", self.base_url(), name);

        let encoded_value = base64::engine::general_purpose::STANDARD.encode(value);

        let version_body = serde_json::json!({
            "payload": {
                "data": encoded_value
            }
        });

        let version_response = self.client
            .post(&version_url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&version_body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        if !version_response.status().is_success() {
            let status = version_response.status();
            let body = version_response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("GCP SM error {status}: {body}")));
        }

        Ok(())
    }

    async fn update(&self, name: &str, value: &str) -> Result<()> {
        // In GCP, updating means adding a new version
        let version_url = format!("{}/{}/versions:add", self.base_url(), name);

        let encoded_value = base64::engine::general_purpose::STANDARD.encode(value);

        let body = serde_json::json!({
            "payload": {
                "data": encoded_value
            }
        });

        let response = self.client
            .post(&version_url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("Secret '{name}' not found")));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("GCP SM error {status}: {body}")));
        }

        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let url = format!("{}/{}", self.base_url(), name);

        let response = self.client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("GCP SM error {status}: {body}")));
        }

        Ok(true)
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretInfo>> {
        let mut url = self.base_url();

        if let Some(p) = prefix {
            url = format!("{url}?filter=name:{p}");
        }

        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Internal(format!("GCP SM error {status}: {body}")));
        }

        let body: serde_json::Value = response.json().await
            .map_err(|e| Error::Internal(format!("Failed to parse GCP SM response: {e}")))?;

        let secrets = body
            .get("secrets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let infos: Vec<SecretInfo> = secrets
            .into_iter()
            .filter_map(|s| {
                let full_name = s.get("name")?.as_str()?;
                // Extract just the secret name from the full resource path
                let name = full_name.rsplit('/').next()?.to_string();

                let created_at = s.get("createTime")
                    .and_then(|v| v.as_str())
                    .unwrap_or("N/A")
                    .to_string();

                Some(SecretInfo {
                    name,
                    version: "N/A".to_string(),
                    created_at: created_at.clone(),
                    updated_at: created_at,
                })
            })
            .collect();

        Ok(infos)
    }

    async fn exists(&self, name: &str) -> Result<bool> {
        let url = format!("{}/{}", self.base_url(), name);

        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM request failed: {e}")))?;

        Ok(response.status().is_success())
    }

    async fn health(&self) -> Result<()> {
        // List secrets with page size 1 as health check
        let url = format!("{}?pageSize=1", self.base_url());

        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("GCP SM health check failed: {e}")))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::Internal(format!(
                "GCP SM unhealthy: status {}",
                response.status()
            )))
        }
    }
}

// ============================================================================
// Service
// ============================================================================

/// Secrets Manager service.
#[derive(Clone)]
pub struct SecretsService {
    backend: Arc<dyn SecretsBackend>,
}

impl std::fmt::Debug for SecretsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsService")
            .field("backend", &"<SecretsBackend>")
            .finish()
    }
}

impl SecretsService {
    /// Create a new secrets service with the given backend.
    pub fn new(backend: impl SecretsBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Create secrets service from environment configuration.
    ///
    /// Reads `SECRETS_URL` environment variable.
    ///
    /// Supported URL schemes:
    /// - `memory://` - In-memory (testing)
    /// - `env://PREFIX` - Environment variables with prefix
    /// - `vault://host:port` - `HashiCorp` Vault
    /// - `aws-sm://region` - AWS Secrets Manager
    /// - `gcp-sm://project` - GCP Secret Manager
    ///
    /// # Errors
    ///
    /// Returns an error if `SECRETS_URL` is not set or the URL scheme is unsupported.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::SecretsService;
    ///
    /// // Set SECRETS_URL=vault://localhost:8200?token=my-token
    /// let secrets = SecretsService::from_env()?;
    /// ```
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("SECRETS_URL").map_err(|_| {
            Error::Config("SECRETS_URL environment variable not set".to_string())
        })?;

        Self::from_url(&url)
    }

    /// Create secrets service from a URL.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is unsupported or backend initialization fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::SecretsService;
    ///
    /// // In-memory secrets (for testing)
    /// let secrets = SecretsService::from_url("memory://")?;
    ///
    /// // Environment variables with prefix
    /// let secrets = SecretsService::from_url("env://APP_SECRET_")?;
    ///
    /// // HashiCorp Vault
    /// let secrets = SecretsService::from_url("vault://localhost:8200?token=my-token&mount=secret")?;
    ///
    /// // AWS Secrets Manager (uses AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY env vars)
    /// let secrets = SecretsService::from_url("aws-sm://us-east-1")?;
    ///
    /// // GCP Secret Manager (uses GCP_ACCESS_TOKEN env var)
    /// let secrets = SecretsService::from_url("gcp-sm://my-project")?;
    /// ```
    pub fn from_url(url: &str) -> Result<Self> {
        info!(url = %url, "Initializing secrets backend");

        if url.starts_with("memory://") {
            return Ok(Self::new(MemoryBackend::new()));
        }

        if url.starts_with("env://") {
            let prefix = url.strip_prefix("env://").unwrap_or("SECRET_");
            return Ok(Self::new(EnvBackend::new(prefix)));
        }

        if url.starts_with("vault://") {
            return Ok(Self::new(VaultBackend::from_url(url)?));
        }

        if url.starts_with("aws-sm://") {
            return Ok(Self::new(AwsSecretsManagerBackend::from_url(url)?));
        }

        if url.starts_with("gcp-sm://") {
            return Ok(Self::new(GcpSecretManagerBackend::from_url(url)?));
        }

        Err(Error::Config(format!(
            "Unknown secrets URL scheme: {url}. Supported: memory://, env://, vault://, aws-sm://, gcp-sm://"
        )))
    }

    /// Create an in-memory secrets service (for testing).
    #[must_use]
    pub fn in_memory() -> Self {
        Self::new(MemoryBackend::new())
    }

    /// Create a health check for this service.
    #[must_use]
    pub fn health_checker(&self) -> HealthCheck {
        let backend = self.backend.clone();
        HealthCheck::with_async("secrets", move || {
            let backend = backend.clone();
            async move {
                match backend.health().await {
                    Ok(()) => HealthResult::healthy(),
                    Err(e) => {
                        warn!(error = %e, "Secrets health check failed");
                        HealthResult::unhealthy(format!("Secrets backend unhealthy: {e}"))
                    }
                }
            }
        })
    }
}

impl Sidecar for SecretsService {
    fn name(&self) -> &'static str {
        "secrets"
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/secret/{*name}", get(get_secret))
            .route("/secret/{*name}", post(create_secret))
            .route("/secret/{*name}", put(update_secret))
            .route("/secret/{*name}", delete(delete_secret))
            .route("/exists/{*name}", get(exists_secret))
            .route("/list", get(list_secrets))
            .route("/list/{*prefix}", get(list_secrets_prefix))
            .with_state(self.backend.clone())
    }

    fn health_check(&self) -> bool {
        true
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// Get a secret by name.
async fn get_secret(
    State(backend): State<Arc<dyn SecretsBackend>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse> {
    let secret = backend
        .get(&name)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Secret '{name}' not found")))?;

    Ok(Json(secret))
}

/// Create a new secret.
async fn create_secret(
    State(backend): State<Arc<dyn SecretsBackend>>,
    Path(name): Path<String>,
    Json(req): Json<SecretRequest>,
) -> Result<impl IntoResponse> {
    backend.create(&name, &req.value, req.labels).await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "status": "created",
            "name": name
        })),
    ))
}

/// Update an existing secret.
async fn update_secret(
    State(backend): State<Arc<dyn SecretsBackend>>,
    Path(name): Path<String>,
    Json(req): Json<SecretRequest>,
) -> Result<impl IntoResponse> {
    backend.update(&name, &req.value).await?;

    Ok(Json(serde_json::json!({
        "status": "updated",
        "name": name
    })))
}

/// Delete a secret.
async fn delete_secret(
    State(backend): State<Arc<dyn SecretsBackend>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse> {
    let deleted = backend.delete(&name).await?;

    if deleted {
        Ok(Json(serde_json::json!({
            "status": "deleted",
            "name": name
        })))
    } else {
        Err(Error::NotFound(format!("Secret '{name}' not found")))
    }
}

/// Check if a secret exists.
async fn exists_secret(
    State(backend): State<Arc<dyn SecretsBackend>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse> {
    let exists = backend.exists(&name).await?;

    Ok(Json(serde_json::json!({
        "exists": exists,
        "name": name
    })))
}

/// List all secrets.
async fn list_secrets(
    State(backend): State<Arc<dyn SecretsBackend>>,
) -> Result<impl IntoResponse> {
    let secrets = backend.list(None).await?;

    Ok(Json(serde_json::json!({
        "secrets": secrets,
        "count": secrets.len()
    })))
}

/// List secrets with a prefix.
async fn list_secrets_prefix(
    State(backend): State<Arc<dyn SecretsBackend>>,
    Path(prefix): Path<String>,
) -> Result<impl IntoResponse> {
    let secrets = backend.list(Some(&prefix)).await?;

    Ok(Json(serde_json::json!({
        "secrets": secrets,
        "count": secrets.len(),
        "prefix": prefix
    })))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_backend_crud() {
        let backend = MemoryBackend::new();

        // Create
        backend
            .create("db/password", "secret123", HashMap::new())
            .await
            .unwrap();

        // Get
        let secret = backend.get("db/password").await.unwrap().unwrap();
        assert_eq!(secret.value, "secret123");
        assert_eq!(secret.version, "1");

        // Update
        backend.update("db/password", "newsecret").await.unwrap();
        let secret = backend.get("db/password").await.unwrap().unwrap();
        assert_eq!(secret.value, "newsecret");
        assert_eq!(secret.version, "2");

        // Exists
        assert!(backend.exists("db/password").await.unwrap());
        assert!(!backend.exists("nonexistent").await.unwrap());

        // List
        backend
            .create("db/username", "admin", HashMap::new())
            .await
            .unwrap();
        let list = backend.list(Some("db/")).await.unwrap();
        assert_eq!(list.len(), 2);

        // Delete
        assert!(backend.delete("db/password").await.unwrap());
        assert!(!backend.delete("nonexistent").await.unwrap());
        assert!(!backend.exists("db/password").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "Requires setting env vars which is unsafe in Rust 1.80+ (std::env::set_var is unsafe)"]
    async fn test_env_backend() {
        // This test is ignored because std::env::set_var became unsafe in Rust 1.80+
        // and the crate forbids unsafe code. The EnvBackend is tested through
        // integration tests that set env vars before running.
        let backend = EnvBackend::new("TEST_SECRET_");

        // Env backend is read-only
        assert!(backend.create("new", "val", HashMap::new()).await.is_err());
        assert!(backend.update("api/key", "val").await.is_err());
        assert!(backend.delete("api/key").await.is_err());
    }
}
