use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::metrics::Metrics;

pub const HEADER_SIZE: usize = 8;
pub const VERSION_REQUEST_SIZE: usize = 12;
pub const SETUP_REQUEST_SIZE: usize = 1;

pub const HEADER_VERSION: u32 = 0;
pub const HEADER_CONTROL: u32 = 1;
pub const HEADER_SETUP: u32 = 2;
pub const HEADER_TCP: u32 = 6;

pub const CONTROL_RESULT: u8 = 1;
pub const CONTROL_DEVICE_ADD: u8 = 2;
pub const CONTROL_DEVICE_REMOVE: u8 = 3;

pub const SETUP_CONNECT: u8 = 7;

pub const TCP_FIN: u16 = 0x001;
pub const TCP_SYN: u16 = 0x002;
pub const TCP_RST: u16 = 0x004;
pub const TCP_PSH: u16 = 0x008;
pub const TCP_ACK: u16 = 0x010;

pub const USB_MTU: usize = 49152;
pub const USB_MRU: usize = 16384;
pub const ZLP_THRESHOLD: usize = 512;

/// Matches reference usbmuxd: 128 KiB initial receive window, >>8 goes on the wire.
pub const INITIAL_WINDOW: u32 = 131072;

#[derive(Debug, Clone)]
pub enum DeviceState {
    Idle,
    Active,
}

#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    Connected { device_id: u32, sport: u16, tag: u32 },
    Refused { device_id: u32, sport: u16, tag: u32 },
    DataReady { device_id: u32, sport: u16 },
    Disconnected { device_id: u32, sport: u16 },
    Error { device_id: u32, sport: u16, error: String },
}

#[derive(Debug, Clone)]
pub enum MuxPacket {
    Version { major: u32, minor: u32 },
    Control { payload: Vec<u8> },
    Setup,
    Tcp { header: TcpHeader, payload: Vec<u8> },
}

