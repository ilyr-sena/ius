use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, info, warn};

use super::mux::{
    ConnState, ConnectionManager, ConnectionEvent, DeviceState,
    build_version_request, build_setup_packet, build_tcp_packet,
    parse_packet, MuxPacket, TCP_SYN, TCP_ACK, INITIAL_WINDOW,
};
use super::usb::{AppleMuxInterface, UsbReader, PacketReassembler};
use crate::daemon::device_scanner::UsbDevice;

pub struct ManagedDevice {
    pub device_id: u32,
    pub usb: Arc<AppleMuxInterface>,
    pub data_tx: mpsc::Sender<(u16, Vec<u8>)>,
    pub connect_tx: mpsc::Sender<ConnectRequest>,
}

pub struct ConnectRequest {
    pub device_id: u32,
    pub dport: u16,
    pub tag: u32,
    pub resp_tx: oneshot::Sender<Result<u16, String>>,
}

pub struct DeviceManager {
    pub devices: Arc<RwLock<HashMap<u32, ManagedDevice>>>,
    pub conn_mgr: ConnectionManager,
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            conn_mgr: ConnectionManager::new(),
        }
    }

    pub async fn add_device(&self, dev: &UsbDevice) {
        let mut devices = self.devices.write().await;
        if devices.contains_key(&dev.device_id) {
            debug!("device {} already managed", dev.device_id);
            return;
        }

        let rusb_dev = {
            let all_devices = match rusb::devices() {
                Ok(d) => d,
                Err(e) => {
                    warn!("failed to list USB devices for {}: {e}", dev.udid);
                    return;
                }
            };
            let mut found = None;
            for device in all_devices.iter() {
                if let Ok(desc) = device.device_descriptor() {
                    if desc.vendor_id() == 0x05AC {
                        if let Ok(handle) = device.open() {
                            if let Ok(serial) = handle.read_serial_number_string_ascii(&desc) {
                                if serial.trim().trim_end_matches('\0') == dev.udid {
                                    found = Some(device.clone());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            match found {
                Some(d) => d,
                None => {
                    warn!("failed to find USB device for {}: not found", dev.udid);
                    return;
                }
            }
        };

        let usb = match AppleMuxInterface::open(&rusb_dev) {
            Ok(u) => Arc::new(u),
            Err(e) => {
                warn!("failed to open mux interface for {}: {e}", dev.udid);
                return;
            }
        };

        let (usb_tx, usb_rx) = mpsc::channel::<Vec<u8>>(64);
        let (data_tx, data_rx) = mpsc::channel::<(u16, Vec<u8>)>(256);
        let (connect_tx, connect_rx) = mpsc::channel::<ConnectRequest>(16);

        let reader = UsbReader::new(usb.clone(), usb_tx.clone());
        reader.spawn();

        let managed = ManagedDevice {
            device_id: dev.device_id,
            usb: usb.clone(),
            data_tx,
            connect_tx,
        };

        devices.insert(dev.device_id, managed);

        self.conn_mgr.add_device(dev.device_id).await;

        let version_pkt = build_version_request(0, 0xFFFF);
        if let Err(e) = usb.send(&version_pkt) {
            warn!("failed to send version request for device {}: {e}", dev.device_id);
        } else {
            info!("sent version request for device {}", dev.device_id);
        }

        let conn_mgr = ConnectionManager {
            devices: self.conn_mgr.devices.clone(),
        };
        let device_id = dev.device_id;
        let usb_for_task = usb.clone();

        tokio::spawn(async move {
            let mut reassembler = PacketReassembler::new();
            let mut version_negotiated = false;
            let mut usb_rx = usb_rx;
            let mut data_rx = data_rx;
            let mut connect_rx = connect_rx;

            loop {
                tokio::select! {
                    result = usb_rx.recv() => {
                        match result {
                            Some(raw_data) => {
                                if let Some(packet_data) = reassembler.feed(&raw_data, None) {
                                    Self::process_device_packet(
                                        &conn_mgr,
                                        &usb_for_task,
                                        device_id,
                                        &packet_data,
                                        &mut version_negotiated,
                                    ).await;
                                }
                            }
                            None => {
                                debug!("USB read channel closed for device {device_id}");
                                break;
                            }
                        }
                    }

                    result = data_rx.recv() => {
                        match result {
                            Some((sport, data)) => {
                                Self::send_client_data(
                                    &conn_mgr,
                                    &usb_for_task,
                                    device_id,
                                    sport,
                                    &data,
                                ).await;
                            }
                            None => {
                                debug!("data channel closed for device {device_id}");
                                break;
                            }
                        }
                    }

                    result = connect_rx.recv() => {
                        match result {
                            Some(req) => {
                                let conn_mgr_clone = ConnectionManager {
                                    devices: conn_mgr.devices.clone(),
                                };
                                let usb_clone = usb_for_task.clone();
                                tokio::spawn(async move {
                                    Self::handle_connect_request(
                                        &conn_mgr_clone,
                                        &usb_clone,
                                        req,
                                    ).await;
                                });
                            }
                            None => {
                                debug!("connect channel closed for device {device_id}");
                                break;
                            }
                        }
                    }
                }
            }

            info!("device {device_id} processing task stopped");
        });

        info!("device {} managed with USB mux interface", dev.device_id);
    }

    pub async fn remove_device(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        devices.remove(&device_id);
        self.conn_mgr.remove_device(device_id).await;
        info!("device {device_id} removed from manager");
    }

    async fn process_device_packet(
        conn_mgr: &ConnectionManager,
        usb: &Arc<AppleMuxInterface>,
        device_id: u32,
        data: &[u8],
        version_negotiated: &mut bool,
    ) {
        if data.len() >= 16 {
            debug!("raw mux data ({}B): {:02x?}", data.len(), &data[..data.len().min(64)]);
        }
        let packet = match parse_packet(data) {
            Ok(p) => p,
            Err(e) => {
                warn!("failed to parse mux packet on device {device_id}: {e}");
                return;
            }
        };

        match packet {
            MuxPacket::Version { major, minor } => {
                info!("device {device_id} version response: {major}.{minor}");
                if major >= 2 {
                    let setup_pkt = build_setup_packet(0, 0xFFFF);
                    if let Err(e) = usb.send(&setup_pkt) {
                        warn!("failed to send SETUP for device {device_id}: {e}");
                    } else {
                        info!("device {device_id} negotiated v2 protocol");
                        *version_negotiated = true;
                    }
                } else {
                    info!("device {device_id} using v1 protocol");
                    *version_negotiated = true;
                }

                let mut devices = conn_mgr.devices.write().await;
                if let Some(device) = devices.get_mut(&device_id) {
                    device.version = if major >= 2 { 2 } else { 1 };
                    device.state = DeviceState::Active;
                    device.tx_seq = 1;
                }
            }

            MuxPacket::Control { payload } => {
                if !payload.is_empty() {
                    match payload[0] {
                        3 => warn!("device {device_id} error: {}", String::from_utf8_lossy(&payload[1..])),
                        5 => warn!("device {device_id} warning: {}", String::from_utf8_lossy(&payload[1..])),
                        7 => info!("device {device_id}: {}", String::from_utf8_lossy(&payload[1..])),
                        _ => debug!("device {device_id} control: type={}", payload[0]),
                    }
                }
            }

            MuxPacket::Setup => {
                debug!("device {device_id} setup packet");
            }

            MuxPacket::Tcp { header, payload } => {
                let (events, send_queue) = conn_mgr.handle_tcp_packet(device_id, &header, &payload).await;
                for pkt in send_queue {
                    if let Err(e) = usb.send(&pkt) {
                        warn!("failed to send queued packet on device {device_id}: {e}");
                    }
                }
                for event in events {
                    match event {
                        ConnectionEvent::Connected { device_id: _, sport, tag: _ } => {
                            info!("device {device_id} connection established on sport={sport}");
                        }
                        ConnectionEvent::Refused { device_id: _, sport, tag: _ } => {
                            warn!("device {device_id} connection refused on sport={sport}");
                        }
                        ConnectionEvent::DataReady { device_id: _, sport } => {
                            debug!("device {device_id} data ready on sport={sport}");
                        }
                        ConnectionEvent::Disconnected { device_id: _, sport } => {
                            info!("device {device_id} connection disconnected on sport={sport}");
                        }
                        ConnectionEvent::Error { device_id: _, sport, error } => {
                            warn!("device {device_id} connection error on sport={sport}: {error}");
                        }
                    }
                }
            }
        }
    }

    async fn send_client_data(
        conn_mgr: &ConnectionManager,
        usb: &Arc<AppleMuxInterface>,
        device_id: u32,
        sport: u16,
        data: &[u8],
    ) {
        let (pkt, data_len) = {
            let devices = conn_mgr.devices.read().await;
            let device = match devices.get(&device_id) {
                Some(d) => d,
                None => {
                    warn!("send_client_data: device {device_id} not found");
                    return;
                }
            };

            let conn = match device.connections.get(&sport) {
                Some(c) => c,
                None => {
                    warn!("send_client_data: sport={sport} not found on device {device_id}");
                    return;
                }
            };

            if conn.state != ConnState::Connected {
                warn!("send_client_data: sport={sport} not connected on device {device_id}");
                return;
            }

            let tcp_seq = conn.tx_seq;
            let tcp_ack = conn.tx_ack;
            let win = (INITIAL_WINDOW >> 8) as u16;

            let pkt = build_tcp_packet(
                device.version,
                device.tx_seq,
                device.rx_seq,
                sport,
                conn.dport,
                tcp_seq,
                tcp_ack,
                TCP_ACK,
                win,
                Some(data),
            );

            (pkt, data.len() as u32)
        };

        if let Err(e) = usb.send(&pkt) {
            warn!("failed to send TCP data for device {device_id} sport={sport}: {e}");
            return;
        }

        info!("sent {} bytes TCP data on device {device_id} sport={sport}", data.len());

        let mut devices = conn_mgr.devices.write().await;
        if let Some(device) = devices.get_mut(&device_id) {
            device.tx_seq = device.tx_seq.wrapping_add(1);
            if let Some(conn) = device.connections.get_mut(&sport) {
                conn.tx_seq = conn.tx_seq.wrapping_add(data_len);
                conn.ob_buf.extend_from_slice(data);
            }
        }
    }

    async fn handle_connect_request(
        conn_mgr: &ConnectionManager,
        usb: &Arc<AppleMuxInterface>,
        req: ConnectRequest,
    ) {
        let sport = match conn_mgr.start_connect(req.device_id, req.dport, req.tag).await {
            Ok(s) => s,
            Err(e) => {
                let _ = req.resp_tx.send(Err(format!("connect failed: {e}")));
                return;
            }
        };

        let devices = conn_mgr.devices.read().await;
        let device = match devices.get(&req.device_id) {
            Some(d) => d,
            None => {
                let _ = req.resp_tx.send(Err("device not found".into()));
                return;
            }
        };

        let conn = match device.connections.get(&sport) {
            Some(c) => c,
            None => {
                let _ = req.resp_tx.send(Err("connection not found".into()));
                return;
            }
        };

        let syn_pkt = build_tcp_packet(
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

        debug!("SYN packet ({}B): {:02x?}", syn_pkt.len(), &syn_pkt[..syn_pkt.len().min(64)]);

        drop(devices);

        if let Err(e) = usb.send(&syn_pkt) {
            warn!("failed to send SYN for device {} sport={sport}: {e}", req.device_id);
            let _ = req.resp_tx.send(Err(format!("SYN send failed: {e}")));
            return;
        }

        info!("sent SYN for device {} sport={sport} dport={}", req.device_id, req.dport);

        let start = tokio::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);

        loop {
            if start.elapsed() > timeout {
                warn!("connect timeout for device {} sport={sport}", req.device_id);
                let _ = req.resp_tx.send(Err("connect timeout".into()));
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(10)).await;

            let devices = conn_mgr.devices.read().await;
            if let Some(device) = devices.get(&req.device_id) {
                if let Some(conn) = device.connections.get(&sport) {
                    match conn.state {
                        ConnState::Connected => {
                            info!("connection established for device {} sport={sport}", req.device_id);
                            let _ = req.resp_tx.send(Ok(sport));
                            return;
                        }
                        ConnState::Refused => {
                            warn!("connection refused for device {} sport={sport}", req.device_id);
                            let _ = req.resp_tx.send(Err("connection refused".into()));
                            return;
                        }
                        ConnState::Dead => {
                            warn!("connection dead for device {} sport={sport}", req.device_id);
                            let _ = req.resp_tx.send(Err("connection dead".into()));
                            return;
                        }
                        _ => continue,
                    }
                }
            }
        }
    }

    pub async fn connect(&self, device_id: u32, dport: u16, tag: u32) -> Result<u16, String> {
        let devices = self.devices.read().await;
        let device = devices.get(&device_id).ok_or("device not found")?;

        let (resp_tx, resp_rx) = oneshot::channel();
        let req = ConnectRequest {
            device_id,
            dport,
            tag,
            resp_tx,
        };

        device.connect_tx.send(req).await.map_err(|e| format!("channel send failed: {e}"))?;
        drop(devices);

        resp_rx.await.map_err(|e| format!("channel recv failed: {e}"))?
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}
