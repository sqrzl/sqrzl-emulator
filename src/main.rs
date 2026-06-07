use std::io::IsTerminal;
use std::sync::Arc;

use peas_emulator::api::server::start_ui_server;
use peas_emulator::config::LogFormat;
use peas_emulator::error::Result;
use peas_emulator::server::Server;
use peas_emulator::storage::{FilesystemStorage, Storage};
use peas_emulator::utils::validation::validate_bucket_name;
use peas_emulator::{Config, Error};

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration from environment variables
    let config = Config::from_env();
    let log_format = Config::log_format_from_env();

    // Initialize structured logging
    init_logging(log_format);

    tracing::info!(version = "0.1.0", "Peas Emulator started");
    tracing::info!("Provider-compatible object storage emulator");

    // Log authentication status
    if config.enforce_auth {
        if let Some(key) = config.access_key() {
            tracing::info!(access_key = key, "Authentication enabled");
        }
    } else {
        tracing::info!("Authentication disabled");
    }

    // Initialize storage
    tracing::info!(path = %config.blobs_path, "Using filesystem storage");
    let storage = Arc::new(FilesystemStorage::new(&config.blobs_path));
    let startup_buckets = Config::startup_bucket_names_from_env();

    ensure_startup_buckets(storage.as_ref(), &startup_buckets)?;

    // Start lifecycle executor
    let lifecycle_executor =
        peas_emulator::LifecycleExecutor::new(storage.clone(), config.lifecycle_interval);
    let _lifecycle_handle = lifecycle_executor.start();
    tracing::info!("Lifecycle executor started");

    // Start both servers
    tracing::info!("S3 API listening on http://127.0.0.1:{}", config.api_port);
    tracing::info!("UI listening on http://127.0.0.1:{}", config.ui_port);

    let api_server =
        Server::new(storage.clone(), Arc::new(config.clone()), config.api_port).start();
    let ui_server = start_ui_server(storage, Arc::new(config.clone()));

    // Run both servers concurrently
    let result = tokio::select! {
        result = api_server => result,
        result = ui_server => result,
    };
    result?;
    Ok(())
}

fn init_logging(log_format: LogFormat) {
    match log_format {
        LogFormat::Json => {
            let env_filter = tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("peas_emulator=info".parse().unwrap());
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .with_current_span(true)
                .with_target(true)
                .with_level(true)
                .init();
        }
        LogFormat::Text => {
            let env_filter = tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("peas_emulator=info".parse().unwrap());
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(env_filter)
                .with_timer(tracing_subscriber::fmt::time::SystemTime)
                .with_ansi(std::io::stderr().is_terminal())
                .with_target(true)
                .with_level(true)
                .with_file(false)
                .with_line_number(false)
                .init();
        }
    }
}

fn ensure_startup_buckets(storage: &dyn Storage, bucket_names: &[String]) -> Result<()> {
    for bucket_name in bucket_names {
        if let Err(message) = validate_bucket_name(bucket_name) {
            return Err(Error::InvalidRequest(format!(
                "Invalid startup bucket '{bucket_name}': {message}"
            )));
        }
    }

    if !bucket_names.is_empty() {
        tracing::info!(count = bucket_names.len(), "Ensuring startup buckets");
    }

    for bucket_name in bucket_names {
        match storage.create_bucket(bucket_name.clone()) {
            Ok(()) => tracing::info!(bucket = %bucket_name, "Created startup bucket"),
            Err(Error::BucketAlreadyExists) => {
                tracing::debug!(bucket = %bucket_name, "Startup bucket already exists");
            }
            Err(err) => return Err(err),
        }
    }

    Ok(())
}
