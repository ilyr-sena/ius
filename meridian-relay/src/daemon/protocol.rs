use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

pub const XML_PLIST_VERSION: u32 = 1;
pub const PLIST_MESSAGE_TYPE: u32 = 8;

/// Result codes returned in `Result`/`Number` responses.
/// Codes 0–3, 6 match usbmuxd conventions for client compatibility.
pub mod result {
    /// Operation succeeded.
    pub const OK: u32 = 0;
    /// Badly formed or unknown command.
    pub const BAD_COMMAND: u32 = 1;
    /// Referenced an unknown/invalid device.
    pub const BAD_DEVICE: u32 = 2;
    /// Device refused the connection (also used for general connect failure).
    pub const CONNECTION_REFUSED: u32 = 3;
    /// Meridian extension: lockdown connection requires an existing pair record.
    pub const PAIR_RECORD_REQUIRED: u32 = 8;
}

#[derive(Debug)]
pub struct RawPacket {
    pub size: u32,
    pub version: u32,
    pub message: u32,
    pub tag: u32,
    pub plist: plist::Dictionary,
}

impl RawPacket {
    pub fn new(plist: plist::Dictionary, version: u32, message: u32, tag: u32) -> Self {
        RawPacket {
            size: 0,
            version,
            message,
            tag,
            plist,
        }
    }

    pub async fn write_to(&self, stream: &mut (impl AsyncWriteExt + Unpin)) -> Result<(), std::io::Error> {
        let mut plist_bytes = Vec::new();
        plist::to_writer_xml(&mut plist_bytes, &plist::Value::Dictionary(self.plist.clone()))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let total_size = (plist_bytes.len() + 16) as u32;

        let mut packet = Vec::with_capacity(total_size as usize);
        packet.extend_from_slice(&total_size.to_le_bytes());
        packet.extend_from_slice(&self.version.to_le_bytes());
        packet.extend_from_slice(&self.message.to_le_bytes());
        packet.extend_from_slice(&self.tag.to_le_bytes());
        packet.extend_from_slice(&plist_bytes);

        stream.write_all(&packet).await?;
        Ok(())
    }
}

pub async fn read_packet(
    stream: &mut (impl AsyncReadExt + Unpin),
    max_packet_bytes: usize,
) -> Result<RawPacket, std::io::Error> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;

    let size = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let message = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let tag = u32::from_le_bytes(header[12..16].try_into().unwrap());

    if size < 16 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("packet size too small: {size} bytes"),
        ));
    }

    let body_size = (size - 16) as usize;
    if body_size > max_packet_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("packet body too large: {body_size} bytes (max {max_packet_bytes})"),
        ));
    }

    let mut body = vec![0u8; body_size];
    stream.read_exact(&mut body).await?;

    let plist: plist::Dictionary = if !body.is_empty() && body[0] == b'b' {
        let mut cursor = std::io::Cursor::new(&body);
        plist::from_reader(&mut cursor)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("binary plist parse error: {e}")))?
    } else {
        plist::from_bytes(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("xml plist parse error: {e}")))?
    };

    debug!("read packet: size={size} version={version} message={message} tag={tag}");

    Ok(RawPacket {
        size,
        version,
        message,
        tag,
        plist,
    })
}

pub fn make_result_response(tag: u32, number: u32) -> RawPacket {
    let mut plist = plist::Dictionary::new();
    plist.insert("MessageType".into(), plist::Value::String("Result".into()));
    plist.insert("Number".into(), plist::Value::Integer(number.into()));
    RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, tag)
}

pub fn make_device_list_response(tag: u32, devices: &[crate::daemon::device_scanner::UsbDevice]) -> RawPacket {
    let mut device_array = Vec::new();
    for dev in devices {
        let mut props = plist::Dictionary::new();
        props.insert("DeviceID".into(), plist::Value::Integer(dev.device_id.into()));
        props.insert("ConnectionType".into(), plist::Value::String("USB".into()));
        props.insert("SerialNumber".into(), plist::Value::String(dev.udid.clone()));
        props.insert("ProductID".into(), plist::Value::Integer(dev.product_id.into()));
        props.insert("LocationID".into(), plist::Value::Integer(0.into()));
        props.insert("Port".into(), plist::Value::Integer(0.into()));

        let mut device_entry = plist::Dictionary::new();
        device_entry.insert("Properties".into(), plist::Value::Dictionary(props));
        device_array.push(plist::Value::Dictionary(device_entry));
    }

    let mut plist = plist::Dictionary::new();
    plist.insert("DeviceList".into(), plist::Value::Array(device_array));
    RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, tag)
}

