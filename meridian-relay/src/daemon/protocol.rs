use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

pub const XML_PLIST_VERSION: u32 = 1;
pub const PLIST_MESSAGE_TYPE: u32 = 8;

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

pub async fn read_packet(stream: &mut (impl AsyncReadExt + Unpin)) -> Result<RawPacket, std::io::Error> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;

    let size = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let message = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let tag = u32::from_le_bytes(header[12..16].try_into().unwrap());

    let body_size = (size - 16) as usize;
    let mut body = vec![0u8; body_size];
    stream.read_exact(&mut body).await?;

    let plist: plist::Dictionary = if body.len() > 0 && body[0] == b'b' {
        // Binary plist starts with 'b' - needs a cursor for seeking
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
