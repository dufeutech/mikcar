# mikcar

Sidecar infrastructure services for mik.

## Overview

mikcar provides HTTP-based infrastructure services for WASM handlers running in mik:

- **Storage** - Object storage (S3, GCS, MinIO, local filesystem)
- **KV** - Embedded key-value store (redb - pure Rust, no external dependencies)
- **SQL** - Database proxy (Postgres, SQLite)
- **Queue** - Message queues (Redis Streams, RabbitMQ)
- **Secrets** - Secret managers (Vault, AWS Secrets Manager, GCP Secret Manager)
- **Email** - SMTP email (works with any provider)

Same API works locally and in production. Your WASM handlers don't know or care if they're talking to MinIO or AWS S3.

## Docker

Two optimized images available:

| Image            | Size  | Queue Support   | Base                  |
| ---------------- | ----- | --------------- | --------------------- |
| `Dockerfile`     | ~21MB | No              | scratch (musl static) |
| `Dockerfile.all` | ~65MB | Redis, RabbitMQ | distroless            |

```bash
# Minimal image (no queue) - recommended for most use cases
docker build -t mikcar .

# Full image with queue support
docker build -f Dockerfile.all -t mikcar:all .

# Run
docker run -p 3001:3001 \
  -e SIDECAR_TOKEN=secret \
  -e KV_URL=memory:// \
  -e STORAGE_URL=memory:// \
  mikcar --kv --storage
```

## Installation

```bash
# Install with default services (storage, kv, sql, secrets, email)
cargo install mikcar

# Install with queue support
cargo install mikcar --features all

# Install specific services only
cargo install mikcar --features storage,kv
```

## Usage

```bash
# Run storage service
STORAGE_URL=file:///data mikcar --storage --port 3001

# Run KV service (embedded redb, no external dependencies)
KV_URL=file:///data/kv.redb mikcar --kv --port 3002

# Run SQL service
DATABASE_URL=postgres://user:pass@localhost/db mikcar --sql --port 3003

# Run secrets service
SECRETS_URL=vault://localhost:8200 mikcar --secrets --port 3004

# Run email service
EMAIL_URL=smtp://localhost:1025 mikcar --email --port 3005

# Supercar mode (multiple services on one port)
mikcar --kv --storage --sql --email --port 3001
```

## API

### Storage (`/storage/*`)

```
GET    /object/{path}    Get object
PUT    /object/{path}    Put object (body = content)
DELETE /object/{path}    Delete object
HEAD   /object/{path}    Check exists + metadata
GET    /list/{prefix}    List objects
```

### Key-Value (`/kv/*`)

```
GET    /get/{key}        Get value
POST   /get/batch        Get multiple (body = {keys: [...]})
POST   /set/{key}?ttl=   Set value (body = value)
POST   /set/batch        Set multiple (body = {key: value, ...})
DELETE /del/{key}        Delete key
GET    /keys/{pattern}   List keys matching pattern
POST   /increment/{key}  Increment (atomic)
GET    /exists/{key}     Check if key exists
GET    /ttl/{key}        Get TTL (-1 = no TTL, -2 = not found)
POST   /expire/{key}     Set expiration (body = {seconds: N})
```

### SQL (`/sql/*`)

```
POST   /query            Execute SELECT (body = {sql, params})
POST   /execute          Execute INSERT/UPDATE/DELETE
POST   /batch            Execute multiple statements in transaction
POST   /script           Execute JavaScript with SQL in transaction
```

#### SQL Script Endpoint

The `/sql/script` endpoint executes JavaScript code with SQL operations within a single database transaction. This enables complex conditional logic, validation, and multi-step operations with automatic rollback on failure.

**Request:**
```json
{
  "script": "var user = sql.query('SELECT * FROM users WHERE id = $1', [input.userId]); if (!user.length) throw new Error('User not found'); return { user: user[0] };",
  "input": { "userId": 42 }
}
```

**Response:**
```json
{
  "result": { "user": { "id": 42, "name": "Alice" } },
  "queries_executed": 1
}
```

**Script API:**
- `sql.query(sql, params)` - Execute SELECT, returns array of row objects
- `sql.execute(sql, params)` - Execute INSERT/UPDATE/DELETE, returns rows affected
- `input` - The input object from the request
- `return` - Specify the response value

**Supported script formats:**