#[derive(Debug, Clone)]
pub enum MuxEvent {
    DeviceAdded {
        device_id: u32,
        product_id: u16,
        connection_type: String,
        usb_speed: String,
    },
    DeviceRemoved {
        device_id: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnState {
    SynSent,
    SynAckReceived,
    Established,
    Connected,
    Refused,
    FinWait1,
    CloseWait,
    Closing,
    Closed,
    Dead,
}

impl ConnState {
    pub fn is_active(&self) -> bool {
        matches!(self, ConnState::Established | ConnState::Connected | ConnState::CloseWait)
    }
}

#[derive(Debug)]
pub struct ConnectionManager {
    pub devices: Arc<tokio::sync::RwLock<HashMap<u32, MuxDevice>>>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    pub async fn add_device(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        devices.entry(device_id).or_insert_with(|| MuxDevice {
            device_id,
            connections: HashMap::new(),
            version: 1,
            state: DeviceState::Idle,
            tx_seq: 1,
            rx_seq: 1,
        });
    }

    pub async fn remove_device(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        devices.remove(&device_id);
    }

    pub async fn get_data_notify(&self, device_id: u32, sport: u16) -> Option<Arc<tokio::sync::Notify>> {
        let devices = self.devices.read().await;
        devices.get(&device_id)?
            .connections.get(&sport)
            .map(|c| c.data_notify.clone())
    }

    pub async fn start_connect(&self, device_id: u32, dport: u16, tag: u32) -> Result<u16, MuxError> {
        let mut devices = self.devices.write().await;
        let device = devices.get_mut(&device_id)
            .ok_or_else(|| MuxError::InvalidState("device not found".into()))?;

        let sport = (1024u16..65535)
            .find(|&p| !device.connections.contains_key(&p))
            .ok_or_else(|| MuxError::InvalidState("no available ports".into()))?;

        let conn = Connection {
            source_port: sport,
            dest_port: dport,
            state: ConnState::SynSent,
            tx_seq: 0,
            tx_ack: 0,
            ib_buf: Vec::new(),
            ob_buf: VecDeque::new(),
            data_notify: Arc::new(tokio::sync::Notify::new()),
            tag,
        };

        device.connections.insert(sport, conn);
        Ok(sport)
    }

    pub async fn handle_tcp_packet(
        &self,
        device_id: u32,
        tcp_header: &TcpHeader,
        payload: &[u8],
        metrics: &Arc<Metrics>,
        max_conn_buffer: usize,
    ) -> (Vec<ConnectionEvent>, Vec<Vec<u8>>) {
        let mut events = Vec::new();
        let mut send_queue = Vec::new();

        let sport = tcp_header.source_port;
        let flags = tcp_header.flags();

        let mut devices = self.devices.write().await;
        let device = match devices.get_mut(&device_id) {
            Some(d) => d,
            None => return (events, send_queue),
        };

        // Primary: exact (our source port, device source port) tuple match.
        // Fallback: legacy loose match for devices that echo ports unexpectedly.
        let conn_key = device
            .connections
            .keys()
            .find(|&&k| {
                device
                    .connections
                    .get(&k)
                    .map(|c| {
                        c.source_port == tcp_header.dest_port && c.dest_port == tcp_header.source_port
                    })
                    .unwrap_or(false)
            })
            .or_else(|| {
                device.connections.keys().find(|&&k| {
                    device
                        .connections
                        .get(&k)
                        .map(|c| c.source_port == sport || c.dest_port == sport)
                        .unwrap_or(false)
                })
            })
            .copied();

        if let Some(key) = conn_key {
            let conn = device.connections.get_mut(&key).unwrap();

            if flags & TCP_SYN != 0 && flags & TCP_ACK != 0 {
                conn.state = ConnState::Connected;
                conn.tx_ack = tcp_header.seq.wrapping_add(1);

                let ack_pkt = build_tcp_packet(
                    if device.version >= 2 { Some((device.tx_seq, device.rx_seq)) } else { None },
                    conn.source_port, conn.dest_port,   // us→them: our port → their listening port
                    conn.tx_seq, conn.tx_ack,
                    TCP_ACK, 65535, None,
                );
                device.tx_seq = device.tx_seq.wrapping_add(1);
                send_queue.push(ack_pkt);

                events.push(ConnectionEvent::Connected {
                    device_id,
                    sport: conn.source_port,
                    tag: conn.tag,
                });
            } else if flags & TCP_RST != 0 {
                conn.state = ConnState::Dead;
                events.push(ConnectionEvent::Disconnected { device_id, sport: conn.source_port });
            } else if flags & TCP_FIN != 0 {
                conn.state = ConnState::CloseWait;
                events.push(ConnectionEvent::Disconnected { device_id, sport: conn.source_port });
            } else if !payload.is_empty() {
                if conn.ib_buf.len() + payload.len() > max_conn_buffer {
                    // Flow control: refuse to buffer beyond the per-connection cap.
                    // We intentionally do not ACK these bytes.
                    metrics.overflow_rejections.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "device {device_id} sport={}: connection buffer full ({} bytes), dropping {} bytes",
                        conn.source_port,
                        conn.ib_buf.len(),
                        payload.len()
                    );
                } else {
                    // Advance the receive sequence and ACK with the remaining window
                    // so the device can keep sending.
                    conn.tx_ack = tcp_header.seq.wrapping_add(payload.len() as u32);
                    conn.ib_buf.extend_from_slice(payload);
                    let free = max_conn_buffer.saturating_sub(conn.ib_buf.len());
                    let ack_pkt = build_tcp_packet(
                        if device.version >= 2 { Some((device.tx_seq, device.rx_seq)) } else { None },
                        conn.source_port, conn.dest_port,
                        conn.tx_seq, conn.tx_ack,
                        TCP_ACK,
                        scaled_window(free),
                        None,
                    );
                    device.tx_seq = device.tx_seq.wrapping_add(1);
                    send_queue.push(ack_pkt);
                    conn.data_notify.notify_waiters();
                    events.push(ConnectionEvent::DataReady { device_id, sport: conn.source_port });
                }
            }
        } else {
            events.push(ConnectionEvent::Error {
                device_id,
                sport,
                error: "unknown connection".into(),
            });
        }

        (events, send_queue)
    }
}

#[derive(Debug)]
pub struct MuxDevice {
    pub device_id: u32,
    pub connections: HashMap<u16, Connection>,
    pub version: u32,
    pub state: DeviceState,
    pub tx_seq: u16,
    pub rx_seq: u16,
}

#[derive(Debug)]
pub struct Connection {
    pub source_port: u16,
    pub dest_port: u16,
    pub state: ConnState,
    pub tx_seq: u32,
    pub tx_ack: u32,
    pub ib_buf: Vec<u8>,
    pub ob_buf: VecDeque<u8>,
    pub data_notify: Arc<tokio::sync::Notify>,
    pub tag: u32,
}

#[derive(Debug, Clone)]
pub enum MuxError {
    InvalidHeader(String),
    InvalidVersion(String),
    InvalidState(String),
    IoError(String),
    UsbError(String),
    ChannelError(String),
    UnsupportedVersion(u32),
    UnsupportedProtocol(u32),
}

impl std::fmt::Display for MuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MuxError::InvalidHeader(msg) => write!(f, "invalid header: {msg}"),
            MuxError::InvalidVersion(msg) => write!(f, "invalid version: {msg}"),
            MuxError::InvalidState(msg) => write!(f, "invalid state: {msg}"),
            MuxError::IoError(msg) => write!(f, "I/O error: {msg}"),
            MuxError::UsbError(msg) => write!(f, "USB error: {msg}"),
            MuxError::ChannelError(msg) => write!(f, "channel error: {msg}"),
            MuxError::UnsupportedVersion(v) => write!(f, "unsupported version: {v}"),
            MuxError::UnsupportedProtocol(p) => write!(f, "unsupported protocol: {p}"),
        }
    }
}

