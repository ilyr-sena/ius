pub mod protocol;
pub mod device_scanner;
pub mod device_manager;
pub mod connection;
pub mod mux;
pub mod usb;

use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::DaemonConfig;
use crate::metrics::Metrics;
use crate::security::{PeerCredentials, SocketCleanupGuard};
use self::device_scanner::{DeviceScanner, DeviceChange};
use self::device_manager::DeviceManager;
use self::connection::handle_client;

pub async fn run_daemon(config: DaemonConfig, metrics: Arc<Metrics>) -> Result<(), Box<dyn std::error::Error>> {
    // Defense in depth: never run with an invalid configuration.
    config.validate().map_err(std::io::Error::other)?;

    if config.read_workers > 1 {
        warn!(
            "read_workers={} is deprecated: the mux byte stream requires ordered reads, \
             so a single reader is used. This setting is ignored.",
            config.read_workers
        );
    }

    if config.socket_path.exists() {
        std::fs::remove_file(&config.socket_path)?;
        info!("removed existing socket: {}", config.socket_path.display());
    }

    let listener = UnixListener::bind(&config.socket_path)?;

    // Remove the socket file on any exit path (including graceful shutdown).
    let _socket_guard = SocketCleanupGuard::new(config.socket_path.clone(), true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(config.socket_mode))?;

        // Optional group ownership for socket-based access control.
        if let Some(gid) = config.resolve_group_gid() {
            chown_to_group(&config.socket_path, gid)?;
        }
    }

    info!("listening on {}", config.socket_path.display());
    notify_systemd_ready();

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

    info!("daemon started: socket={} max_clients={}", config.socket_path.display(), max_clients);

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        let accept_result = {
            #[cfg(unix)]
            {
                tokio::select! {
                    res = listener.accept() => Some(res),
                    _ = tokio::signal::ctrl_c() => { info!("received SIGINT, shutting down"); None }
                    _ = sigterm.recv() => { info!("received SIGTERM, shutting down"); None }
                }
            }
            #[cfg(not(unix))]
            {
                tokio::select! {
                    res = listener.accept() => Some(res),
                    _ = tokio::signal::ctrl_c() => { info!("received SIGINT, shutting down"); None }
                }
            }
        };

        let (stream, _addr) = match accept_result {
            Some(Ok(pair)) => pair,
            Some(Err(e)) => {
                // Client vanished between SYN and accept — keep serving.
                tracing::debug!("accept error (continuing): {e}");
                continue;
            }
            None => break,
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

        let peer = PeerCredentials::from_unix_stream(&stream);

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
    // _socket_guard drops here, removing the socket file.
    Ok(())
}

/// Apply socket group ownership (a no-op if the group couldn't be resolved).
#[cfg(unix)]
fn chown_to_group(path: &std::path::Path, gid: u32) -> std::io::Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // (uid_t)-1 tells chown to keep the current owner.
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    info!("socket group set to gid={gid}");
    Ok(())
}

/// Send systemd `READY=1` if we were started with socket notification.
fn notify_systemd_ready() {
    let Ok(notify_socket) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    #[cfg(unix)]
    {
        if let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() {
            // An abstract socket address starts with NUL.
            let addr = notify_socket.strip_prefix('@')
                .map(|name| format!("\0{name}"))
                .unwrap_or(notify_socket);
            if let Err(e) = sock.send_to(b"READY=1", addr) {
                warn!("failed to send systemd READY=1: {e}");
            }
        }
    }
}