```javascript
// 1. Export default function (recommended)
export default function(input) {
  var users = sql.query("SELECT * FROM users WHERE org = $1", [input.org]);
  return { count: users.length };
}

// 2. Raw code with return (simple scripts)
var users = sql.query("SELECT * FROM users");
return { count: users.length };

// 3. Raw expression (simplest)
sql.query("SELECT COUNT(*) FROM users")
```

**Transaction semantics:**
- All SQL operations run in a single transaction
- If the script throws an error, the transaction is rolled back
- If any SQL operation fails, the transaction is rolled back
- Only on successful completion is the transaction committed

**Example: Fund transfer with balance check**
```javascript
export default function(input) {
  // Check source account
  var from = sql.query("SELECT id, balance FROM accounts WHERE id = $1", [input.from]);
  if (!from.length) throw new Error("Source account not found");
  if (parseFloat(from[0].balance) < input.amount) throw new Error("Insufficient funds");

  // Check destination account
  var to = sql.query("SELECT id FROM accounts WHERE id = $1", [input.to]);
  if (!to.length) throw new Error("Destination account not found");

  // Perform transfer (atomic - both succeed or both fail)
  sql.execute("UPDATE accounts SET balance = balance - $1 WHERE id = $2", [input.amount, input.from]);
  sql.execute("UPDATE accounts SET balance = balance + $1 WHERE id = $2", [input.amount, input.to]);

  return { success: true, transferred: input.amount };
}
```

### Queue (`/queue/*`)

```
POST   /publish/{topic}  Publish message
GET    /subscribe/{topic} Long-poll for messages
POST   /ack/{topic}/{id} Acknowledge message
POST   /push/{queue}     Push to work queue
GET    /pop/{queue}      Pop from work queue
```

### Email (`/email/*`)

```
POST   /send             Send single email
POST   /send/batch       Send multiple emails
```

**Request body (POST /send):**
```json
{
  "from": "sender@example.com",
  "to": ["recipient@example.com"],
  "cc": ["cc@example.com"],
  "bcc": ["bcc@example.com"],
  "subject": "Hello",
  "text": "Plain text body",
  "html": "<p>HTML body</p>",
  "reply_to": "reply@example.com"
}
```

**Response:**
```json
{"id": "message-id", "success": true}
```

**Batch request (POST /send/batch):**
```json
{
  "emails": [
    {"from": "...", "to": ["..."], "subject": "...", "text": "..."},
    {"from": "...", "to": ["..."], "subject": "...", "text": "..."}
  ]
}
```

## Authentication

Set `SIDECAR_TOKEN` or `AUTH_TOKEN` environment variable. Requests must include:

```
Authorization: Bearer <token>
```

The `/health` endpoint is always accessible without authentication.

## Configuration

| Variable        | Service | Example                                                  |
| --------------- | ------- | -------------------------------------------------------- |
| `STORAGE_URL`   | storage | `file:///data`, `s3://bucket`, `memory://`               |
| `KV_URL`        | kv      | `file:///data/kv.redb`, `memory://`                      |
| `DATABASE_URL`  | sql     | `postgres://user:pass@host/db`                           |
| `QUEUE_URL`     | queue   | `redis://host:port`, `amqp://user:pass@host:port`        |
| `SECRETS_URL`   | secrets | `vault://host:port`, `awssm://region`, `gcpsm://project` |
| `EMAIL_URL`     | email   | `smtp://host:port`, `smtps://user:pass@host:465`         |
| `SIDECAR_TOKEN` | auth    | Bearer token for API authentication                      |

## Queue Backends

mikcar acts as an HTTP proxy to queue infrastructure. Supported backends:

| Backend       | URL Format                   | Platform    |
| ------------- | ---------------------------- | ----------- |
| In-memory     | `memory://`                  | All         |
| Redis Streams | `redis://host:port`          | Linux/macOS |
| RabbitMQ      | `amqp://user:pass@host:port` | Linux/macOS |

**Auto-creation:** Queues are automatically created on first push:
- Redis: Creates stream + consumer group (`XGROUP CREATE ... MKSTREAM`)
- RabbitMQ: Declares durable queue

No manual setup required - just push and pop.

**Note:** Queue requires the `all` feature or `Dockerfile.all`. Windows builds only support `memory://`.

```bash
# Redis
QUEUE_URL=redis://localhost:6379 mikcar --queue

# RabbitMQ
QUEUE_URL=amqp://guest:guest@localhost:5672 mikcar --queue

# In-memory (dev only)
QUEUE_URL=memory:// mikcar --queue
```