impl std::error::Error for MuxError {}

impl From<std::io::Error> for MuxError {
    fn from(e: std::io::Error) -> Self {
        MuxError::IoError(e.to_string())
    }
}

#[derive(Debug, Clone)]
pub enum TcpFlag {
    Syn,
    SynAck,
    Ack,
    Fin,
    Rst,
    Psh,
}

impl TcpFlag {
    pub fn from_u16(flags: u16) -> Vec<TcpFlag> {
        let mut result = Vec::new();
        if flags & TCP_SYN != 0 { result.push(TcpFlag::Syn); }
        if flags & TCP_ACK != 0 { result.push(TcpFlag::Ack); }
        if flags & TCP_FIN != 0 { result.push(TcpFlag::Fin); }
        if flags & TCP_RST != 0 { result.push(TcpFlag::Rst); }
        if flags & TCP_PSH != 0 { result.push(TcpFlag::Psh); }
        result
    }
}

impl std::fmt::Display for TcpFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TcpFlag::Syn => write!(f, "SYN"),
            TcpFlag::SynAck => write!(f, "SYN|ACK"),
            TcpFlag::Ack => write!(f, "ACK"),
            TcpFlag::Fin => write!(f, "FIN"),
            TcpFlag::Rst => write!(f, "RST"),
            TcpFlag::Psh => write!(f, "PSH"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MuxHeader {
    pub protocol: u32,
    pub length: u32,         // total payload bytes INCLUDING this header
    pub is_v2: bool,         // magic present ⇒ extended framing
    pub tx_seq: u16,         // valid when is_v2
    pub rx_seq: u16,         // valid when is_v2
}

impl MuxHeader {
    /// Byte offset of the first payload byte after this header.
    pub fn header_len(&self) -> usize {
        if self.is_v2 { 16 } else { 8 }
    }
}

#[derive(Debug, Clone)]
pub struct TcpHeader {
    pub source_port: u16,
    pub dest_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub offset_flags: u16,
    pub window: u16,
}

impl TcpHeader {
    pub fn offset(&self) -> u8 {
        ((self.offset_flags >> 12) & 0x0F) as u8
    }

    pub fn flags(&self) -> u16 {
        self.offset_flags & 0x0FF
    }
}

#[derive(Debug, Clone)]
pub struct TcpPacket {
    pub header: TcpHeader,
    pub payload: Vec<u8>,
}

pub trait TcpPacketExt {
    fn to_bytes(&self) -> Vec<u8>;
}

impl TcpPacketExt for TcpPacket {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20 + self.payload.len());
        buf.extend_from_slice(&self.header.source_port.to_be_bytes());
        buf.extend_from_slice(&self.header.dest_port.to_be_bytes());
        buf.extend_from_slice(&self.header.seq.to_be_bytes());
        buf.extend_from_slice(&self.header.ack.to_be_bytes());
        buf.extend_from_slice(&self.header.offset_flags.to_be_bytes());
        buf.extend_from_slice(&self.header.window.to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&self.payload);
        buf
    }
}

