use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

pub const USB_MTU: usize = 49152;
pub const MUX_HEADER_V1_SIZE: usize = 8;
pub const MUX_HEADER_V2_SIZE: usize = 16;
pub const TCP_HEADER_SIZE: usize = 20;
pub const VERSION_HEADER_SIZE: usize = 12;
pub const MAX_PAYLOAD: usize = USB_MTU - MUX_HEADER_V2_SIZE - TCP_HEADER_SIZE;
pub const INITIAL_WINDOW: u32 = 131072;
pub const ACK_TIMEOUT_MS: u64 = 30000;
pub const MAGIC: u32 = 0xFEEDFACE;

pub const PROTO_VERSION: u32 = 0;
pub const PROTO_CONTROL: u32 = 1;
pub const PROTO_SETUP: u32 = 2;
pub const PROTO_TCP: u32 = 6;

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_ACK: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxHeader {
    pub protocol: u32,
    pub length: u32,
    pub magic: u32,
    pub tx_seq: u16,
    pub rx_seq: u16,
}

impl MuxHeader {
    pub fn v1(protocol: u32, length: u32) -> Self {
        MuxHeader {
            protocol,
            length,
            magic: 0,
            tx_seq: 0,
            rx_seq: 0,
        }
    }

    pub fn v2(protocol: u32, length: u32, tx_seq: u16, rx_seq: u16) -> Self {
        MuxHeader {
            protocol,
            length,
            magic: MAGIC,
            tx_seq,
            rx_seq,
        }
    }

    pub fn header_size(version: u8) -> usize {
        match version {
            2 => MUX_HEADER_V2_SIZE,
            _ => MUX_HEADER_V1_SIZE,
        }
    }

    pub fn to_bytes(&self, version: u8) -> Vec<u8> {
        let size = Self::header_size(version);
        let mut buf = vec![0u8; size];
        buf[0..4].copy_from_slice(&self.protocol.to_be_bytes());
        buf[4..8].copy_from_slice(&self.length.to_be_bytes());
        if version == 2 {
            buf[8..12].copy_from_slice(&self.magic.to_be_bytes());
            buf[12..14].copy_from_slice(&self.tx_seq.to_be_bytes());
            buf[14..16].copy_from_slice(&self.rx_seq.to_be_bytes());
        }
        buf
    }