pub fn make_attached_event(tag: u32, device: &crate::daemon::device_scanner::UsbDevice) -> RawPacket {
    let mut props = plist::Dictionary::new();
    props.insert("DeviceID".into(), plist::Value::Integer(device.device_id.into()));
    props.insert("ConnectionType".into(), plist::Value::String("USB".into()));
    props.insert("SerialNumber".into(), plist::Value::String(device.udid.clone()));
    props.insert("ProductID".into(), plist::Value::Integer(device.product_id.into()));
    props.insert("LocationID".into(), plist::Value::Integer(0.into()));
    props.insert("Port".into(), plist::Value::Integer(0.into()));

    let mut plist = plist::Dictionary::new();
    plist.insert("MessageType".into(), plist::Value::String("Attached".into()));
    plist.insert("DeviceID".into(), plist::Value::Integer(device.device_id.into()));
    plist.insert("Properties".into(), plist::Value::Dictionary(props));
    RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, tag)
}

pub fn make_detached_event(tag: u32, device_id: u32) -> RawPacket {
    let mut plist = plist::Dictionary::new();
    plist.insert("MessageType".into(), plist::Value::String("Detached".into()));
    plist.insert("DeviceID".into(), plist::Value::Integer(device_id.into()));
    RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, tag)
}

pub fn make_stats_response(tag: u32, json: &str) -> RawPacket {
    let mut plist = plist::Dictionary::new();
    plist.insert("MessageType".into(), plist::Value::String("Result".into()));
    plist.insert("Number".into(), plist::Value::Integer(0.into()));
    plist.insert("Stats".into(), plist::Value::String(json.to_string()));
    RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, tag)
}

pub fn make_stats_command(tag: u32) -> RawPacket {
    let mut plist = plist::Dictionary::new();
    plist.insert("MessageType".into(), plist::Value::String("MeridianStats".into()));
    RawPacket::new(plist, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, tag)
}

pub async fn write_packet(
    stream: &mut (impl AsyncWriteExt + Unpin),
    packet: &RawPacket,
) -> Result<(), std::io::Error> {
    packet.write_to(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;


    #[tokio::test]
    async fn test_write_read_roundtrip() {
        let cfg = crate::config::DaemonConfig::default();
        let mut plist = plist::Dictionary::new();
        plist.insert("MessageType".into(), plist::Value::String("ListDevices".into()));
        plist.insert("ClientVersion".into(), plist::Value::Integer(1.into()));
        let pkt = RawPacket::new(plist, 1, 8, 42);

        let (mut client, mut server) = tokio::io::duplex(65536);
        pkt.write_to(&mut client).await.unwrap();

        let resp = read_packet(&mut server, cfg.max_packet_bytes).await.unwrap();
        assert_eq!(resp.tag, 42);
        assert_eq!(resp.version, 1);
        assert_eq!(resp.message, 8);
        assert_eq!(
            resp.plist.get("MessageType").and_then(|v| v.as_string()),
            Some("ListDevices")
        );
    }

    #[tokio::test]
    async fn test_read_rejects_huge_packet() {
        let cfg = crate::config::DaemonConfig::default();
        let (mut client, mut server) = tokio::io::duplex(65536);

        let size: u32 = 16 + 256 * 1024 * 1024;
        let header = [
            size.to_le_bytes().as_slice(),
            &1u32.to_le_bytes(),
            &8u32.to_le_bytes(),
            &1u32.to_le_bytes(),
        ].concat();
        use tokio::io::AsyncWriteExt;
        client.write_all(&header).await.unwrap();

        let result = read_packet(&mut server, cfg.max_packet_bytes).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_make_result_response() {
        let pkt = make_result_response(5, 0);
        assert_eq!(pkt.tag, 5);
        assert_eq!(
            pkt.plist.get("MessageType").and_then(|v| v.as_string()),
            Some("Result")
        );
        assert_eq!(
            pkt.plist.get("Number").and_then(|v| v.as_unsigned_integer()),
            Some(0)
        );
    }

    #[test]
    fn test_make_device_list_empty() {
        let pkt = make_device_list_response(1, &[]);
        let list = pkt.plist.get("DeviceList").unwrap().as_array().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_make_stats_response() {
        let pkt = make_stats_response(1, r#"{"uptime":42}"#);
        assert_eq!(
            pkt.plist.get("Stats").and_then(|v| v.as_string()),
            Some(r#"{"uptime":42}"#)
        );
    }

    #[test]
    fn test_make_stats_command() {
        let pkt = make_stats_command(7);
        assert_eq!(pkt.tag, 7);
        assert_eq!(
            pkt.plist.get("MessageType").and_then(|v| v.as_string()),
            Some("MeridianStats")
        );
    }

    #[test]
    fn test_result_codes_match_usbmuxd() {
        assert_eq!(result::OK, 0);
        assert_eq!(result::BAD_COMMAND, 1);
        assert_eq!(result::BAD_DEVICE, 2);
        assert_eq!(result::CONNECTION_REFUSED, 3);
    }
}