pub trait ParseTcp {
    fn parse_tcp_header(data: &[u8]) -> Result<TcpHeader, MuxError>;
}

impl ParseTcp for TcpHeader {
    fn parse_tcp_header(data: &[u8]) -> Result<TcpHeader, MuxError> {
        if data.len() < 20 {
            return Err(MuxError::InvalidHeader("too short".into()));
        }

        Ok(TcpHeader {
            source_port: u16::from_be_bytes([data[0], data[1]]),
            dest_port: u16::from_be_bytes([data[2], data[3]]),
            seq: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            ack: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            offset_flags: u16::from_be_bytes([data[12], data[13]]),
            window: u16::from_be_bytes([data[14], data[15]]),
        })
    }
}

pub struct PacketReassembler {
    max_buffer: usize,
    buffers: Vec<u8>,
}

impl PacketReassembler {
    pub fn new(max_buffer: usize) -> Self {
        Self {
            max_buffer,
            buffers: Vec::new(),
        }
    }

    /// Feed raw bytes into the reassembly buffer and return **all** complete
    /// mux packets currently buffered. Leftover partial data is retained for
    /// the next call.
    ///
    /// If the buffered data exceeds `max_buffer` without yielding a complete
    /// packet, the buffer is discarded (the stream is treated as corrupt).
    /// Frames with an invalid length field are skipped one byte at a time to
    /// resynchronize on the next plausible header.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffers.extend_from_slice(data);

        let mut packets = Vec::new();
        loop {
            if self.buffers.len() < HEADER_SIZE {
                break;
            }
            let length = u32::from_be_bytes([
                self.buffers[4], self.buffers[5], self.buffers[6], self.buffers[7],
            ]) as usize;
            if length < HEADER_SIZE || length > self.max_buffer {
                // Impossible frame length (too small, or a frame that could
                // never fit in the buffer) — corrupt stream. Drop one byte
                // and try to resync on the next plausible header.
                tracing::warn!(
                    "reassembler: invalid frame length {length}, dropping 1 byte to resync"
                );
                self.buffers.remove(0);
                continue;
            }
            if self.buffers.len() < length {
                break;
            }
            packets.push(self.buffers[..length].to_vec());
            self.buffers.drain(..length);
        }

        if self.buffers.len() > self.max_buffer {
            tracing::warn!(
                "reassembler: buffered {} bytes exceeds limit {}, clearing",
                self.buffers.len(),
                self.max_buffer
            );
            self.buffers.clear();
        }

        packets
    }

    pub fn clear(&mut self) {
        self.buffers.clear();
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }
}