    pub fn from_bytes(data: &[u8], version: u8) -> Option<Self> {
        let size = Self::header_size(version);
        if data.len() < size {
            return None;
        }
        let protocol = u32::from_be_bytes(data[0..4].try_into().ok()?);
        let length = u32::from_be_bytes(data[4..8].try_into().ok()?);
        let (magic, tx_seq, rx_seq) = if version == 2 {
            let magic = u32::from_be_bytes(data[8..12].try_into().ok()?);
            let tx_seq = u16::from_be_bytes(data[12..14].try_into().ok()?);
            let rx_seq = u16::from_be_bytes(data[14..16].try_into().ok()?);
            (magic, tx_seq, rx_seq)
        } else {
            (0, 0, 0)
        };
        Some(MuxHeader {
            protocol,
            length,
            magic,
            tx_seq,
            rx_seq,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct TcpHeader {
    pub th_sport: u16,
    pub th_dport: u16,
    pub th_seq: u32,
    pub th_ack: u32,
    pub th_off: u8,
    pub th_flags: u8,
    pub th_win: u16,
    pub th_sum: u16,
    pub th_urp: u16,
}

impl TcpHeader {
    pub fn new(sport: u16, dport: u16, seq: u32, ack: u32, flags: u8, win: u16) -> Self {
        TcpHeader {
            th_sport: sport,
            th_dport: dport,
            th_seq: seq,
            th_ack: ack,
            th_off: 0x50,
            th_flags: flags,
            th_win: win,
            th_sum: 0,
            th_urp: 0,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; TCP_HEADER_SIZE];
        buf[0..2].copy_from_slice(&self.th_sport.to_be_bytes());
        buf[2..4].copy_from_slice(&self.th_dport.to_be_bytes());
        buf[4..8].copy_from_slice(&self.th_seq.to_be_bytes());
        buf[8..12].copy_from_slice(&self.th_ack.to_be_bytes());
        buf[12] = self.th_off;
        buf[13] = self.th_flags;
        buf[14..16].copy_from_slice(&self.th_win.to_be_bytes());
        buf[16..18].copy_from_slice(&self.th_sum.to_be_bytes());
        buf[18..20].copy_from_slice(&self.th_urp.to_be_bytes());
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < TCP_HEADER_SIZE {
            return None;
        }
        Some(TcpHeader {
            th_sport: u16::from_be_bytes(data[0..2].try_into().ok()?),
            th_dport: u16::from_be_bytes(data[2..4].try_into().ok()?),
            th_seq: u32::from_be_bytes(data[4..8].try_into().ok()?),
            th_ack: u32::from_be_bytes(data[8..12].try_into().ok()?),
            th_off: data[12],
            th_flags: data[13],
            th_win: u16::from_be_bytes(data[14..16].try_into().ok()?),
            th_sum: u16::from_be_bytes(data[16..18].try_into().ok()?),
            th_urp: u16::from_be_bytes(data[18..20].try_into().ok()?),
        })
    }

    pub fn source_port(&self) -> u16 {
        self.th_sport
    }

    pub fn dest_port(&self) -> u16 {
        self.th_dport
    }

    pub fn seq(&self) -> u32 {
        self.th_seq
    }

    pub fn ack(&self) -> u32 {
        self.th_ack
    }

    pub fn window(&self) -> u32 {
        (self.th_win as u32) << 8
    }

    pub fn has_flag(&self, flag: u8) -> bool {
        self.th_flags & flag != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionHeader {
    pub major: u32,
    pub minor: u32,
    pub padding: u32,
}

impl VersionHeader {
    pub fn new(major: u32) -> Self {
        VersionHeader {
            major,
            minor: 0,
            padding: 0,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; VERSION_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.major.to_be_bytes());
        buf[4..8].copy_from_slice(&self.minor.to_be_bytes());
        buf[8..12].copy_from_slice(&self.padding.to_be_bytes());
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < VERSION_HEADER_SIZE {
            return None;
        }
        Some(VersionHeader {
            major: u32::from_be_bytes(data[0..4].try_into().ok()?),
            minor: u32::from_be_bytes(data[4..8].try_into().ok()?),
            padding: u32::from_be_bytes(data[8..12].try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxPacket {
    Version { major: u32, minor: u32 },
    Control { payload: Vec<u8> },
    Setup,
    Tcp { header: TcpHeader, payload: Vec<u8> },
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("packet too short: need {need} bytes, got {got}")]
    PacketTooShort { need: usize, got: usize },

    #[error("invalid protocol: {0}")]
    InvalidProtocol(u32),

    #[error("invalid mux header")]
    InvalidHeader,

    #[error("invalid TCP header")]
    InvalidTcpHeader,

    #[error("connection not found for sport={sport}")]
    ConnectionNotFound { sport: u16 },

    #[error("connection refused on port {dport}")]
    ConnectionRefused { dport: u16 },

    #[error("no available source ports")]
    NoAvailablePorts,

    #[error("device not found: {device_id}")]
    DeviceNotFound { device_id: u32 },

    #[error("invalid version header")]
    InvalidVersionHeader,
}

pub type Result<T> = std::result::Result<T, MuxError>;

pub fn parse_packet(data: &[u8]) -> Result<MuxPacket> {
    if data.len() < MUX_HEADER_V1_SIZE {
        return Err(MuxError::PacketTooShort {
            need: MUX_HEADER_V1_SIZE,
            got: data.len(),
        });
    }

    let protocol = u32::from_be_bytes(data[0..4].try_into().unwrap());
    let length = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;

    let (header_size, tx_seq, rx_seq) = if data.len() >= MUX_HEADER_V2_SIZE {
        let magic = u32::from_be_bytes(data[8..12].try_into().unwrap());
        if magic == MAGIC || magic == 0xFACEFACE {
            let tx = u16::from_be_bytes(data[12..14].try_into().unwrap());
            let rx = u16::from_be_bytes(data[14..16].try_into().unwrap());
            (MUX_HEADER_V2_SIZE, tx, rx)
        } else {
            (MUX_HEADER_V1_SIZE, 0u16, 0u16)
        }
    } else {
        (MUX_HEADER_V1_SIZE, 0u16, 0u16)
    };

    if data.len() < length {
        return Err(MuxError::PacketTooShort {
            need: length,
            got: data.len(),
        });
    }

    match protocol {
        PROTO_VERSION => {
            let vhdr_start = header_size;
            if data.len() < vhdr_start + VERSION_HEADER_SIZE {
                return Err(MuxError::PacketTooShort {
                    need: vhdr_start + VERSION_HEADER_SIZE,
                    got: data.len(),
                });
            }
            let vhdr = VersionHeader::from_bytes(&data[vhdr_start..])
                .ok_or(MuxError::InvalidVersionHeader)?;
            Ok(MuxPacket::Version {
                major: vhdr.major,
                minor: vhdr.minor,
            })
        }
        PROTO_CONTROL => {
            let payload_start = header_size;
            let payload = if length > payload_start {
                data[payload_start..length].to_vec()
            } else {
                Vec::new()
            };
            Ok(MuxPacket::Control { payload })
        }
        PROTO_SETUP => Ok(MuxPacket::Setup),
        PROTO_TCP => {
            let header_start = header_size;
            if data.len() < header_start + TCP_HEADER_SIZE {
                return Err(MuxError::PacketTooShort {
                    need: header_start + TCP_HEADER_SIZE,
                    got: data.len(),
                });
            }
            let tcp_hdr =
                TcpHeader::from_bytes(&data[header_start..]).ok_or(MuxError::InvalidTcpHeader)?;
            let payload_start = header_start + TCP_HEADER_SIZE;
            let payload = if length > payload_start {
                data[payload_start..length].to_vec()
            } else {
                Vec::new()
            };
            Ok(MuxPacket::Tcp {
                header: tcp_hdr,
                payload,
            })
        }
        _ => Err(MuxError::InvalidProtocol(protocol)),
    }
}

pub fn build_version_request(tx_seq: u16, rx_seq: u16) -> Vec<u8> {
    let length = (MUX_HEADER_V2_SIZE + VERSION_HEADER_SIZE) as u32;
    let hdr = MuxHeader::v2(PROTO_VERSION, length, tx_seq, rx_seq);
    let mut buf = hdr.to_bytes(2);
    let vhdr = VersionHeader::new(2);
    buf.extend_from_slice(&vhdr.to_bytes());
    buf
}

pub fn build_setup_packet(tx_seq: u16, rx_seq: u16) -> Vec<u8> {
    let length = (MUX_HEADER_V2_SIZE + 1) as u32;
    let hdr = MuxHeader::v2(PROTO_SETUP, length, tx_seq, rx_seq);
    let mut buf = hdr.to_bytes(2);
    buf.push(0x07);
    buf
}

pub fn build_tcp_packet(
    proto_version: u8,
    tx_seq: u16,
    rx_seq: u16,
    sport: u16,
    dport: u16,
    tcp_seq: u32,
    tcp_ack: u32,
    flags: u8,
    window: u16,
    payload: Option<&[u8]>,
) -> Vec<u8> {
    let payload_len = payload.map_or(0, |p| p.len());
    let total_length = MuxHeader::header_size(proto_version as u8) + TCP_HEADER_SIZE + payload_len;

    let mux_hdr = if proto_version == 2 {
        MuxHeader::v2(PROTO_TCP, total_length as u32, tx_seq, rx_seq)
    } else {
        MuxHeader::v1(PROTO_TCP, total_length as u32)
    };

    let mut buf = mux_hdr.to_bytes(proto_version);
    let tcp_hdr = TcpHeader::new(sport, dport, tcp_seq, tcp_ack, flags, window);
    buf.extend_from_slice(&tcp_hdr.to_bytes());
    if let Some(data) = payload {
        buf.extend_from_slice(data);
    }
    buf
}

pub fn build_rst_packet(proto_version: u8, tx_seq: u16, rx_seq: u16, sport: u16, dport: u16) -> Vec<u8> {
    build_tcp_packet(
        proto_version,
        tx_seq,
        rx_seq,
        sport,
        dport,
        0,
        0,
        TCP_RST,
        0,
        None,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Refused,
    Dying,
    Dead,
}

pub struct MuxConnection {
    pub state: ConnState,
    pub sport: u16,
    pub dport: u16,
    pub tx_seq: u32,
    pub tx_ack: u32,
    pub tx_acked: u32,
    pub tx_win: u32,
    pub rx_seq: u32,
    pub rx_recvd: u32,
    pub rx_ack: u32,
    pub rx_win: u32,
    pub ib_buf: Vec<u8>,
    pub ob_buf: Vec<u8>,
    pub max_payload: usize,
    pub last_ack_time: Instant,
    pub tag: u32,
    pub data_notify: Arc<tokio::sync::Notify>,
}

impl MuxConnection {
    pub fn new(sport: u16, dport: u16, tag: u32) -> Self {
        MuxConnection {
            state: ConnState::Connecting,
            sport,
            dport,
            tx_seq: 0,
            tx_ack: 0,
            tx_acked: 0,
            tx_win: INITIAL_WINDOW,
            rx_seq: 0,
            rx_recvd: 0,
            rx_ack: 0,
            rx_win: INITIAL_WINDOW,
            ib_buf: Vec::new(),
            ob_buf: Vec::new(),
            max_payload: MAX_PAYLOAD,
            last_ack_time: Instant::now(),
            tag,
            data_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn is_sendable(&self) -> bool {
        self.state == ConnState::Connected && !self.ob_buf.is_empty()
    }

    pub fn get_sendable(&self) -> usize {
        if self.state != ConnState::Connected {
            return 0;
        }
        let in_flight = self.tx_seq.saturating_sub(self.tx_acked) as usize;
        let available = self.tx_win.saturating_sub(in_flight as u32) as usize;
        let buffered = self.ob_buf.len();
        std::cmp::min(available, std::cmp::min(buffered, self.max_payload))
    }

    pub fn update_ack(&mut self) {
        self.rx_ack = self.rx_recvd;
    }

    pub fn needs_ack(&self) -> bool {
        self.rx_ack != self.rx_recvd
            || self.last_ack_time.elapsed().as_millis() >= ACK_TIMEOUT_MS as u128
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Init,
    Active,
    Dead,
}

pub struct MuxDevice {
    pub device_id: u32,
    pub version: u8,
    pub state: DeviceState,
    pub tx_seq: u16,
    pub rx_seq: u16,
    pub connections: HashMap<u16, MuxConnection>,
    next_sport: u16,
    pub pktbuf: Vec<u8>,
    pub pktlen: usize,
}

impl MuxDevice {
    pub fn new(device_id: u32) -> Self {
        MuxDevice {
            device_id,
            version: 0,
            state: DeviceState::Init,
            tx_seq: 0,
            rx_seq: 0,
            connections: HashMap::new(),
            next_sport: 1,
            pktbuf: Vec::with_capacity(USB_MTU),
            pktlen: 0,
        }
    }

    pub fn alloc_sport(&mut self) -> Option<u16> {
        let start = self.next_sport;
        loop {
            if !self.connections.contains_key(&self.next_sport) {
                let sport = self.next_sport;
                self.next_sport = self.next_sport.wrapping_add(1);
                if self.next_sport == 0 {
                    self.next_sport = 1;
                }
                return Some(sport);
            }
            self.next_sport = self.next_sport.wrapping_add(1);
            if self.next_sport == 0 {
                self.next_sport = 1;
            }
            if self.next_sport == start {
                return None;
            }
        }
    }

    pub fn get_connection(&self, sport: u16) -> Option<&MuxConnection> {
        self.connections.get(&sport)
    }

    pub fn get_connection_mut(&mut self, sport: u16) -> Option<&mut MuxConnection> {
        self.connections.get_mut(&sport)
    }

    pub fn find_connection(&self, sport: u16, dport: u16) -> Option<&MuxConnection> {
        self.connections
            .values()
            .find(|c| c.sport == sport && c.dport == dport)
    }

    pub fn remove_connection(&mut self, sport: u16) -> Option<MuxConnection> {
        self.connections.remove(&sport)
    }
}

#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    Connected {
        device_id: u32,
        sport: u16,
        tag: u32,
    },
    Refused {
        device_id: u32,
        sport: u16,
        tag: u32,
    },
    DataReady {
        device_id: u32,
        sport: u16,
    },
    Disconnected {
        device_id: u32,
        sport: u16,
    },
    Error {
        device_id: u32,
        sport: u16,
        error: String,
    },
}

pub struct ConnectionManager {
    pub devices: Arc<RwLock<HashMap<u32, MuxDevice>>>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        ConnectionManager {
            devices: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn add_device(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        devices.entry(device_id).or_insert_with(|| MuxDevice::new(device_id));
    }

    pub async fn remove_device(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        devices.remove(&device_id);
    }

    pub async fn get_device(&self, device_id: u32) -> Option<tokio::sync::RwLockReadGuard<'_, HashMap<u32, MuxDevice>>> {
        let devices = self.devices.read().await;
        if devices.contains_key(&device_id) {
            Some(devices)
        } else {
            None
        }
    }

    pub async fn get_data_notify(&self, device_id: u32, sport: u16) -> Option<Arc<tokio::sync::Notify>> {
        let devices = self.devices.read().await;
        let device = devices.get(&device_id)?;
        let conn = device.connections.get(&sport)?;
        Some(conn.data_notify.clone())
    }

    pub async fn get_connection_version(&self, device_id: u32) -> Option<u8> {
        let devices = self.devices.read().await;
        let device = devices.get(&device_id)?;
        Some(device.version)
    }

    pub async fn get_device_mut(&self, device_id: u32) -> Option<tokio::sync::RwLockWriteGuard<'_, HashMap<u32, MuxDevice>>> {
        let devices = self.devices.write().await;
        if devices.contains_key(&device_id) {
            Some(devices)
        } else {
            None
        }
    }

    pub async fn start_connect(
        &self,
        device_id: u32,
        dport: u16,
        tag: u32,
    ) -> Result<u16> {
        let mut devices = self.devices.write().await;
        let device = devices
            .get_mut(&device_id)
            .ok_or(MuxError::DeviceNotFound { device_id })?;

        let sport = device.alloc_sport().ok_or(MuxError::NoAvailablePorts)?;

        let conn = MuxConnection::new(sport, dport, tag);
        device.connections.insert(sport, conn);

        info!(
            "start_connect: device_id={device_id} sport={sport} dport={dport} tag={tag}"
        );

        Ok(sport)
    }

    pub async fn handle_usb_packet(
        &self,
        device_id: u32,
        data: &[u8],
    ) -> (Vec<ConnectionEvent>, Vec<Vec<u8>>) {
        let packet = match parse_packet(data) {
            Ok(p) => p,
            Err(e) => {
                error!("failed to parse mux packet: {e}");
                return (vec![], vec![]);
            }
        };

        match packet {
            MuxPacket::Version { major, minor } => {
                debug!("received version response: {major}.{minor}");
                (vec![], vec![])
            }
            MuxPacket::Control { payload } => {
                debug!("received control packet: {} bytes", payload.len());
                (vec![], vec![])
            }
            MuxPacket::Setup => {
                debug!("received setup packet");
                (vec![], vec![])
            }
            MuxPacket::Tcp { header, payload } => {
                self.handle_tcp_packet(device_id, &header, &payload).await
            }
        }
    }

    pub async fn handle_tcp_packet(
        &self,
        device_id: u32,
        tcp_hdr: &TcpHeader,
        payload: &[u8],
    ) -> (Vec<ConnectionEvent>, Vec<Vec<u8>>) {
        let mut send_queue = Vec::new();
        let dport = tcp_hdr.dest_port();
        let mut events = Vec::new();

        let mut devices = self.devices.write().await;
        let device = match devices.get_mut(&device_id) {
            Some(d) => d,
            None => {
                warn!("tcp packet for unknown device_id={device_id}");
                return (vec![], vec![]);
            }
        };

        let mut matched_sport: Option<u16> = None;
        let tcp_sport = tcp_hdr.source_port();
        let tcp_dport = tcp_hdr.dest_port();
        for (&s, conn) in &device.connections {
            if (conn.dport == tcp_sport) && conn.state != ConnState::Dead {
                matched_sport = Some(s);
                break;
            }
            if (conn.dport == tcp_dport) && conn.state != ConnState::Dead {
                matched_sport = Some(s);
                break;
            }
        }

        let sport = match matched_sport {
            Some(s) => s,
            None => {
                warn!("tcp packet for unknown dport={dport} on device_id={device_id}");
                return (vec![], vec![]);
            }
        };

        let mut notify_handles: Vec<Arc<tokio::sync::Notify>> = Vec::new();

        let conn = match device.connections.get_mut(&sport) {
            Some(c) => c,
            None => {
                warn!("tcp packet for sport={sport} but connection not found");
                return (vec![], vec![]);
            }
        };

        if tcp_hdr.has_flag(TCP_RST) {
            debug!("received RST on sport={sport}");
            let reason = if !payload.is_empty() {
                String::from_utf8_lossy(payload).to_string()
            } else {
                String::new()
            };
            if !reason.is_empty() {
                info!("RST reason: {reason}");
            }
            conn.state = ConnState::Dead;
            events.push(ConnectionEvent::Disconnected {
                device_id,
                sport,
            });
            return (events, send_queue);
        }

        if conn.state == ConnState::Connecting {
            if tcp_hdr.has_flag(TCP_SYN) && tcp_hdr.has_flag(TCP_ACK) {
                debug!("received SYN|ACK on sport={sport}, sending ACK");
                let peer_seq = tcp_hdr.seq();
                let our_ack = peer_seq.wrapping_add(1);
                let our_seq = conn.tx_seq;

                conn.tx_seq = our_seq.wrapping_add(1);
                conn.tx_ack = our_ack;
                conn.rx_seq = peer_seq;
                conn.rx_recvd = our_ack;
                conn.state = ConnState::Connected;
                conn.last_ack_time = Instant::now();

                let ack_pkt = build_tcp_packet(
                    device.version,
                    device.tx_seq,
                    device.rx_seq,
                    sport,
                    dport,
                    our_seq,
                    our_ack,
                    TCP_ACK,
                    (INITIAL_WINDOW >> 8) as u16,
                    None,
                );
                send_queue.push(ack_pkt);
                device.tx_seq = device.tx_seq.wrapping_add(1);

                events.push(ConnectionEvent::Connected {
                    device_id,
                    sport,
                    tag: conn.tag,
                });
                return (events, send_queue);
            }

            if tcp_hdr.has_flag(TCP_ACK) && !tcp_hdr.has_flag(TCP_SYN) {
                debug!("received ACK during CONNECTING on sport={sport}, connection may be established");
                conn.state = ConnState::Connected;
                conn.last_ack_time = Instant::now();
                events.push(ConnectionEvent::Connected {
                    device_id,
                    sport,
                    tag: conn.tag,
                });
                return (events, send_queue);
            }
        }

        if conn.state == ConnState::Connected && !payload.is_empty() {
            let peer_seq = tcp_hdr.seq();
            if peer_seq == conn.rx_seq || conn.rx_recvd == 0 {
                let data_len = payload.len() as u32;
                conn.ib_buf.extend_from_slice(payload);
                conn.rx_seq = peer_seq.wrapping_add(data_len);
                conn.rx_recvd = conn.rx_recvd.wrapping_add(data_len);
                conn.update_ack();
                conn.last_ack_time = Instant::now();
                notify_handles.push(conn.data_notify.clone());

                events.push(ConnectionEvent::DataReady {
                    device_id,
                    sport,
                });
            } else {
                debug!(
                    "out-of-order data on sport={sport}: expected seq={}, got seq={}",
                    conn.rx_seq, peer_seq
                );
            }
        }

        if conn.state == ConnState::Connected && tcp_hdr.has_flag(TCP_ACK) {
            let acked = tcp_hdr.ack();
            if acked != conn.tx_acked {
                let newly_acked = acked.wrapping_sub(conn.tx_acked);
                conn.tx_acked = acked;
                if newly_acked > 0 && !conn.ob_buf.is_empty() {
                    let remove = std::cmp::min(newly_acked as usize, conn.ob_buf.len());
                    conn.ob_buf.drain(..remove);
                }
                debug!(
                    "ack on sport={sport}: acked={acked} in_flight={}",
                    conn.tx_seq.wrapping_sub(conn.tx_acked)
                );
            }
        }

        for notify in notify_handles {
            notify.notify_one();
        }

        (events, send_queue)
    }

    pub async fn process_connection(
        &self,
        device_id: u32,
        sport: u16,
    ) -> Option<ConnectionEvent> {
        let mut devices = self.devices.write().await;
        let device = devices.get_mut(&device_id)?;

        let conn = device.connections.get_mut(&sport)?;

        if conn.state == ConnState::Dead {
            return Some(ConnectionEvent::Disconnected { device_id, sport });
        }

        if conn.state == ConnState::Refused {
            let tag = conn.tag;
            device.connections.remove(&sport);
            return Some(ConnectionEvent::Refused {
                device_id,
                sport,
                tag,
            });
        }

        if conn.state == ConnState::Connecting && conn.needs_ack() {
            let pkt = build_tcp_packet(
                device.version,
                device.tx_seq,
                device.rx_seq,
                sport,
                conn.dport,
                0,
                0,
                TCP_SYN,
                (INITIAL_WINDOW >> 8) as u16,
                None,
            );
            device.tx_seq = device.tx_seq.wrapping_add(1);
            debug!("sending SYN for sport={sport}");
            let _ = pkt;
            return None;
        }

        if conn.state == ConnState::Connected && conn.is_sendable() {
            let sendable = conn.get_sendable();
            if sendable > 0 {
                let chunk = &conn.ob_buf[..sendable];
                let tcp_seq = conn.tx_seq;
                let tcp_ack = conn.tx_ack;
                let win = (INITIAL_WINDOW >> 8) as u16;

                let _pkt = build_tcp_packet(
                    device.version,
                    device.tx_seq,
                    device.rx_seq,
                    sport,
                    conn.dport,
                    tcp_seq,
                    tcp_ack,
                    TCP_ACK,
                    win,
                    Some(chunk),
                );

                conn.tx_seq = conn.tx_seq.wrapping_add(sendable as u32);
                device.tx_seq = device.tx_seq.wrapping_add(1);

                debug!(
                    "sending {} bytes on sport={sport}: seq={tcp_seq}",
                    sendable
                );
                return None;
            }
        }

        if conn.state == ConnState::Connected && conn.needs_ack() {
            let pkt = build_tcp_packet(
                device.version,
                device.tx_seq,
                device.rx_seq,
                sport,
                conn.dport,
                conn.tx_seq,
                conn.tx_ack,
                TCP_ACK,
                (conn.rx_win >> 8) as u16,
                None,
            );
            device.tx_seq = device.tx_seq.wrapping_add(1);
            conn.update_ack();
            conn.last_ack_time = Instant::now();
            debug!("sending ACK for sport={sport}");
            let _ = pkt;
            return None;
        }

        None
    }
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}
