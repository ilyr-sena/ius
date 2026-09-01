use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::device_scanner::{DeviceScanner, DeviceChange};
use super::device_manager::DeviceManager;
use super::mux::{ConnState, ConnectionManager};
use super::protocol::{self, RawPacket};

const LOCKDOWN_DIR: &str = "/var/lib/lockdown";
const LOCKDOWN_PORT: u16 = 62078;

pub async fn handle_client(
    mut stream: UnixStream,
    scanner: Arc<RwLock<DeviceScanner>>,
    event_tx: tokio::sync::broadcast::Sender<DeviceChange>,
    device_manager: Arc<DeviceManager>,
) {
    info!("new client connected");

    loop {
        let packet = match protocol::read_packet(&mut stream).await {
            Ok(p) => p,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    debug!("client disconnected");
                } else {
                    warn!("failed to read packet from client: {e}");
                }
                return;
            }
        };

        let message_type = match packet.plist.get("MessageType") {
            Some(plist::Value::String(s)) => s.clone(),
            _ => {
                warn!("client sent packet without MessageType");
                let resp = protocol::make_result_response(packet.tag, 1);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write response: {e}");
                    return;
                }
                continue;
            }
        };

        debug!("client message: {message_type}");

        match message_type.as_str() {
            "ListDevices" => {
                let devices = {
                    let s = scanner.read().unwrap();
                    s.get_devices()
                };
                info!("ListDevices: returning {} device(s)", devices.len());
                for d in &devices {
                    info!("  - id={} serial={}", d.device_id, d.udid);
                }
                let resp = protocol::make_device_list_response(packet.tag, &devices);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write device list: {e}");
                    return;
                }
            }

            "Listen" => {
                debug!("client subscribed to listen");
                let resp = protocol::make_result_response(packet.tag, 0);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write listen ack: {e}");
                    return;
                }

                let mut rx = event_tx.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(change) => {
                            let event_packet = match change {
                                DeviceChange::Attached(ref dev) => {
                                    debug!("sending Attached event for {}", dev.udid);
                                    protocol::make_attached_event(0, dev)
                                }
                                DeviceChange::Detached { device_id } => {
                                    debug!("sending Detached event for id={device_id}");
                                    protocol::make_detached_event(0, device_id)
                                }
                            };
                            if let Err(e) = event_packet.write_to(&mut stream).await {
                                debug!("client disconnected during listen: {e}");
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("client lagged, missed {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("event channel closed");
                            return;
                        }
                    }
                }
            }

            "Connect" => {
                let device_id = match packet.plist.get("DeviceID") {
                    Some(plist::Value::Integer(i)) => i.as_unsigned().unwrap_or(0) as u32,
                    _ => 0,
                };
                let port = match packet.plist.get("PortNumber") {
                    Some(plist::Value::Integer(i)) => {
                        let raw = i.as_unsigned().unwrap_or(0) as u16;
                        u16::from_be(raw)
                    }
                    _ => 0,
                };

                info!("Connect request: device_id={device_id} port={port}");

                if port == LOCKDOWN_PORT {
                    let udid: Option<String> = {
                        let s = scanner.read().unwrap();
                        s.get_device_by_id(device_id).map(|d| d.udid.clone())
                    };
                    if let Some(ref udid) = udid {
                        if !has_pair_record(udid) {
                            warn!("Connect to lockdown port {LOCKDOWN_PORT} rejected: no pair record for {udid}");
                            let resp = protocol::make_result_response(packet.tag, 8);
                            if let Err(e) = resp.write_to(&mut stream).await {
                                warn!("failed to write connect error: {e}");
                                return;
                            }
                            continue;
                        }
                    }
                }

                match device_manager.connect(device_id, port, packet.tag).await {
                    Ok(sport) => {
                        info!("connection established: device_id={device_id} sport={sport}");

                        let resp = protocol::make_result_response(packet.tag, 0);
                        if let Err(e) = resp.write_to(&mut stream).await {
                            warn!("failed to write connect response: {e}");
                            return;
                        }

                        let data_tx = {
                            let devices = device_manager.devices.read().await;
                            devices.get(&device_id).map(|d| d.data_tx.clone())
                        };

                        let data_tx = match data_tx {
                            Some(tx) => tx,
                            None => {
                                warn!("device {device_id} not found for proxy");
                                return;
                            }
                        };

                        device_manager.increment_refcount(device_id).await;

                        if let Err(e) = proxy_connection(
                            &mut stream,
                            &device_manager.conn_mgr,
                            device_id,
                            sport,
                            data_tx,
                        ).await {
                            debug!("proxy ended: {e}");
                        }

                        device_manager.decrement_refcount(device_id).await;
                        return;
                    }
                    Err(e) => {
                        warn!("connect failed: {e}");
                        let resp = protocol::make_result_response(packet.tag, 3);
                        if let Err(e) = resp.write_to(&mut stream).await {
                            warn!("failed to write connect error: {e}");
                            return;
                        }
                    }
                }
            }

            "ReadBUID" => {
                let buid = get_or_create_buid();
                let mut resp_plist = plist::Dictionary::new();
                resp_plist.insert("BUID".into(), plist::Value::String(buid));
                let resp = RawPacket::new(resp_plist, protocol::XML_PLIST_VERSION, protocol::PLIST_MESSAGE_TYPE, packet.tag);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write BUID: {e}");
                    return;
                }
            }

            "ReadPairRecord" => {
                let pair_record_id = match packet.plist.get("PairRecordID") {
                    Some(plist::Value::String(s)) => s.clone(),
                    _ => {
                        warn!("ReadPairRecord without PairRecordID");
                        let resp = protocol::make_result_response(packet.tag, 1);
                        let _ = resp.write_to(&mut stream).await;
                        continue;
                    }
                };

                match read_pair_record(&pair_record_id) {
                    Ok(data) => {
                        let mut resp_plist = plist::Dictionary::new();
                        resp_plist.insert("PairRecordData".into(), plist::Value::Data(data));
                        let resp = RawPacket::new(resp_plist, protocol::XML_PLIST_VERSION, protocol::PLIST_MESSAGE_TYPE, packet.tag);
                        if let Err(e) = resp.write_to(&mut stream).await {
                            warn!("failed to write pair record: {e}");
                            return;
                        }
                    }
                    Err(e) => {
                        warn!("failed to read pair record for {pair_record_id}: {e}");
                        let resp = protocol::make_result_response(packet.tag, 2);
                        let _ = resp.write_to(&mut stream).await;
                    }
                }
            }

            "SavePairRecord" => {
                let pair_record_id = match packet.plist.get("PairRecordID") {
                    Some(plist::Value::String(s)) => s.clone(),
                    _ => {
                        warn!("SavePairRecord without PairRecordID");
                        let resp = protocol::make_result_response(packet.tag, 1);
                        let _ = resp.write_to(&mut stream).await;
                        continue;
                    }
                };

                let pair_data = match packet.plist.get("PairRecordData") {
                    Some(plist::Value::Data(d)) => d.clone(),
                    _ => {
                        warn!("SavePairRecord without PairRecordData");
                        let resp = protocol::make_result_response(packet.tag, 1);
                        let _ = resp.write_to(&mut stream).await;
                        continue;
                    }
                };

                match save_pair_record(&pair_record_id, &pair_data) {
                    Ok(()) => {
                        info!("pair record saved for {pair_record_id}");
                        let resp = protocol::make_result_response(packet.tag, 0);
                        if let Err(e) = resp.write_to(&mut stream).await {
                            warn!("failed to write save response: {e}");
                            return;
                        }
                    }
                    Err(e) => {
                        warn!("failed to save pair record for {pair_record_id}: {e}");
                        let resp = protocol::make_result_response(packet.tag, 1);
                        let _ = resp.write_to(&mut stream).await;
                    }
                }
            }

            "DeletePairRecord" => {
                let pair_record_id = match packet.plist.get("PairRecordID") {
                    Some(plist::Value::String(s)) => s.clone(),
                    _ => {
                        warn!("DeletePairRecord without PairRecordID");
                        let resp = protocol::make_result_response(packet.tag, 1);
                        let _ = resp.write_to(&mut stream).await;
                        continue;
                    }
                };

                match delete_pair_record(&pair_record_id) {
                    Ok(()) => {
                        info!("pair record deleted for {pair_record_id}");
                        let resp = protocol::make_result_response(packet.tag, 0);
                        if let Err(e) = resp.write_to(&mut stream).await {
                            warn!("failed to write delete response: {e}");
                            return;
                        }
                    }
                    Err(e) => {
                        warn!("failed to delete pair record for {pair_record_id}: {e}");
                        let resp = protocol::make_result_response(packet.tag, 1);
                        let _ = resp.write_to(&mut stream).await;
                    }
                }
            }

            _ => {
                warn!("unknown message type: {message_type}");
                let resp = protocol::make_result_response(packet.tag, 1);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write error response: {e}");
                    return;
                }
            }
        }
    }
}

