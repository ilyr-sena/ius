use std::time::Duration;
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::daemon::protocol::{self, RawPacket, PLIST_MESSAGE_TYPE, XML_PLIST_VERSION};
use crate::config::DEFAULT_SOCKET_PATH;

/// Maximum plist event frame accepted from the daemon.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

pub async fn watch_devices() -> Result<(), Box<dyn std::error::Error>> {
    watch_devices_from(DEFAULT_SOCKET_PATH, None).await
}

/// Watch device attach/detach events from the daemon socket.
///
/// `udid_filter`, when set, restricts printed events to the matching UDID.
/// Automatically reconnects with exponential backoff (100 ms → 5 s) if the
/// connection to the daemon drops.
pub async fn watch_devices_from(socket_path: &str, udid_filter: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = std::env::var("USBMUXD_SOCKET_ADDRESS")
        .unwrap_or_else(|_| socket_path.to_string());
    let udid_filter = udid_filter.map(|s| s.to_string());

    let mut backoff = Duration::from_millis(100);
    let max_backoff = Duration::from_secs(5);

    loop {
        match try_connect_and_listen(&socket_path, udid_filter.as_deref()).await {
            Ok(()) => {
                info!("device watch ended normally");
                return Ok(());
            }
            Err(e) => {
                warn!("device watch error: {e}, reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

async fn try_connect_and_listen(socket_path: &str, udid_filter: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    debug!("connecting to usbmuxd at {socket_path}");

    let mut stream = UnixStream::connect(socket_path).await?;

    let mut plist = plist::Dictionary::new();
    plist.insert("ClientVersion".into(), plist::Value::Integer(1.into()));
    plist.insert("MessageType".into(), plist::Value::String("Listen".into()));
    plist.insert("ProgName".into(), plist::Value::String("meridian-relay".into()));
    plist.insert("kLibUSBMuxVersion".into(), plist::Value::Integer(0.into()));

    let request = RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 1);
    request.write_to(&mut stream).await?;

    loop {
        let response = protocol::read_packet(&mut stream, MAX_EVENT_BYTES).await
            .map_err(|e| format!("failed to read event packet: {e}"))?;
        let plist_dict = response.plist;

        if let Some(result) = plist_dict.get("Result") {
            if let Some(status) = result.as_unsigned_integer() {
                if status != 0 {
                    return Err(format!("listen failed with status {status}").into());
                }
            }
        }

        let message_type = plist_dict.get("MessageType")
            .and_then(|v| v.as_string())
            .unwrap_or("Unknown")
            .to_string();

        match message_type.as_str() {
            "Attached" => {
                if let Some(props) = plist_dict.get("Properties").and_then(|v| v.as_dictionary()) {
                    let udid = props.get("SerialNumber")
                        .and_then(|v| v.as_string())
                        .unwrap_or("")
                        .to_string();

                    let device_id = props.get("DeviceID")
                        .and_then(|v| v.as_unsigned_integer())
                        .unwrap_or(0) as u32;

                    if udid_filter.map_or(true, |f| f == udid) {
                        info!("device attached: {udid} (device_id={device_id})");
                        println!("+ ATTACHED   {udid} (device_id={device_id})");
                    }
                }
            }
            "Detached" => {
                let device_id = plist_dict.get("DeviceID")
                    .and_then(|v| v.as_unsigned_integer())
                    .unwrap_or(0) as u32;
                let udid = plist_dict.get("SerialNumber")
                    .and_then(|v| v.as_string())
                    .unwrap_or("");

                if udid_filter.map_or(true, |f| udid.is_empty() || f == udid) {
                    info!("device detached: {udid} (device_id={device_id})");
                    println!("- DETACHED   {udid} (device_id={device_id})");
                }
            }
            _ => {
                debug!("unknown message type: {message_type}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_watch_devices_from_invalid_path() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watch_devices_from("/nonexistent/socket", None),
        ).await;
        assert!(result.is_err() || result.unwrap().is_err());
    }
}
