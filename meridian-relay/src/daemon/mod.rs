pub mod protocol;
pub mod device_scanner;
pub mod device_manager;
pub mod connection;
pub mod mux;
pub mod usb;
pub mod transport;
pub mod relay;

use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::DaemonConfig;
use crate::metrics::Metrics;
use crate::platform;
#[cfg(unix)]
use crate::security;
use self::device_scanner::{DeviceScanner, DeviceChange};
use self::device_manager::DeviceManager;
use self::connection::handle_client;
use self::transport::{Endpoint, TransportListener};

pub async fn run_daemon(
    config: DaemonConfig,
    metrics: Arc<Metrics>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), Box<dyn std::error::Error>> {
    run_daemon_inner(config, metrics, shutdown).await
}

async fn run_daemon_inner(
    config: DaemonConfig,
    metrics: Arc<Metrics>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), Box<dyn std::error::Error>> {
    // Defense in depth: never run with an invalid configuration.
    config.validate().map_err(std::io::Error::other)?;

    if config.read_workers > 1 {
        warn!(
            "read_workers={} is deprecated: the mux byte stream requires ordered reads, \
             so a single reader is used. This setting is ignored.",
            config.read_workers
        );
    }

    // Remove a stale unix socket before binding. Names pipes need no cleanup.
    if let Endpoint::Unix(ref path) = config.endpoint {
        if path.exists() {
            std::fs::remove_file(path)?;
            info!("removed existing socket: {}", path.display());
        }
    }

    let mut listener = TransportListener::bind(&config.endpoint, &config.pipe_security).await?;

    // For unix sockets: apply file permissions + optional group ownership.
    // For named pipes: the DACL was applied atomically at CreateNamedPipe.
    #[cfg(unix)]
    let _socket_guard = {
        if let Endpoint::Unix(ref path) = config.endpoint {
            platform::apply_endpoint_security(
                path,
                config.socket_mode,
                config.resolve_group_gid(),
            )?;
            Some(security::SocketCleanupGuard::new(path.clone(), true))
        } else {
            None
        }
    };
    #[cfg(not(unix))]
    let _socket_guard: Option<()> = None; // named pipes need no cleanup

    info!("listening on {}", config.endpoint.display_string());
    platform::notify_service_ready();

    let scanner = Arc::new(tokio::sync::RwLock::new(DeviceScanner::new()));
    let (event_tx, _) = tokio::sync::broadcast::channel(config.broadcast_capacity);

    let device_manager = Arc::new(DeviceManager::new(config.clone(), metrics.clone()));

    let scan_task = {
        let scanner_clone = scanner.clone();
        let device_manager_clone = device_manager.clone();
        let event_tx_clone = event_tx.clone();
        let metrics_clone = metrics.clone();
        let scan_interval = config.scan_interval;

        tokio::spawn(async move {
            loop {
                let changes = {
                    let mut s = scanner_clone.write().await;
                    s.scan()
                };

                for change in &changes {
                    match change {
                        DeviceChange::Attached(dev) => {
                            device_manager_clone.add_device(dev).await;
                        }
                        DeviceChange::Detached { device_id } => {
                            device_manager_clone.remove_device(*device_id).await;
                        }
                    }
                    let _ = event_tx_clone.send(change.clone());
                }

                let count = {
                    let s = scanner_clone.read().await;
                    s.get_devices().len()
                };
                metrics_clone.devices_attached.store(count as i64, std::sync::atomic::Ordering::Relaxed);

                tokio::time::sleep(scan_interval).await;
            }
        })
    };

    let max_clients = config.max_clients;
    let semaphore = Arc::new(Semaphore::new(max_clients));

    info!("daemon started: endpoint={} max_clients={}", config.endpoint.display_string(), max_clients);

    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let (stream, peer) = tokio::select! {
            res = listener.accept() => match res {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::debug!("accept error (continuing): {e}");
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!("client limit reached ({max_clients}), rejecting connection");
                metrics.clients_rejected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                drop(stream);
                continue;
            }
        };

        let scanner = scanner.clone();
        let event_tx = event_tx.clone();
        let device_manager = device_manager.clone();
        let metrics = metrics.clone();
        let config = config.clone();

        tokio::spawn(async move {
            let _permit = permit;
            handle_client(stream, scanner, event_tx, device_manager, metrics, config, peer).await;
        });
    }

    scan_task.abort();
    drop(listener);
    info!("daemon stopped");
    // _socket_guard drops here, removing a unix socket file if we created one.
    Ok(())
}