fn has_pair_record(udid: &str) -> bool {
    let lockdown_dir = std::path::Path::new(LOCKDOWN_DIR);
    if !lockdown_dir.exists() {
        return false;
    }

    let raw = udid.trim().trim_end_matches('\0');
    let dashed = if raw.len() == 24 && !raw.contains('-') {
        format!("{}-{}", &raw[..8], &raw[8..])
    } else {
        raw.to_string()
    };

    let plist_path = lockdown_dir.join(format!("{raw}.plist"));
    if plist_path.exists() {
        return true;
    }
    let plist_path = lockdown_dir.join(format!("{dashed}.plist"));
    if plist_path.exists() {
        return true;
    }

    if let Ok(entries) = std::fs::read_dir(lockdown_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".plist")
                && (name_str.contains(raw) || name_str.contains(&dashed))
            {
                return true;
            }
        }
    }

    false
}

fn read_pair_record(udid: &str) -> Result<Vec<u8>, std::io::Error> {
    let lockdown_dir = std::path::Path::new(LOCKDOWN_DIR);
    if !lockdown_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "lockdown directory not found",
        ));
    }

    let raw = udid.trim().trim_end_matches('\0');
    let dashed = if raw.len() == 24 && !raw.contains('-') {
        format!("{}-{}", &raw[..8], &raw[8..])
    } else {
        raw.to_string()
    };

    let plist_path = lockdown_dir.join(format!("{raw}.plist"));
    if plist_path.exists() {
        return std::fs::read(&plist_path);
    }
    let plist_path = lockdown_dir.join(format!("{dashed}.plist"));
    if plist_path.exists() {
        return std::fs::read(&plist_path);
    }

    for entry in std::fs::read_dir(lockdown_dir).map_err(|e| {
        std::io::Error::new(e.kind(), format!("failed to read lockdown dir: {e}"))
    })? {
        let entry = entry.map_err(|e| {
            std::io::Error::new(e.kind(), format!("failed to read lockdown entry: {e}"))
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".plist")
            && (name_str.contains(raw) || name_str.contains(&dashed))
        {
            return std::fs::read(entry.path());
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("pair record not found for {udid}"),
    ))
}

