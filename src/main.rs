use std::io::IsTerminal;
use std::sync::Arc;

use sqrzl_emulator::api::server::start_ui_server_with_sms;
use sqrzl_emulator::config::LogFormat;
use sqrzl_emulator::error::Result;
use sqrzl_emulator::mail::{FilesystemMailStore, SmtpServer};
use sqrzl_emulator::server::Server;
use sqrzl_emulator::sms::FilesystemSmsStore;
use sqrzl_emulator::storage::{BucketStore, FilesystemStorage, StorageRootWriter};
use sqrzl_emulator::utils::validation::validate_bucket_name;
use sqrzl_emulator::{Config, Error};

fn main() -> Result<()> {
    // Load configuration from environment variables
    let config = Config::from_env();
    config
        .validate_vendor_credentials()
        .map_err(Error::InvalidRequest)?;
    let log_format = Config::log_format_from_env();

    // Initialize structured logging
    init_logging(log_format);

    tracing::info!(version = "0.1.0", "Sqrzl Emulator started");
    tracing::info!("Provider-compatible object storage emulator");

    // Log authentication status
    if config.enforce_auth {
        if let Some(key) = config.access_key() {
            tracing::info!(access_key = key, "Authentication enabled");
        }
    } else {
        tracing::info!("Authentication disabled");
    }

    let root = config.blobs_path.clone();
    with_storage_writer_runtime(&root, |runtime| runtime.block_on(run(config)))
}

fn with_storage_writer_runtime<T>(
    root: impl AsRef<std::path::Path>,
    run: impl FnOnce(&tokio::runtime::Runtime) -> Result<T>,
) -> Result<T> {
    with_storage_writer_shutdown(root, run, drop)
}

fn with_storage_writer_shutdown<T>(
    root: impl AsRef<std::path::Path>,
    run: impl FnOnce(&tokio::runtime::Runtime) -> Result<T>,
    shutdown: impl FnOnce(tokio::runtime::Runtime),
) -> Result<T> {
    // Declare ownership before the runtime so normal return, errors and panic
    // unwinding all finish Tokio's blocking writers before releasing the root.
    let _writer = StorageRootWriter::acquire(root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::InternalError(format!("Failed to initialize runtime: {error}")))?;
    let result = run(&runtime);
    shutdown(runtime);
    result
}

async fn run(config: Config) -> Result<()> {
    // Initialize storage
    tracing::info!(path = %config.blobs_path, "Using filesystem storage");
    let storage = Arc::new(FilesystemStorage::open(&config.blobs_path)?);
    let mail = Arc::new(FilesystemMailStore::open(&config.blobs_path)?);
    let sms = Arc::new(FilesystemSmsStore::open(&config.blobs_path)?);
    let startup_buckets = Config::startup_bucket_names_from_env();

    ensure_startup_buckets(storage.as_ref(), &startup_buckets)?;

    // Start lifecycle executor
    let lifecycle_executor =
        sqrzl_emulator::LifecycleExecutor::new(storage.clone(), config.lifecycle_interval);
    let _lifecycle_handle = lifecycle_executor.start();
    tracing::info!("Lifecycle executor started");

    // Start all three listeners
    tracing::info!("S3 API listening on http://127.0.0.1:{}", config.api_port);
    tracing::info!("UI listening on http://127.0.0.1:{}", config.ui_port);
    tracing::info!(
        "SMTP capture listening on smtp://127.0.0.1:{}",
        config.smtp_port
    );

    let config = Arc::new(config);
    let mut listeners = tokio::task::JoinSet::new();
    listeners.spawn({
        let storage = storage.clone();
        let mail = mail.clone();
        let config = config.clone();
        let sms = sms.clone();
        async move {
            Server::new_with_sms(storage, mail, sms, config.clone(), config.api_port)
                .start()
                .await
        }
    });
    listeners.spawn({
        let storage = storage.clone();
        let config = config.clone();
        let mail = mail.clone();
        let sms = sms.clone();
        async move { start_ui_server_with_sms(storage, config, mail, sms).await }
    });
    listeners.spawn({
        let mail = mail.clone();
        let port = config.smtp_port;
        let max_message_bytes = config.max_request_bytes;
        async move {
            SmtpServer::new(mail, port)
                .with_max_message_bytes(max_message_bytes)
                .start()
                .await
        }
    });

    let Some(result) = listeners.join_next().await else {
        return Ok(());
    };

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(Error::InternalError(err.to_string())),
        Err(err) => Err(Error::InternalError(err.to_string())),
    }
}

fn init_logging(log_format: LogFormat) {
    match log_format {
        LogFormat::Json => {
            let env_filter = tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sqrzl_emulator=info".parse().unwrap());
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
                .add_directive("sqrzl_emulator=info".parse().unwrap());
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

fn ensure_startup_buckets(
    storage: &(impl BucketStore + ?Sized),
    bucket_names: &[String],
) -> Result<()> {
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

#[cfg(test)]
mod shutdown_lifetime_tests {
    use super::*;
    use sqrzl_emulator::models::Object;
    use sqrzl_emulator::storage::ObjectStore;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn should_hold_root_writer_given_listener_failure_when_blocking_publication_is_running() {
        // Arrange
        let root =
            std::env::temp_dir().join(format!("sqrzl-shutdown-writer-{}", uuid::Uuid::new_v4()));
        let owner_root = root.clone();
        let (releasing, release) = mpsc::channel();
        let (shutdown_started, shutdown) = mpsc::channel();
        let (owner_finished, finished) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let result = with_storage_writer_shutdown(
                &owner_root,
                |runtime| {
                    runtime.block_on(async {
                        let storage = FilesystemStorage::open(&owner_root)?;
                        storage.create_bucket("shutdown".to_string())?;
                        let (started, ready) = tokio::sync::oneshot::channel();
                        tokio::spawn(async move {
                            tokio::task::block_in_place(|| {
                                started.send(()).unwrap();
                                release.recv_timeout(Duration::from_secs(10)).unwrap();
                                storage
                                    .put_object(
                                        "shutdown",
                                        "item".to_string(),
                                        Object::new(
                                            "item".to_string(),
                                            b"committed".to_vec(),
                                            "text/plain".to_string(),
                                        ),
                                    )
                                    .unwrap();
                            });
                        });
                        ready.await.unwrap();
                        Err::<(), _>(Error::InternalError("listener failed".to_string()))
                    })
                },
                |runtime| {
                    // The main future has returned; runtime destruction still
                    // waits for the in-flight blocking storage operation.
                    shutdown_started.send(()).unwrap();
                    drop(runtime);
                },
            );
            owner_finished.send(result).unwrap();
        });
        shutdown.recv_timeout(Duration::from_secs(5)).unwrap();

        // Act
        let competing_owner = StorageRootWriter::acquire(&root);

        // Assert
        assert!(
            matches!(competing_owner, Err(Error::InvalidRequest(ref message)) if message.contains("already has an active writer")),
            "root ownership was released before blocking publication finished"
        );
        assert!(matches!(
            finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        releasing.send(()).unwrap();
        assert!(
            matches!(finished.recv_timeout(Duration::from_secs(5)).unwrap(), Err(Error::InternalError(message)) if message == "listener failed")
        );
        owner.join().unwrap();
        let writer = StorageRootWriter::acquire(&root).unwrap();
        let storage = FilesystemStorage::open(&root).unwrap();
        assert_eq!(
            storage.get_object("shutdown", "item").unwrap().data,
            b"committed"
        );
        drop(storage);
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }
}
