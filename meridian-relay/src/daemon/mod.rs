pub mod connection;
pub mod device_manager;
pub mod device_scanner;
pub mod mux;
pub mod protocol;
pub mod usb;

use std::sync::{Arc, RwLock};
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tracing::{info, error};

use device_scanner::{DeviceScanner, DeviceChange};
use device_manager::DeviceManager;

const SOCKET_PATH: &str = "/var/run/usbmuxd";

pub async fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = UnixListener::bind(SOCKET_PATH)?;
    info!("usbmuxd daemon listening on {SOCKET_PATH}");

    // Set permissions so non-root clients can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o666);
        let _ = std::fs::set_permissions(SOCKET_PATH, perms);
    }

    // SAFETY: set early before threads
    unsafe { std::env::set_var("USBMUXD_SOCKET_ADDRESS", SOCKET_PATH); }

    let scanner = Arc::new(RwLock::new(DeviceScanner::new()));
    let (event_tx, _) = broadcast::channel::<DeviceChange>(256);
    let device_manager = Arc::new(DeviceManager::new());

    // Initial scan — drop lock before any .await
    {
        let changes = {
            let mut s = scanner.write().unwrap();
            s.scan()
        };
        for change in &changes {
            match change {
                DeviceChange::Attached(dev) => {
                    info!("initial scan: + {} (id={})", dev.udid, dev.device_id);
                    device_manager.add_device(dev).await;
                }
                DeviceChange::Detached { device_id } => {
                    info!("initial scan: - id={device_id}");
                    device_manager.remove_device(*device_id).await;
                }
            }
        }
    }

    // Background scanner task
    let scanner_clone = scanner.clone();
    let event_tx_clone = event_tx.clone();
    let device_manager_clone = device_manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let changes = {
                let mut s = scanner_clone.write().unwrap();
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
        }
    });

    info!("waiting for connections...");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let scanner = scanner.clone();
                let event_tx = event_tx.clone();
                let device_manager = device_manager.clone();
                tokio::spawn(async move {
                    connection::handle_client(stream, scanner, event_tx, device_manager).await;
                });
            }
            Err(e) => {
                error!("failed to accept connection: {e}");
            }
        }
    }
}