fn save_pair_record(udid: &str, data: &[u8]) -> Result<(), std::io::Error> {
    let lockdown_dir = std::path::Path::new(LOCKDOWN_DIR);
    if !lockdown_dir.exists() {
        std::fs::create_dir_all(lockdown_dir)?;
    }

    let plist_path = lockdown_dir.join(format!("{udid}.plist"));
    std::fs::write(&plist_path, data)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&plist_path, perms)?;
    }

    Ok(())
}

fn delete_pair_record(udid: &str) -> Result<(), std::io::Error> {
    let lockdown_dir = std::path::Path::new(LOCKDOWN_DIR);
    if !lockdown_dir.exists() {
        return Ok(());
    }

    let plist_path = lockdown_dir.join(format!("{udid}.plist"));
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
        return Ok(());
    }

    for entry in std::fs::read_dir(lockdown_dir).map_err(|e| {
        std::io::Error::new(e.kind(), format!("failed to read lockdown dir: {e}"))
    })? {
        let entry = entry.map_err(|e| {
            std::io::Error::new(e.kind(), format!("failed to read lockdown entry: {e}"))
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".plist") && name_str.contains(udid) {
            std::fs::remove_file(entry.path())?;
            return Ok(());
        }
    }

    Ok(())
}

fn get_or_create_buid() -> String {
    let buid_path = std::path::Path::new(LOCKDOWN_DIR).join("buid");
    if let Ok(buid) = std::fs::read_to_string(&buid_path) {
        let trimmed = buid.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }

    let buid = format!("{:016x}{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
        rand_u64(),
    );

    let lockdown_dir = std::path::Path::new(LOCKDOWN_DIR);
    if !lockdown_dir.exists() {
        let _ = std::fs::create_dir_all(lockdown_dir);
    }
    let _ = std::fs::write(&buid_path, &buid);

    buid
}

