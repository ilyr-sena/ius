use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::mux::{
    ConnState, ConnectionManager, ConnectionEvent, DeviceState,
    build_version_request, build_setup_packet, build_tcp_packet, build_control_packet,
    parse_packet, scaled_window, MuxPacket, TCP_SYN, TCP_ACK, INITIAL_WINDOW,
};
use super::usb::{AppleMuxInterface, UsbReader};
use super::mux::PacketReassembler;
use crate::daemon::device_scanner::UsbDevice;
use crate::config::DaemonConfig;
use crate::metrics::Metrics;

/// Message sent from client proxies to the per-device processing task.
pub enum MuxOutMsg {
    /// Raw payload to transmit on the given source port.
    Data { sport: u16, payload: Vec<u8> },
    /// The client has drained the connection buffer; send a window update ACK.
    WindowUpdate { sport: u16 },
}

pub struct ManagedDevice {
    pub device_id: u32,
    pub usb: Arc<AppleMuxInterface>,
    pub data_tx: mpsc::Sender<MuxOutMsg>,
    pub connect_tx: mpsc::Sender<ConnectRequest>,
    pub refcount: u32,
}

pub struct ConnectRequest {
    pub device_id: u32,
    pub dport: u16,
    pub tag: u32,
    pub resp_tx: oneshot::Sender<Result<u16, String>>,
}

pub struct DeviceManager {
    pub devices: Arc<tokio::sync::RwLock<HashMap<u32, ManagedDevice>>>,
    pub conn_mgr: ConnectionManager,
    pub config: DaemonConfig,
    pub metrics: Arc<Metrics>,
}

impl DeviceManager {
    pub fn new(config: DaemonConfig, metrics: Arc<Metrics>) -> Self {
        Self {
            devices: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            conn_mgr: ConnectionManager::new(),
            config,
            metrics,
        }
    }

    pub async fn increment_refcount(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        if let Some(dev) = devices.get_mut(&device_id) {
            dev.refcount += 1;
            debug!("device {device_id} refcount incremented to {}", dev.refcount);
        }
    }

    pub async fn decrement_refcount(&self, device_id: u32) {
        let mut devices = self.devices.write().await;
        if let Some(dev) = devices.get_mut(&device_id) {
            dev.refcount = dev.refcount.saturating_sub(1);
            debug!("device {device_id} refcount decremented to {}", dev.refcount);
        }
    }

