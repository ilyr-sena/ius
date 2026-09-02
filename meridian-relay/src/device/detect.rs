use tracing::{debug, info};

use crate::daemon::protocol::{self, RawPacket, PLIST_MESSAGE_TYPE, XML_PLIST_VERSION};
use crate::daemon::transport::Endpoint;
use crate::device::{ConnectionType, Device};
use crate::platform;

/// Maximum plist response accepted from the daemon (16 MiB, matches server cap).
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub async fn list_devices() -> Result<Vec<Device>, Box<dyn std::error::Error>> {
    list_devices_from(&platform::default_endpoint()).await
}

pub async fn list_devices_from(endpoint_str: &str) -> Result<Vec<Device>, Box<dyn std::error::Error>> {
    // Back-compat: USBMUXD_SOCKET_ADDRESS may hold a bare path or full endpoint.
    let endpoint_str = std::env::var("USBMUXD_SOCKET_ADDRESS")
        .unwrap_or_else(|_| endpoint_str.to_string());

    debug!("connecting to usbmuxd at {endpoint_str}");

    let endpoint = Endpoint::parse(&endpoint_str)?;
    let mut stream = endpoint.connect().await?;

    let mut plist = plist::Dictionary::new();
    plist.insert("ClientVersion".into(), plist::Value::Integer(1.into()));
    plist.insert("MessageType".into(), plist::Value::String("ListDevices".into()));
    plist.insert("ProgName".into(), plist::Value::String("meridian-relay".into()));
    plist.insert("kLibUSBMuxVersion".into(), plist::Value::Integer(0.into()));

    let request = RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 1);
    request.write_to(&mut stream).await?;

    let response = protocol::read_packet(&mut stream, MAX_RESPONSE_BYTES).await?;
    let plist_dict = response.plist;

    if let Some(result) = plist_dict.get("Result") {
        if let Some(status) = result.as_unsigned_integer() {
            if status != 0 {
                return Err(format!("list devices failed with status {status}").into());
            }
        }
    }

    let device_list = plist_dict.get("DeviceList")
        .and_then(|v| v.as_array())
        .ok_or("DeviceList missing or not an array")?;

    let mut devices = Vec::new();
    for device_entry in device_list {
        let device_dict = match device_entry.as_dictionary() {
            Some(d) => d,
            None => continue,
        };

        let props = match device_dict.get("Properties") {
            Some(plist::Value::Dictionary(d)) => d,
            _ => continue,
        };

        let udid = props.get("SerialNumber")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();

        let model = props.get("ProductType")
            .and_then(|v| v.as_string())
            .map(|s| s.to_string());

        let device_id = props.get("DeviceID")
            .and_then(|v| v.as_unsigned_integer())
            .unwrap_or(0) as u32;

        let connection_type_str = props.get("ConnectionType")
            .and_then(|v| v.as_string())
            .unwrap_or("unknown");

        let connection_type = match connection_type_str {
            "USB" => ConnectionType::Usb,
            _ => ConnectionType::Network,
        };

        debug!("found device: {udid} (device_id={device_id})");

        devices.push(Device {
            udid,
            device_id,
            name: None,
            model,
            ios_version: None,
            build_version: None,
            connection_type,
        });
    }

    info!("found {} device(s)", devices.len());
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_devices_from_invalid_path() {
        let result = tokio_test::block_on(list_devices_from("/nonexistent/socket"));
        assert!(result.is_err());
    }
}
