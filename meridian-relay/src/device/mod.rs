pub mod detect;
pub mod error;
pub mod info;
pub mod monitor;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum ConnectionType {
    Usb,
    Network,
}

impl std::fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionType::Usb => write!(f, "USB"),
            ConnectionType::Network => write!(f, "Network"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub udid: String,
    pub device_id: u32,
    pub name: Option<String>,
    pub model: Option<String>,
    pub ios_version: Option<String>,
    pub build_version: Option<String>,
    pub connection_type: ConnectionType,
}

impl Device {
    pub fn from_usb_device(dev: &crate::daemon::device_scanner::UsbDevice) -> Self {
        Self {
            udid: format_device_udid(&dev.udid),
            device_id: dev.device_id,
            name: None,
            model: None,
            ios_version: None,
            build_version: None,
            connection_type: ConnectionType::Usb,
        }
    }
}

fn format_device_udid(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('\0');
    if trimmed.len() == 24 && !trimmed.contains('-') {
        format!("{}-{}", &trimmed[..8], &trimmed[8..])
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Connected(Device),
    Disconnected {
        udid: String,
        device_id: u32,
    },
}

impl std::fmt::Display for DeviceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceEvent::Connected(dev) => {
                let name = dev.name.as_deref().unwrap_or("Unknown");
                let model = dev.model.as_deref().unwrap_or("?");
                let ios = dev.ios_version.as_deref().unwrap_or("?");
                write!(
                    f,
                    "+ CONNECTED  {} ({}) [{}] iOS {}",
                    name, dev.udid, model, ios
                )
            }
            DeviceEvent::Disconnected { udid, .. } => {
                write!(f, "- DISCONNECTED  {udid}")
            }
        }
    }
}
