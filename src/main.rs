//! mikcar CLI - Sidecar infrastructure services.
//!
//! Run individual services or combine them into a supercar.

// Use mimalloc for better multi-core performance (especially with musl)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use mikcar::SidecarBuilder;

#[derive(Parser)]
#[command(name = "mikcar")]
#[command(version)]
#[command(about = "Sidecar infrastructure services for mikrozen")]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value = "3001", env = "PORT")]
    port: u16,

    /// Authentication token (or set SIDECAR_TOKEN env var)
    #[arg(long, env = "SIDECAR_TOKEN")]
    auth_token: Option<String>,

    /// OTLP endpoint for distributed tracing (e.g., http://localhost:4317)
    #[cfg(feature = "otlp")]
    #[arg(long, env = "OTLP_ENDPOINT")]
    otlp_endpoint: Option<String>,

    /// Service name for traces (default: mikcar)
    #[cfg(feature = "otlp")]
    #[arg(long, env = "OTLP_SERVICE_NAME", default_value = "mikcar")]
    service_name: String,

    /// Enable storage service (S3, GCS, MinIO, filesystem)
    #[cfg(feature = "storage")]
    #[arg(long)]
    storage: bool,

    /// Enable key-value service (embedded redb)
    #[cfg(feature = "kv")]
    #[arg(long)]
    kv: bool,

    /// Enable SQL service (Postgres)
    #[cfg(feature = "sql")]
    #[arg(long)]
    sql: bool,

    /// Enable queue service (Redis Streams, RabbitMQ, SQS)
    #[cfg(feature = "queue")]
    #[arg(long)]
    queue: bool,

    /// Enable secrets service (Vault, AWS SM, GCP SM)
    #[cfg(feature = "secrets")]
    #[arg(long)]
    secrets: bool,

    /// Enable email service (SMTP)
    #[cfg(feature = "email-smtp")]
    #[arg(long)]
    email: bool,

    /// Enable all services (supercar mode)
    #[arg(long)]
    all: bool,

    /// Disable authentication
    #[arg(long)]
    no_auth: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing (with OTLP if configured)
    init_tracing(&cli);

    // Build the sidecar server
    let mut builder = SidecarBuilder::new().port(cli.port);

    if cli.no_auth {
        builder = builder.no_auth();
    } else if let Some(token) = cli.auth_token.clone() {
        builder = builder.auth_token(token);
    }

    // Collect enabled services
    let mut services: Vec<(&str, Box<dyn mikcar::Sidecar>)> = Vec::new();

    #[cfg(feature = "storage")]
    if cli.storage || cli.all {
        let storage = mikcar::storage::StorageService::from_env()?;
        services.push(("/storage", Box::new(storage)));
    }

    #[cfg(feature = "kv")]
    if cli.kv || cli.all {
        let kv = mikcar::kv::KvService::from_env()?;
        services.push(("/kv", Box::new(kv)));
    }

    #[cfg(feature = "sql")]
    if cli.sql || cli.all {
        let sql = mikcar::sql::SqlService::from_env().await?;
        services.push(("/sql", Box::new(sql)));
    }

    #[cfg(feature = "queue")]
    if cli.queue || cli.all {
        let queue = mikcar::queue::QueueService::from_env().await?;
        services.push(("/queue", Box::new(queue)));
    }

    #[cfg(feature = "secrets")]
    if cli.secrets || cli.all {
        let secrets = mikcar::secrets::SecretsService::from_env()?;
        services.push(("/secrets", Box::new(secrets)));
    }

    #[cfg(feature = "email-smtp")]
    if cli.email || cli.all {
        let email = mikcar::email::EmailService::from_env()?;
        services.push(("/email", Box::new(email)));
    }

    if services.is_empty() {
        eprintln!("Error: No services enabled. Use --storage, --kv, --sql, --queue, --secrets, or --all");
        eprintln!("\nAvailable features (compile with):");
        eprintln!("  cargo build -p mikcar --features storage");
        eprintln!("  cargo build -p mikcar --features kv");
        eprintln!("  cargo build -p mikcar --features secrets");
        eprintln!("  cargo build -p mikcar --features sql");
        eprintln!("  cargo build -p mikcar --features queue");
        eprintln!("  cargo build -p mikcar --features all");
        std::process::exit(1);
    }

    // Always use serve_multi to maintain consistent URL prefixes
    // e.g., /kv/get/{key}, /storage/object/{path}, etc.
    builder.serve_multi(services).await?;

    Ok(())
}

/// Initialize tracing based on configuration.
#[allow(unused_variables)]
fn init_tracing(cli: &Cli) {
    // Try OTLP if feature is enabled and endpoint is set
    #[cfg(feature = "otlp")]
    if let Some(endpoint) = &cli.otlp_endpoint {
        use mikcar::otlp::{init_with_otlp, OtlpConfig};

        let config = OtlpConfig::new(endpoint).with_service_name(&cli.service_name);

        if let Err(e) = init_with_otlp(config) {
            eprintln!("Warning: Failed to initialize OTLP tracing: {e}");
            eprintln!("Falling back to stdout logging");
            init_stdout_tracing();
        }
        return;
    }

    // Default: stdout logging
    init_stdout_tracing();
}

/// Initialize stdout logging (fallback when OTLP is not configured).
fn init_stdout_tracing() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mikcar=info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}