pub fn parse_mux_header(data: &[u8]) -> Result<MuxHeader, MuxError> {
    if data.len() < 8 {
        return Err(MuxError::InvalidHeader(format!("too short: {} bytes", data.len())));
    }
    let protocol = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let length = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

    let is_v2 = data.len() >= 12 && {
        let m = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        // Both magics observed on the wire: 0xFEEDFACE (sent by us / most
        // implementations) and 0xFACEFACE (sent back by recent Apple devices).
        m == 0xFEED_FACE || m == 0xFACE_FACE
    };
    let (tx_seq, rx_seq) = if is_v2 && data.len() >= 16 {
        (
            u16::from_be_bytes([data[12], data[13]]),
            u16::from_be_bytes([data[14], data[15]]),
        )
    } else {
        (0, 0)
    };
    Ok(MuxHeader { protocol, length, is_v2, tx_seq, rx_seq })
}

pub fn parse_packet(data: &[u8]) -> Result<MuxPacket, MuxError> {
    let header = parse_mux_header(data)?;
    let payload_start = header.header_len();
    let payload_end = header.length as usize;
    if payload_end > data.len() || payload_end < payload_start {
        return Err(MuxError::InvalidHeader(format!(
            "declared length {} inconsistent with buffer {} (header {} bytes)",
            payload_end, data.len(), payload_start
        )));
    }
    let payload = &data[payload_start..payload_end];

    match header.protocol {
        HEADER_VERSION => {
            let major = if payload.len() >= 4 {
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
            } else {
                0
            };
            let minor = if payload.len() >= 8 {
                u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]])
            } else {
                0
            };
            Ok(MuxPacket::Version { major, minor })
        }
        HEADER_CONTROL => Ok(MuxPacket::Control { payload: payload.to_vec() }),
        HEADER_SETUP => Ok(MuxPacket::Setup),
        HEADER_TCP => {
            if payload.len() >= 20 {
                let tcp_header = TcpHeader::parse_tcp_header(payload)?;
                let tcp_payload = payload[20..].to_vec();
                Ok(MuxPacket::Tcp { header: tcp_header, payload: tcp_payload })
            } else {
                Err(MuxError::InvalidHeader("TCP packet too short".into()))
            }
        }
        _ => Err(MuxError::UnsupportedProtocol(header.protocol)),
    }
}

pub fn parse_tcp_packet(data: &[u8]) -> Result<TcpPacket, MuxError> {
    let tcp_header = TcpHeader::parse_tcp_header(data)?;
    let tcp_payload = data[20..].to_vec();
    Ok(TcpPacket { header: tcp_header, payload: tcp_payload })
}

pub fn build_control_result(tag: u32, status: u8) -> Vec<u8> {
    vec![
        0, 0, 0, 0,
        0, 0, 0, 12,
        0, 0, 0, 1,
        status, 0, 0, 0,
        (tag >> 24) as u8, (tag >> 16) as u8, (tag >> 8) as u8, tag as u8,
        0, 0, 0, 0,
    ]
}

pub fn build_setup_connect(source_port: u16, dest_port: u16) -> Vec<u8> {
    vec![
        0, 0, 0, 2,
        0, 0, 0, 17,
        0xFE, 0xED, 0xFA, 0xCE,
        0, 0, 0, 0,
        0, 0, 0, 1,
        7,
        (source_port >> 8) as u8, source_port as u8,
        (dest_port >> 8) as u8, dest_port as u8,
    ]
}