    pub async fn add_device(&self, dev: &UsbDevice) {
        {
            let devices = self.devices.read().await;
            if devices.contains_key(&dev.device_id) {
                debug!("device {} already managed", dev.device_id);
                return;
            }
        }

        let udid_for_block = dev.udid.clone();
        let usb_timeout = self.config.usb_timeout;
        let usb = match tokio::task::spawn_blocking(move || -> Result<Arc<AppleMuxInterface>, ()> {
            let all_devices = match rusb::devices() {
                Ok(d) => d,
                Err(e) => {
                    warn!("failed to list USB devices for {udid_for_block}: {e}");
                    return Err(());
                }
            };
            let mut found = None;
            for device in all_devices.iter() {
                if let Ok(desc) = device.device_descriptor() {
                    if desc.vendor_id() == 0x05AC {
                        if let Ok(handle) = device.open() {
                            if let Ok(serial) = handle.read_serial_number_string_ascii(&desc) {
                                if serial.trim().trim_end_matches('\0') == udid_for_block {
                                    found = Some(device.clone());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            let rusb_dev = match found {
                Some(d) => d,
                None => {
                    warn!("failed to find USB device for {udid_for_block}: not found");
                    return Err(());
                }
            };

            match AppleMuxInterface::open(&rusb_dev, usb_timeout) {
                Ok(u) => Ok(Arc::new(u)),
                Err(e) => {
                    warn!("failed to open mux interface for {udid_for_block}: {e} (will retry next scan)");
                    Err(())
                }
            }
        }).await {
            Ok(Ok(u)) => u,
            _ => {
                return;
            }
        };

        let (usb_tx, usb_rx) = mpsc::channel::<Vec<u8>>(self.config.max_data_channel);
        let (data_tx, data_rx) = mpsc::channel::<MuxOutMsg>(self.config.max_data_channel);
        let (connect_tx, connect_rx) = mpsc::channel::<ConnectRequest>(self.config.connect_channel);

        // Single ordered reader: concurrent bulk reads on one endpoint can
        // complete out of order and corrupt the mux byte stream.
        let reader = UsbReader::new(usb.clone(), usb_tx.clone(), self.metrics.clone());
        reader.spawn();

        let managed = ManagedDevice {
            device_id: dev.device_id,
            usb: usb.clone(),
            data_tx,
            connect_tx,
            refcount: 0,
        };

        {
            let mut devices = self.devices.write().await;
            devices.insert(dev.device_id, managed);
        }

        // NOTE: devices_attached is owned by the scanner loop (it stores the
        // authoritative scan count); do not increment here.

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
        let metrics = self.metrics.clone();
        let max_conn_buffer = self.config.max_conn_buffer;
        let max_reassembly_bytes = self.config.max_reassembly_bytes;
        let devices_map = self.devices.clone();

        tokio::spawn(async move {
            let mut reassembler = PacketReassembler::new(max_reassembly_bytes);
            let mut version_negotiated = false;
            let mut usb_rx = usb_rx;
            let mut data_rx = data_rx;
            let mut connect_rx = connect_rx;

            let mut disconnect_detected = false;

            loop {
                tokio::select! {
                    result = usb_rx.recv() => {
                        match result {
                            Some(raw_data) => {
                                metrics.usb_rx_bytes.fetch_add(raw_data.len() as u64, Ordering::Relaxed);
                                for packet_data in reassembler.feed(&raw_data) {
                                    Self::process_device_packet(
                                        &conn_mgr,
                                        &usb_for_task,
                                        device_id,
                                        &packet_data,
                                        &mut version_negotiated,
                                        &metrics,
                                        max_conn_buffer,
                                    ).await;
                                }
                            }
                            None => {
                                info!("USB read channel closed for device {device_id} — disconnect detected");
                                Self::handle_usb_disconnect(&conn_mgr, &devices_map, device_id).await;
                                disconnect_detected = true;
                                break;
                            }
                        }
                    }

                    result = data_rx.recv() => {
                        match result {
                            Some(MuxOutMsg::Data { sport, payload }) => {
                                Self::send_client_data(
                                    &conn_mgr,
                                    &usb_for_task,
                                    device_id,
                                    sport,
                                    &payload,
                                    &metrics,
                                ).await;
                            }
                            Some(MuxOutMsg::WindowUpdate { sport }) => {
                                Self::send_window_update(
                                    &conn_mgr,
                                    &usb_for_task,
                                    device_id,
                                    sport,
                                    max_conn_buffer,
                                    &metrics,
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
                                let metrics_clone = metrics.clone();
                                tokio::spawn(async move {
                                    Self::handle_connect_request(
                                        &conn_mgr_clone,
                                        &usb_clone,
                                        req,
                                        &metrics_clone,
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

            if disconnect_detected {
                Self::cleanup_device_connections(&conn_mgr, device_id).await;
            }

            info!("device {device_id} processing task stopped");
        });

        info!("device {} managed with USB mux interface", dev.device_id);
    }

    pub async fn remove_device(&self, device_id: u32) {
        {
            let mut devices = self.devices.write().await;
            devices.remove(&device_id);
        }
        self.conn_mgr.remove_device(device_id).await;
        // NOTE: devices_attached is owned by the scanner loop; do not decrement here.
        info!("device {device_id} removed from manager");
    }

    /// Handle USB-layer disconnect: tear down all connections and drop the
    /// managed device so its USB handle and channels are released.
    async fn handle_usb_disconnect(
        conn_mgr: &ConnectionManager,
        devices_map: &Arc<tokio::sync::RwLock<HashMap<u32, ManagedDevice>>>,
        device_id: u32,
    ) {
        Self::cleanup_device_connections(conn_mgr, device_id).await;

        let mut devices = devices_map.write().await;
        if devices.remove(&device_id).is_some() {
            info!("device {device_id} removed from manager after USB disconnect");
        }
    }

    /// After the client drains buffered data, advertise the reopened window so
    /// the device can resume sending.
    async fn send_window_update(
        conn_mgr: &ConnectionManager,
        usb: &Arc<AppleMuxInterface>,
        device_id: u32,
        sport: u16,
        max_conn_buffer: usize,
        metrics: &Arc<Metrics>,
    ) {
        let pkt = {
            let devices = conn_mgr.devices.read().await;
            let device = match devices.get(&device_id) {
                Some(d) => d,
                None => return,
            };
            let conn = match device.connections.get(&sport) {
                Some(c) => c,
                None => return,
            };
            if !conn.state.is_active() {
                return;
            }
            let free = max_conn_buffer.saturating_sub(conn.ib_buf.len());
            build_tcp_packet(
                device.version,
                device.tx_seq,
                device.rx_seq,
                sport,
                conn.dest_port,
                conn.tx_seq,
                conn.tx_ack,
                TCP_ACK,
                scaled_window(free),
                None,
            )
        };

        metrics.usb_tx_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        if let Err(e) = usb.send(&pkt) {
            warn!("failed to send window update for device {device_id} sport={sport}: {e}");
            return;
        }

        let mut devices = conn_mgr.devices.write().await;
        if let Some(device) = devices.get_mut(&device_id) {
            device.tx_seq = device.tx_seq.wrapping_add(1);
        }
    }

    async fn cleanup_device_connections(conn_mgr: &ConnectionManager, device_id: u32) {
        let mut devices = conn_mgr.devices.write().await;
        if let Some(device) = devices.get_mut(&device_id) {
            let sport_keys: Vec<u16> = device.connections.keys().copied().collect();
            for sport in &sport_keys {
                if let Some(conn) = device.connections.get_mut(sport) {
                    conn.state = ConnState::Dead;
                    conn.data_notify.notify_one();
                }
            }
            info!("cleaned up {} connections for device {device_id}", sport_keys.len());
        }
    }

    async fn process_device_packet(
        conn_mgr: &ConnectionManager,
        usb: &Arc<AppleMuxInterface>,
        device_id: u32,
        data: &[u8],
        version_negotiated: &mut bool,
        metrics: &Arc<Metrics>,
        max_conn_buffer: usize,
    ) {
        let packet = match parse_packet(data) {
            Ok(p) => p,
            Err(e) => {
                metrics.parse_errors.fetch_add(1, Ordering::Relaxed);
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

                let win_pkt = build_control_packet(
                    if major >= 2 { 2 } else { 1 },
                    0, 0xFFFF,
                    &build_device_capabilities(),
                );
                if let Err(e) = usb.send(&win_pkt) {
                    warn!("failed to send WIN for device {device_id}: {e}");
                } else {
                    debug!("sent WIN capabilities for device {device_id}");
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
                let (events, send_queue) = conn_mgr.handle_tcp_packet(device_id, &header, &payload, metrics, max_conn_buffer).await;
                for pkt in send_queue {
                    metrics.usb_tx_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
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
                            metrics.rsts_received.fetch_add(1, Ordering::Relaxed);
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
        metrics: &Arc<Metrics>,
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
                conn.dest_port,
                tcp_seq,
                tcp_ack,
                TCP_ACK,
                win,
                Some(data),
            );

            (pkt, data.len() as u32)
        };

        metrics.usb_tx_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        if let Err(e) = usb.send(&pkt) {
            warn!("failed to send TCP data for device {device_id} sport={sport}: {e}");
            return;
        }

        let mut devices = conn_mgr.devices.write().await;
        if let Some(device) = devices.get_mut(&device_id) {
            device.tx_seq = device.tx_seq.wrapping_add(1);
            if let Some(conn) = device.connections.get_mut(&sport) {
                conn.tx_seq = conn.tx_seq.wrapping_add(data_len);
                    conn.ob_buf.extend(data.iter().copied());
            }
        }
    }

    async fn handle_connect_request(
        conn_mgr: &ConnectionManager,
        usb: &Arc<AppleMuxInterface>,
        req: ConnectRequest,
        metrics: &Arc<Metrics>,
    ) {
        metrics.connects_total.fetch_add(1, Ordering::Relaxed);

        let sport = match conn_mgr.start_connect(req.device_id, req.dport, req.tag).await {
            Ok(s) => s,
            Err(e) => {
                metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
                let _ = req.resp_tx.send(Err(format!("connect failed: {e}")));
                return;
            }
        };

        let devices = conn_mgr.devices.read().await;
        let device = match devices.get(&req.device_id) {
            Some(d) => d,
            None => {
                metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
                let _ = req.resp_tx.send(Err("device not found".into()));
                return;
            }
        };

        let conn = match device.connections.get(&sport) {
            Some(c) => c,
            None => {
                metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
                let _ = req.resp_tx.send(Err("connection not found".into()));
                return;
            }
        };

        let syn_pkt = build_tcp_packet(
            device.version,
            device.tx_seq,
            device.rx_seq,
            sport,
            conn.dest_port,
            0,
            0,
            TCP_SYN,
            (INITIAL_WINDOW >> 8) as u16,
            None,
        );

        drop(devices);

        if let Err(e) = usb.send(&syn_pkt) {
            metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
            warn!("failed to send SYN for device {} sport={sport}: {e}", req.device_id);
            let _ = req.resp_tx.send(Err(format!("SYN send failed: {e}")));
            return;
        }

        info!("sent SYN for device {} sport={sport} dport={}", req.device_id, req.dport);

        let data_notify = conn_mgr.get_data_notify(req.device_id, sport).await;

        let start = tokio::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);

        loop {
            if start.elapsed() > timeout {
                metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
                warn!("connect timeout for device {} sport={sport}", req.device_id);
                let _ = req.resp_tx.send(Err("connect timeout".into()));
                return;
            }

            {
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
                                metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
                                warn!("connection refused for device {} sport={sport}", req.device_id);
                                let _ = req.resp_tx.send(Err("connection refused".into()));
                                return;
                            }
                            ConnState::Dead => {
                                metrics.connect_failures.fetch_add(1, Ordering::Relaxed);
                                warn!("connection dead for device {} sport={sport}", req.device_id);
                                let _ = req.resp_tx.send(Err("connection dead".into()));
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            }

            if let Some(ref notify) = data_notify {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

        match device.connect_tx.try_send(req) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err("device busy (too many pending connects)".into());
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err("device processing task stopped".into());
            }
        }
        drop(devices);

        resp_rx.await.map_err(|e| format!("channel recv failed: {e}"))?
    }
}

fn build_device_capabilities() -> Vec<u8> {
    let mut caps = plist::Dictionary::new();
    caps.insert("AllowsSimulators".into(), plist::Value::Boolean(false));
    caps.insert("SupportsLockdown".into(), plist::Value::Boolean(true));
    caps.insert("SupportsPairing".into(), plist::Value::Boolean(true));
    caps.insert("SupportsSSL".into(), plist::Value::Boolean(true));

    let mut features = plist::Dictionary::new();
    features.insert("com.apple.mobiledevice_proxy".into(), plist::Value::Integer(0.into()));
    caps.insert("FeatureSet".into(), plist::Value::Dictionary(features));

    let mut buf = Vec::new();
    if let Err(e) = plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(caps)) {
        tracing::error!("failed to serialize device capabilities: {e}");
    }
    buf
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new(DaemonConfig::default(), std::sync::Arc::new(Metrics::new()))
    }
}
