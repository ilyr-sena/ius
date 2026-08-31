pub mod connection;
pub mod device_scanner;
pub mod protocol;

use std::sync::{Arc, RwLock};
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tracing::{info, error};

use device_scanner::{DeviceScanner, DeviceChange};

const SOCKET_PATH: &str = "/tmp/meridian-relay-usbmuxd.sock";

pub async fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // Clean up old socket
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = UnixListener::bind(SOCKET_PATH)?;
    info!("usbmuxd daemon listening on {SOCKET_PATH}");

    // Set env var so idevice crate connects to our socket
    // SAFETY: we set this early before any threads are spawned
    unsafe { std::env::set_var("USBMUXD_SOCKET_ADDRESS", SOCKET_PATH); }

    let scanner = Arc::new(RwLock::new(DeviceScanner::new()));
    let (event_tx, _) = broadcast::channel::<DeviceChange>(256);

    // Initial scan
    {
        let mut s = scanner.write().unwrap();
        let changes = s.scan();
        for change in &changes {
            match change {
                DeviceChange::Attached(dev) => info!("initial scan: + {} (id={})", dev.udid, dev.device_id),
                DeviceChange::Detached { device_id } => info!("initial scan: - id={device_id}"),
            }
        }
    }

    // Background scanner task
    let scanner_clone = scanner.clone();
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let changes = {
                let mut s = scanner_clone.write().unwrap();
                s.scan()
            };
            for change in changes {
                let _ = event_tx_clone.send(change);
            }
        }
    });

    // Accept connections
    info!("waiting for connections...");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let scanner = scanner.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    connection::handle_client(stream, scanner, event_tx).await;
                });
            }
            Err(e) => {
                error!("failed to accept connection: {e}");
            }
        }
    }
}
