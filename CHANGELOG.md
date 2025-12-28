# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2025-12-27

### Added

- **Storage Service** - Object storage with multiple backends
  - AWS S3, Google Cloud Storage, Azure Blob Storage
  - MinIO (S3-compatible) support
  - Local filesystem backend
  - In-memory backend for testing
  - Path traversal protection

- **Key-Value Service** - Embedded KV store using redb
  - Pure Rust, no external dependencies
  - TTL support with automatic expiration
  - Atomic increment operations
  - Batch get/set operations
  - Pattern-based key listing

- **SQL Service** - Database proxy with transaction support
  - PostgreSQL and SQLite backends
  - JavaScript script execution within transactions
  - SQL audit logging
  - Parameterized queries (injection protection)

- **Queue Service** - Message queue abstraction (Linux/macOS)
  - Redis Streams backend
  - RabbitMQ backend
  - In-memory backend for testing
  - Work queue and pub/sub patterns

- **Secrets Service** - Unified secrets management
  - HashiCorp Vault backend
  - AWS Secrets Manager backend
  - GCP Secret Manager backend
  - Environment variables backend
  - File-based encrypted storage

- **Email Service** - SMTP-based email sending
  - Works with any SMTP provider
  - Gmail, SendGrid, AWS SES, Resend support
  - Batch email sending
  - HTML and plain text support

- **Infrastructure**
  - Token-based authentication with constant-time comparison
  - Health checks with async backend verification
  - CORS support (disabled, permissive, or custom)
  - Request/response compression
  - Request timeout and body size limits
  - OpenTelemetry OTLP tracing support (Jaeger, Tempo)
  - Multi-service "supercar" mode

- **Build & Deploy**
  - Minimal Docker image (~21MB with musl)
  - Full-featured Docker image (~65MB with glibc)
  - Multi-platform CI/CD (Linux, Windows, macOS)
  - MSRV 1.85, security audit, coverage tracking

### Security

- Zero unsafe code (`#![forbid(unsafe_code)]`)
- Constant-time token comparison (timing attack prevention)
- Path traversal validation in storage service
- SQL injection protection via parameterized queries
- Client-safe error messages (sensitive errors logged server-side)

[0.1.0]: https://github.com/dufeut/mikcar/releases/tag/v0.1.0
