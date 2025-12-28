# mikcar - Sidecar infrastructure services (Minimal musl variant)
# Optimized multi-stage build for minimal scratch image
#
# Build: docker build -t mikcar .
# Size: ~20MB
#
# Features included:
#   - storage: S3, GCS, Azure, local filesystem
#   - kv-redb: Embedded key-value store (pure Rust, no Redis needed)
#   - sql: Postgres, SQLite with JS scripting
#   - secrets: Vault, AWS Secrets Manager, GCP Secret Manager
#   - email: SMTP (works with any provider - Gmail, SendGrid, SES, Resend, etc.)
#
# Uses:
#   - clux/muslrust: Pre-configured musl build environment
#   - mimalloc: Fast allocator (fixes musl multi-core performance)
#   - rustls: Pure Rust TLS (no OpenSSL dependency)
#   - scratch: Zero-overhead base image

# =============================================================================
# Stage 1: Build with musl for fully static binary
# =============================================================================
FROM clux/muslrust:stable AS builder

# Feature flags
# Uses 'musl' feature set: kv-redb (embedded), storage, sql, secrets, email
# Note: 'kv-redis' and 'queue' excluded - require native-tls which breaks musl builds
# Use Dockerfile.distroless for all features including Redis and queue backends
ARG FEATURES=musl

WORKDIR /app

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./

# Create dummy src for dependency caching
RUN mkdir -p src && echo "fn main() {}" > src/main.rs && echo "pub fn dummy() {}" > src/lib.rs

# Build dependencies only (cached layer)
RUN cargo build --release --no-default-features --features ${FEATURES} 2>/dev/null || true

# Copy actual source
COPY src ./src

# Touch main.rs to invalidate the dummy
RUN touch src/main.rs src/lib.rs

# Build the final binary
RUN cargo build --release --no-default-features --features ${FEATURES}

# Strip the binary for smaller size
RUN strip /app/target/x86_64-unknown-linux-musl/release/mikcar

# =============================================================================
# Stage 2: Minimal runtime (scratch)
# =============================================================================
FROM scratch

# Copy CA certificates for HTTPS requests (from builder)
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Copy the static binary
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/mikcar /mikcar

# Create app directory structure
WORKDIR /app

# Default environment
ENV PORT=3001
ENV RUST_LOG=info,mikcar=debug
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

EXPOSE 3001

# Run mikcar (no shell available in scratch)
ENTRYPOINT ["/mikcar"]
CMD ["--all"]
