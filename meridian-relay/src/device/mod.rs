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
    pub fn from_usbmuxd(dev: &idevice::usbmuxd::UsbmuxdDevice) -> Self {
        let connection_type = match dev.connection_type {
            idevice::usbmuxd::Connection::Usb => ConnectionType::Usb,
            idevice::usbmuxd::Connection::Network(_) => ConnectionType::Network,
            _ => ConnectionType::Usb,
        };
        Self {
            udid: dev.udid.clone(),
            device_id: dev.device_id,
            name: None,
            model: None,
            ios_version: None,
            build_version: None,
            connection_type,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Connected(Device),
    Disconnected {
        udid: String,
        #[allow(dead_code)]
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