## Email Backends

SMTP is the universal email protocol - works with any provider.

| Provider            | URL Format                                                      |
| ------------------- | --------------------------------------------------------------- |
| Local dev (Mailpit) | `smtp://localhost:1025`                                         |
| Gmail               | `smtps://user:app-password@smtp.gmail.com:465`                  |
| SendGrid            | `smtps://apikey:SG.xxx@smtp.sendgrid.net:465`                   |
| AWS SES             | `smtps://AKIA...:secret@email-smtp.us-east-1.amazonaws.com:465` |
| Resend              | `smtps://resend:re_xxx@smtp.resend.com:465`                     |

**Port behavior:**
- Ports 25, 1025, 2525: Plain SMTP (no TLS) - for local testing
- Port 465: Implicit TLS (`smtps://`)
- Port 587: STARTTLS (`smtp://`)

```bash
# Local development with Mailpit
EMAIL_URL=smtp://localhost:1025 mikcar --email

# Production with SendGrid
EMAIL_URL=smtps://apikey:SG.xxx@smtp.sendgrid.net:465 mikcar --email
```

## Library Usage

mikcar can be used as a Rust library to embed sidecars in your own applications (e.g., Tauri, Axum, or custom servers).

### Add Dependencies

```toml
[dependencies]
mikcar = { git = "https://github.com/dufeut/mikcar", features = ["all"] }
tokio = { version = "1", features = ["full"] }
```

### Example: Single Service

```rust
use mikcar::{SidecarBuilder, StorageService};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let storage = StorageService::from_url("memory://")?;

    SidecarBuilder::new()
        .port(3001)
        .auth_token("secret")
        .serve(storage)
        .await
}
```

### Example: Multiple Services

```rust
use mikcar::{SidecarBuilder, KvService, StorageService, SqlService};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let kv = KvService::from_url("memory://")?;
    let storage = StorageService::from_url("memory://")?;
    let sql = SqlService::from_url("postgres://user:pass@localhost/db").await?;

    SidecarBuilder::new()
        .port(3001)
        .auth_token("secret")
        .add(kv)
        .add(storage)
        .add(sql)
        .serve_many()
        .await
}
```

### Example: Tauri Integration

```rust
use mikcar::{SidecarBuilder, KvService, StorageService};
use std::sync::Arc;
use tokio::sync::Mutex;

struct AppState {
    sidecars: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

#[tauri::command]
async fn start_sidecars(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let kv = KvService::from_url("memory://").map_err(|e| e.to_string())?;
    let storage = StorageService::from_url("memory://").map_err(|e| e.to_string())?;

    let handle = tokio::spawn(async move {
        let _ = SidecarBuilder::new()
            .port(3001)
            .add(kv)
            .add(storage)
            .serve_many()
            .await;
    });

    *state.sidecars.lock().await = Some(handle);
    Ok(())
}
```

### Example: Combined with mik Runtime

```rust
use mik::runtime::HostBuilder;
use mikcar::{SidecarBuilder, KvService, StorageService};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Start sidecars on port 3001
    let kv = KvService::from_url("memory://")?;
    let storage = StorageService::from_url("memory://")?;

    tokio::spawn(async move {
        SidecarBuilder::new()
            .port(3001)
            .add(kv)
            .add(storage)
            .serve_many()
            .await
            .unwrap();
    });

    // Start WASM runtime on port 3000
    let host = HostBuilder::new()
        .port(3000)
        .modules_dir("./modules".into())
        .build()
        .await?;

    host.serve().await?;
    Ok(())
}
```

### Exported Types

| Type             | Description                                   |
| ---------------- | --------------------------------------------- |
| `SidecarBuilder` | Builder for configuring and starting sidecars |
| `StorageService` | S3/GCS/Azure/local filesystem storage         |
| `KvService`      | Embedded key-value store (redb)               |
| `SqlService`     | PostgreSQL/SQLite proxy                       |
| `QueueService`   | Redis Streams/RabbitMQ queues                 |
| `SecretsService` | Vault/AWS/GCP secret managers                 |
| `EmailService`   | SMTP email sending                            |
| `Sidecar` trait  | Implement custom sidecars                     |

### Re-exported Crates

mikcar re-exports commonly used crates for convenience:

```rust
use mikcar::axum;        // Web framework
use mikcar::tower;       // Service abstractions
use mikcar::tower_http;  // HTTP middleware
```

## License

MIT
