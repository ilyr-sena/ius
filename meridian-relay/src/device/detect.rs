use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::debug;

use crate::daemon::device_scanner::UsbDevice;

const USBMUXD_SOCKET: &str = "/var/run/usbmuxd";

pub fn ensure_daemon_socket_env() {
    // No-op: daemon always binds to /var/run/usbmuxd
}

pub fn raw_to_usb_device(raw: &UsbmuxdDeviceInfo) -> UsbDevice {
    UsbDevice {
        device_id: raw.device_id,
        udid: raw.udid.clone(),
        product_id: raw.product_id,
        usb_bus: 0,
        usb_address: 0,
    }
}

pub struct UsbmuxdClient {
    stream: UnixStream,
    tag: u32,
}

impl UsbmuxdClient {
    pub async fn connect() -> Result<Self, std::io::Error> {
        let stream = UnixStream::connect(USBMUXD_SOCKET).await?;
        Ok(Self { stream, tag: 1 })
    }

    fn next_tag(&mut self) -> u32 {
        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1);
        tag
    }

    pub async fn send_list_devices(&mut self) -> Result<Vec<UsbmuxdDeviceInfo>, std::io::Error> {
        let tag = self.next_tag();
        let mut plist = plist::Dictionary::new();
        plist.insert("MessageType".into(), plist::Value::String("ListDevices".into()));
        plist.insert("ClientVersion".into(), plist::Value::Integer(1.into()));

        write_plist_packet(&mut self.stream, &plist, tag).await?;

        let response = read_plist_packet(&mut self.stream).await?;

        let devices = match response.get("DeviceList") {
            Some(plist::Value::Array(arr)) => {
                let mut result = Vec::new();
                for item in arr {
                    if let plist::Value::Dictionary(entry) = item {
                        if let Some(plist::Value::Dictionary(props)) = entry.get("Properties") {
                            result.push(UsbmuxdDeviceInfo::from_plist(props));
                        }
                    }
                }
                result
            }
            _ => Vec::new(),
        };

        debug!("ListDevices returned {} device(s)", devices.len());
        Ok(devices)
    }

    pub async fn send_listen(&mut self) -> Result<(), std::io::Error> {
        let tag = self.next_tag();
        let mut plist = plist::Dictionary::new();
        plist.insert("MessageType".into(), plist::Value::String("Listen".into()));

        write_plist_packet(&mut self.stream, &plist, tag).await?;
        Ok(())
    }

    pub async fn read_event(&mut self) -> Result<ListenEvent, std::io::Error> {
        let response = read_plist_packet(&mut self.stream).await?;

        let msg = match response.get("MessageType") {
            Some(plist::Value::String(s)) => s.clone(),
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no MessageType")),
        };

        match msg.as_str() {
            "Attached" => {
                let device_id = response.get("DeviceID")
                    .and_then(|v| v.as_unsigned_integer())
                    .unwrap_or(0) as u32;
                let props = response.get("Properties")
                    .and_then(|v| v.as_dictionary())
                    .cloned()
                    .unwrap_or_default();
                Ok(ListenEvent::Attached(UsbmuxdDeviceInfo::from_plist_with_id(&props, device_id)))
            }
            "Detached" => {
                let device_id = response.get("DeviceID")
                    .and_then(|v| v.as_unsigned_integer())
                    .unwrap_or(0) as u32;
                Ok(ListenEvent::Detached(device_id))
            }
            other => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unexpected event: {other}")))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ListenEvent {
    Attached(UsbmuxdDeviceInfo),
    Detached(u32),
}

#[derive(Debug, Clone)]
pub struct UsbmuxdDeviceInfo {
    pub device_id: u32,
    pub udid: String,
    pub product_id: u16,
    pub connection_type: String,
}

impl UsbmuxdDeviceInfo {
    fn from_plist(props: &plist::Dictionary) -> Self {
        let device_id = props.get("DeviceID")
            .and_then(|v| v.as_unsigned_integer())
            .unwrap_or(0) as u32;
        Self::from_plist_with_id(props, device_id)
    }

    fn from_plist_with_id(props: &plist::Dictionary, device_id: u32) -> Self {
        let udid = props.get("SerialNumber")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();
        let product_id = props.get("ProductID")
            .and_then(|v| v.as_unsigned_integer())
            .unwrap_or(0) as u16;
        let connection_type = props.get("ConnectionType")
            .and_then(|v| v.as_string())
            .unwrap_or("USB")
            .to_string();

        Self { device_id, udid, product_id, connection_type }
    }
}

async fn write_plist_packet(
    stream: &mut UnixStream,
    plist: &plist::Dictionary,
    tag: u32,
) -> Result<(), std::io::Error> {
    let mut plist_bytes = Vec::new();
    plist::to_writer_xml(&mut plist_bytes, &plist::Value::Dictionary(plist.clone()))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let total_size = (plist_bytes.len() + 16) as u32;

    let mut packet = Vec::with_capacity(total_size as usize);
    packet.extend_from_slice(&total_size.to_le_bytes());
    packet.extend_from_slice(&1u32.to_le_bytes());
    packet.extend_from_slice(&8u32.to_le_bytes());
    packet.extend_from_slice(&tag.to_le_bytes());
    packet.extend_from_slice(&plist_bytes);

    stream.write_all(&packet).await?;
    Ok(())
}

async fn read_plist_packet(stream: &mut UnixStream) -> Result<plist::Dictionary, std::io::Error> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;

    let size = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if size < 16 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("packet size too small: {size} bytes"),
        ));
    }
    let body_size = (size - 16) as usize;
    let mut body = vec![0u8; body_size];
    stream.read_exact(&mut body).await?;

    let plist: plist::Dictionary = if body.len() > 0 && body[0] == b'b' {
        let mut cursor = std::io::Cursor::new(&body);
        plist::from_reader(&mut cursor)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("binary plist parse error: {e}")))?
    } else {
        plist::from_bytes(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("xml plist parse error: {e}")))?
    };

    Ok(plist)
}
