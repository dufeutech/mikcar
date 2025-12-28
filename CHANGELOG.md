# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 (2025-12-28)


### Features

* add core library infrastructure ([0d966cb](https://github.com/dufeut/mikcar/commit/0d966cbd36f20010a1255ecbb7b63b36bd03d500))
* add sidecar services ([7b30586](https://github.com/dufeut/mikcar/commit/7b305863697eda08fd56ddf922fa9aa48b353283))
* **ci:** publish to crates.io on release and upgrade to Rust 1.89 ([baa1398](https://github.com/dufeut/mikcar/commit/baa1398cdb4dd127ea30e652cabab64bbca7a294))


### Bug Fixes

* **ci:** add missing licenses and skip sql on Windows ([22a62e0](https://github.com/dufeut/mikcar/commit/22a62e0a2fe48f35e4c61f25ec921e6bd00a82c5))
* **ci:** add more advisory ignores and license clarifications ([ba3834c](https://github.com/dufeut/mikcar/commit/ba3834c05255c0b67e9ca72b389aa642e5109483))
* **ci:** clean target directory on Windows to avoid LNK1318 PDB errors ([8a75009](https://github.com/dufeut/mikcar/commit/8a7500901ed8379544f7fbb9f03b3a46a5246fe2))
* **ci:** configure security audit to ignore unmaintained advisories ([4d6bad6](https://github.com/dufeut/mikcar/commit/4d6bad6fa96a8a741dfb6c5f932c64bf7bfde2a7))
* **ci:** ignore RSA timing sidechannel advisory ([a4347e7](https://github.com/dufeut/mikcar/commit/a4347e7de24f84299754952692084d8abdd1a9e1))
* **ci:** move use statements to function top and pin cargo-machete ([209f54d](https://github.com/dufeut/mikcar/commit/209f54d1d8888deacf1dbbb1ad6928771265f2d1))
* **ci:** resolve clippy warnings and test feature gates ([652881d](https://github.com/dufeut/mikcar/commit/652881d5dd60ba319031aa5204d852c3f3b41b86))
* **ci:** resolve Windows LNK1318 PDB linker error ([41350fd](https://github.com/dufeut/mikcar/commit/41350fd93b6c4f6ac685efd915bc2da3287045bb))
* **ci:** run Windows tests in release mode to avoid PDB symbol limit ([ca578ac](https://github.com/dufeut/mikcar/commit/ca578acdb9eea35ec546552a94aae7dbc5b3e22b))
* **ci:** skip email integration tests on Windows ([f7791de](https://github.com/dufeut/mikcar/commit/f7791deeb34ad31ca7c062264e03786b3ad3e62e))
* **ci:** skip queue feature on Windows ([e04a09b](https://github.com/dufeut/mikcar/commit/e04a09bd92de61af4ce7a43bb62b59d55dfa6d1a))
* **ci:** use --locked flag for cargo-machete install ([cd92074](https://github.com/dufeut/mikcar/commit/cd920743d285952ff5dc44d9becdf270889459d7))
* collapse nested if statements for clippy ([abac599](https://github.com/dufeut/mikcar/commit/abac599af3ca81ad8834a4f6e27cb00f32939904))
* resolve CI failures (formatting and clippy warnings) ([08ab354](https://github.com/dufeut/mikcar/commit/08ab354627f93a2a470976371eaa041f82dd4da6))
* set initial version to 0.1.0 ([5d73570](https://github.com/dufeut/mikcar/commit/5d73570132a7a0eb31a2f727f0d09ba848af984f))

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