/// The mux-header size depends on the device protocol version:
/// version < 2 → 8 bytes (protocol + length),
/// version >= 2 → 16 bytes (adds `magic` + `tx_seq` + `rx_seq`).
pub fn build_mux_frame(protocol: u32, v2_seqs: Option<(u16, u16)>, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match v2_seqs {
        Some((tx, rx)) => {
            let total = 16 + payload.len() as u32;
            out.extend_from_slice(&protocol.to_be_bytes());
            out.extend_from_slice(&total.to_be_bytes());
            out.extend_from_slice(&0xFEEDFACEu32.to_be_bytes());
            out.extend_from_slice(&tx.to_be_bytes());
            out.extend_from_slice(&rx.to_be_bytes());
        }
        None => {
            let total = 8 + payload.len() as u32;
            out.extend_from_slice(&protocol.to_be_bytes());
            out.extend_from_slice(&total.to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// Version negotiation — always v0/v1 header (8 bytes), 12-byte version body.
/// Matches usbmuxd device_add framing exactly: 20 bytes on the wire.
pub fn build_version_request() -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&2u32.to_be_bytes());   // major = 2
    payload.extend_from_slice(&0u32.to_be_bytes());   // minor = 0
    payload.extend_from_slice(&0u32.to_be_bytes());   // padding
    build_mux_frame(HEADER_VERSION, None, &payload)
}

/// SETUP packet — payload is the single byte 0x07 (from reference).
/// Used only after the device confirms version >= 2.
pub fn build_setup_packet(tx_seq: u16, rx_seq: u16) -> Vec<u8> {
    build_mux_frame(HEADER_SETUP, Some((tx_seq, rx_seq)), &[0x07])
}

pub fn build_mux_header(protocol: u32, length: u32) -> Vec<u8> {
    let mut header = vec![0u8; HEADER_SIZE];
    header[0..4].copy_from_slice(&protocol.to_be_bytes());
    header[4..8].copy_from_slice(&length.to_be_bytes());
    header
}

/// A TCP packet on the mux channel. `v2` when Some((tx_seq, rx_seq)) —
/// uses the 16-byte header with magic and seq counters; otherwise 8-byte.
pub fn build_tcp_packet(
    v2: Option<(u16, u16)>,
    sport: u16,
    dport: u16,
    tcp_seq: u32,
    tcp_ack: u32,
    flags: u16,
    window: u16,
    payload: Option<&[u8]>,
) -> Vec<u8> {
    let mut tcp = Vec::with_capacity(20 + payload.map(|p| p.len()).unwrap_or(0));
    tcp.extend_from_slice(&sport.to_be_bytes());
    tcp.extend_from_slice(&dport.to_be_bytes());
    tcp.extend_from_slice(&tcp_seq.to_be_bytes());
    tcp.extend_from_slice(&tcp_ack.to_be_bytes());
    // Buffer offset: Data offset (4 bits, in 32-bit words) << 12 | flags.
    // A header with no options = 5 words = 0x5 in the top nibble. The previous
    // version had a truncation bug: (0x50 << 12) overflows u16 to zero.
    let offset_flags: u16 = (0x5u16 << 12) | (0x0FFF & flags);
    tcp.extend_from_slice(&offset_flags.to_be_bytes());
    tcp.extend_from_slice(&window.to_be_bytes());
    tcp.extend_from_slice(&[0u8; 2]);
    tcp.extend_from_slice(&[0u8; 2]);
    if let Some(data) = payload {
        tcp.extend_from_slice(data);
    }
    build_mux_frame(HEADER_TCP, v2, &tcp)
}

/// Control packet — device reports errors/status here. Same dual framing.
pub fn build_control_packet(v2: Option<(u16, u16)>, payload: &[u8]) -> Vec<u8> {
    build_mux_frame(HEADER_CONTROL, v2, payload)
}

pub fn build_packet(protocol: u32, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE + data.len());
    buf.extend_from_slice(&protocol.to_be_bytes());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

pub fn increment_seq(seq: u16) -> u16 {
    seq.wrapping_add(1)
}

/// Compute the window value to advertise for a given amount of free buffer
/// space. The window is expressed in 256-byte units (matching the daemon's
/// long-standing `INITIAL_WINDOW >> 8` convention).
pub fn scaled_window(free_bytes: usize) -> u16 {
    ((free_bytes >> 8).min(u16::MAX as usize)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mux_header() {
        let data = build_version_request();
        let header = parse_mux_header(&data).unwrap();
        assert_eq!(header.protocol, 0);
        assert_eq!(header.length, 20);
    }

    #[test]
    fn test_parse_mux_header_too_short() {
        assert!(parse_mux_header(&[0, 0, 0, 2]).is_err());
    }

    #[test]
    fn test_parse_tcp_packet() {
        let packet = parse_tcp_packet(&[0, 80, 0, 50, 0, 0, 0, 1, 0, 0, 0, 2, 0x50, 0x02, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(packet.header.source_port, 80);
        assert_eq!(packet.header.dest_port, 50);
        assert_eq!(packet.header.flags(), 0x002);
    }

    #[test]
    fn test_build_control_result() {
        let result = build_control_result(1, 0);
        assert_eq!(result.len(), 24);
    }

    #[test]
    fn test_build_setup_connect() {
        let result = build_setup_connect(80, 50);
        assert_eq!(result.len(), 25);
    }

    #[test]
    fn test_build_version_request() {
        let data = build_version_request();
        assert_eq!(data.len(), 20, "version request: 8 hdr + 12 version_body");
        let header = parse_mux_header(&data).unwrap();
        assert!(!header.is_v2);
        assert_eq!(header.protocol, HEADER_VERSION);
        assert_eq!(header.length, 20);
        // major=2, minor=0 (BE u32s at offsets 8..16).
        assert_eq!(&data[8..12], &2u32.to_be_bytes());
        assert_eq!(&data[12..16], &0u32.to_be_bytes());
    }

    #[test]
    fn test_build_setup_packet_v2() {
        let data = build_setup_packet(0, 0xFFFF);
        let header = parse_mux_header(&data).unwrap();
        assert!(header.is_v2, "setup is v2-framed");
        assert_eq!(header.protocol, HEADER_SETUP);
        assert_eq!(header.rx_seq, 0xFFFF);  // reference resets to 0xFFFF during SETUP
        assert_eq!(header.tx_seq, 0);
        assert_eq!(*data.last().unwrap(), 0x07); // content byte
    }

    #[test]
    fn test_build_tcp_packet_v2_layout() {
        let pkt = build_tcp_packet(Some((42, 21)), 1100, 62078, 1, 77, TCP_SYN, (INITIAL_WINDOW >> 8) as u16, None);
        // 16 mux-v2 header + 20 tcp header
        assert_eq!(pkt.len(), 36);
        assert_eq!(u32::from_be_bytes([pkt[0], pkt[1], pkt[2], pkt[3]]), HEADER_TCP as u32);
        assert_eq!(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]), 36);
        assert_eq!(&pkt[8..12], &[0xFE, 0xED, 0xFA, 0xCE]);
        assert_eq!(u16::from_be_bytes([pkt[12], pkt[13]]), 42);
        assert_eq!(u16::from_be_bytes([pkt[14], pkt[15]]), 21);
    }

    #[test]
    fn test_build_control_result_with_tag() {
        let result = build_control_result(42, 0);
        assert_eq!(result[16], 0);
        assert_eq!(result[17], 0);
        assert_eq!(result[18], 0);
        assert_eq!(result[19], 42);
    }

    #[test]
    fn test_increment_seq_wraps() {
        let seq = u16::MAX;
        let next = increment_seq(seq);
        assert_eq!(next, 0);
    }

    #[test]
    fn test_parse_mux_header_length_exceeds_data() {
        let data = [0, 0, 0, 2, 0, 0, 0, 200];
        let header = parse_mux_header(&data).unwrap();
        assert_eq!(header.length, 200);
        assert_eq!(header.protocol, 2);
    }

    /// Build a well-formed synthetic frame: [protocol: BE u32][length: BE u32][body].
    /// `length` is the total frame size, matching parse_packet's interpretation.
    fn synthetic_frame(protocol: u32, body: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(8 + body.len());
        f.extend_from_slice(&protocol.to_be_bytes());
        f.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    #[test]
    fn test_reassembler_single_packet() {
        let mut r = PacketReassembler::new(1024);
        let pkt = synthetic_frame(HEADER_TCP, &[1, 2, 3, 4]);
        let out = r.feed(&pkt);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], pkt);
        assert!(r.is_empty());
    }

    #[test]
    fn test_reassembler_drains_multiple_packets() {
        let mut r = PacketReassembler::new(1024);
        let p1 = synthetic_frame(HEADER_TCP, &[1, 2, 3]);
        let p2 = synthetic_frame(HEADER_CONTROL, &[9]);
        let p3 = synthetic_frame(HEADER_TCP, &[5, 6, 7, 8, 9]);
        let mut blob = Vec::new();
        blob.extend_from_slice(&p1);
        blob.extend_from_slice(&p2);
        blob.extend_from_slice(&p3);
        let out = r.feed(&blob);
        assert_eq!(out.len(), 3, "all complete packets must be drained in one feed");
        assert_eq!(out[0], p1);
        assert_eq!(out[1], p2);
        assert_eq!(out[2], p3);
    }

    #[test]
    fn test_reassembler_partial_then_complete() {
        let mut r = PacketReassembler::new(1024);
        let pkt = synthetic_frame(HEADER_TCP, &[1, 2, 3, 4]);
        let (first, second) = pkt.split_at(5);
        assert!(r.feed(first).is_empty());
        assert_eq!(r.len(), 5);
        let out = r.feed(second);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], pkt);
    }

    #[test]
    fn test_reassembler_overflow_clears() {
        let mut r = PacketReassembler::new(64);
        // Header claims a frame larger than the cap — impossible to ever
        // complete, so the buffer must be drained via resync (never growing
        // unboundedly and never emitting the bogus frame).
        let mut data = vec![0u8; 100];
        data[0..4].copy_from_slice(&2u32.to_be_bytes());
        data[4..8].copy_from_slice(&1_000_000u32.to_be_bytes());
        let out = r.feed(&data);
        assert!(out.is_empty());
        assert!(r.len() < HEADER_SIZE, "over-limit stream must be fully resynced away, got {}", r.len());
    }

    #[test]
    fn test_reassembler_buffer_never_exceeds_cap() {
        // Invariant: regardless of input shape, the retained buffer never
        // exceeds the configured cap after a feed call.
        let mut r = PacketReassembler::new(64);
        let mut rng_state = 0x12345678u32;
        let mut next = move || {
            rng_state = rng_state.wrapping_mul(1664525).wrapping_add(1013904223);
            (rng_state >> 16) as u8
        };
        for _ in 0..200 {
            let chunk: Vec<u8> = (0..37).map(|_| next()).collect();
            r.feed(&chunk);
            assert!(r.len() <= 64, "buffer {} exceeds cap 64", r.len());
        }
    }

    #[test]
    fn test_reassembler_resync_on_garbage_length() {
        let mut r = PacketReassembler::new(1024);
        let data = vec![0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44]; // garbage, length < 8
        let pkt = synthetic_frame(HEADER_TCP, &[7, 7, 7]);
        let out1 = r.feed(&data);
        assert!(out1.is_empty());
        // Follow-up valid frame must be recoverable after byte-wise resync.
        let mut stream = Vec::new();
        stream.extend_from_slice(&pkt);
        let out2 = r.feed(&stream);
        assert!(
            out2.iter().any(|p| p == &pkt),
            "reassembler must resync and recover the valid frame"
        );
    }

    #[test]
    fn test_scaled_window() {
        assert_eq!(scaled_window(0), 0);
        assert_eq!(scaled_window(256), 1);
        assert_eq!(scaled_window(1024 * 1024), 4096);
        assert_eq!(scaled_window(usize::MAX), u16::MAX);
    }
}