fn rand_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(0xDEADBEEF);
    h.finish()
}

async fn proxy_connection(
    stream: &mut UnixStream,
    conn_mgr: &ConnectionManager,
    device_id: u32,
    sport: u16,
    data_tx: mpsc::Sender<(u16, Vec<u8>)>,
) -> Result<(), String> {
    let notify = conn_mgr.get_data_notify(device_id, sport).await
        .ok_or("connection not found".to_string())?;

    let mut client_buf = [0u8; 65536];

    let result = proxy_loop(stream, conn_mgr, device_id, sport, &data_tx, &notify, &mut client_buf).await;

    debug!("proxy: client disconnected on sport={sport}, cleaning up connection");

    {
        let mut devices = conn_mgr.devices.write().await;
        if let Some(device) = devices.get_mut(&device_id) {
            device.connections.remove(&sport);
            debug!("proxy: removed connection sport={sport}");
        }
    }

    result
}

async fn proxy_loop(
    stream: &mut UnixStream,
    conn_mgr: &ConnectionManager,
    device_id: u32,
    sport: u16,
    data_tx: &mpsc::Sender<(u16, Vec<u8>)>,
    notify: &Arc<tokio::sync::Notify>,
    client_buf: &mut [u8; 65536],
) -> Result<(), String> {
    loop {
        let data_to_client = {
            let devices = conn_mgr.devices.read().await;
            if let Some(device) = devices.get(&device_id) {
                if let Some(conn) = device.connections.get(&sport) {
                    if conn.state == ConnState::Dead {
                        return Ok(());
                    }
                    if conn.state != ConnState::Connected {
                        return Err("connection not connected".into());
                    }
                    if !conn.ib_buf.is_empty() {
                        Some(conn.ib_buf.clone())
                    } else {
                        None
                    }
                } else {
                    return Err("connection not found".into());
                }
            } else {
                return Err("device not found".into());
            }
        };

        if let Some(data) = data_to_client {
            stream.write_all(&data).await.map_err(|e| e.to_string())?;
            let mut devices = conn_mgr.devices.write().await;
            if let Some(device) = devices.get_mut(&device_id) {
                if let Some(conn) = device.connections.get_mut(&sport) {
                    conn.ib_buf.clear();
                }
            }
            continue;
        }

        let notified = notify.notified();
        tokio::pin!(notified);

        tokio::select! {
            _ = notified => {}
            result = stream.read(client_buf) => {
                match result {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        let data = client_buf[..n].to_vec();
                        if let Err(e) = data_tx.send((sport, data)).await {
                            return Err(format!("data channel send failed: {e}").into());
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        }

        tokio::task::yield_now().await;
    }
}
