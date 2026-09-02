use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tokio::fs as tokio_fs;

use super::device_scanner::{DeviceScanner, DeviceChange};
use super::device_manager::{DeviceManager, MuxOutMsg};
use super::mux::{ConnState, ConnectionManager};
use super::protocol::{self, result, RawPacket};
use super::transport::TransportStream;
use crate::config::DaemonConfig;
use crate::metrics::Metrics;
use crate::platform::{self, PeerIdentity};
use crate::security::{validate_udid, sanitize_udid_for_path};

pub const LOCKDOWN_PORT: u16 = 62078;

/// Cap on accepted pair record payloads (defends against memory abuse).
const MAX_PAIR_RECORD_BYTES: usize = 4 * 1024 * 1024;

pub async fn handle_client(
    mut stream: TransportStream,
    scanner: Arc<tokio::sync::RwLock<DeviceScanner>>,
    event_tx: tokio::sync::broadcast::Sender<DeviceChange>,
    device_manager: Arc<DeviceManager>,
    metrics: Arc<Metrics>,
    config: DaemonConfig,
    peer: PeerIdentity,
) {
    metrics.clients_accepted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    metrics.clients_active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let id_str = match (&peer.credentials, &peer.sid) {
        (Some(c), _) => format!(" uid={} pid={:?}", c.uid, c.pid),
        (None, Some(sid)) => format!(" sid={sid}"),
        (None, None) => String::new(),
    };
    debug!("client connected{id_str}");

    // Peer auth: unix allowlists use UIDs; windows uses SIDs. No identity
    // (loopback TCP) passes only when the relevant allowlist is empty.
    if !peer_is_allowed(&peer, &config) {
        warn!("rejecting client: not in allowlist");
        metrics.clients_rejected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        metrics.clients_active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }

    let result = handle_client_inner(&mut stream, scanner, event_tx, device_manager, &metrics, &config).await;

    metrics.clients_active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

    if let Err(e) = result {
        debug!("client handler ended: {e}");
    }
}

pub fn peer_is_allowed(peer: &PeerIdentity, config: &DaemonConfig) -> bool {
    // UID check (unix). Empty allowlist = allow all. A missing credential
    // when an allowlist exists is a denial (fail-closed).
    if !config.allowed_uids.is_empty() {
        return match &peer.credentials {
            Some(creds) => config.allowed_uids.contains(&creds.uid),
            None => {
                warn!("peer identity unavailable but UID allowlist is set — denying");
                false
            }
        };
    }
    // SID check (windows). Empty allowlist = allow all; same fail-closed rule.
    if !config.allowed_sids.is_empty() {
        return match &peer.sid {
            Some(sid) => config.allowed_sids.iter().any(|s| s == sid),
            None => {
                warn!("peer SID unavailable but SID allowlist is set — denying");
                false
            }
        };
    }
    true
}

async fn handle_client_inner(
    stream: &mut TransportStream,
    scanner: Arc<tokio::sync::RwLock<DeviceScanner>>,
    event_tx: tokio::sync::broadcast::Sender<DeviceChange>,
    device_manager: Arc<DeviceManager>,
    metrics: &Arc<Metrics>,
    config: &DaemonConfig,
) -> Result<(), String> {
    loop {
        let packet = protocol::read_packet(stream, config.max_packet_bytes).await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    "client disconnected".to_string()
                } else {
                    format!("failed to read packet: {e}")
                }
            })?;

        metrics.commands_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let message_type = match packet.plist.get("MessageType") {
            Some(plist::Value::String(s)) => s.clone(),
            _ => {
                warn!("client sent packet without MessageType");
                let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                continue;
            }
        };

        debug!("client message: {message_type}");

        match message_type.as_str() {
            "ListDevices" => {
                metrics.list_devices_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let devices = {
                    let s = scanner.read().await;
                    s.get_devices()
                };
                info!("ListDevices: returning {} device(s)", devices.len());
                let resp = protocol::make_device_list_response(packet.tag, &devices);
                resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
            }

            "Listen" => {
                debug!("client subscribed to listen");
                let resp = protocol::make_result_response(packet.tag, 0);
                resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;

                let _listen_guard = crate::metrics::ListenGuard::new(metrics);

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
                            if let Err(e) = event_packet.write_to(stream).await {
                                debug!("client disconnected during listen: {e}");
                                return Ok(());
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("client lagged, missed {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("event channel closed");
                            return Ok(());
                        }
                    }
                }
            }

            "Connect" => {
                metrics.connects_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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

                if device_id == 0 {
                    warn!("Connect with invalid device_id=0");
                    let resp = protocol::make_result_response(packet.tag, result::BAD_DEVICE);
                    resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                    continue;
                }
                if port == 0 {
                    warn!("Connect with invalid port=0");
                    let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                    resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                    continue;
                }

                // Match usbmuxd semantics: unknown device IDs are a distinct
                // error from a refused connection.
                {
                    let s = scanner.read().await;
                    if s.get_device_by_id(device_id).is_none() {
                        warn!("Connect to unknown device_id={device_id}");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_DEVICE);
                        resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                        continue;
                    }
                }

                if port == LOCKDOWN_PORT && config.require_pair_record {
                    let udid: Option<String> = {
                        let s = scanner.read().await;
                        s.get_device_by_id(device_id).map(|d| d.udid.clone())
                    };
                    if let Some(ref udid) = udid {
                        if !has_pair_record(udid, &config.lockdown_dir).await {
                            warn!("Connect to lockdown port {LOCKDOWN_PORT} rejected: no pair record for {udid}");
                            let resp = protocol::make_result_response(packet.tag, result::PAIR_RECORD_REQUIRED);
                            resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                            continue;
                        }
                    }
                }

                match device_manager.connect(device_id, port, packet.tag).await {
                    Ok(sport) => {
                        info!("connection established: device_id={device_id} sport={sport}");

                        let resp = protocol::make_result_response(packet.tag, 0);
                        resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;

                        let data_tx = {
                            let devices = device_manager.devices.read().await;
                            devices.get(&device_id).map(|d| d.data_tx.clone())
                        };

                        let data_tx = match data_tx {
                            Some(tx) => tx,
                            None => {
                                warn!("device {device_id} not found for proxy");
                                return Err("device not found for proxy".into());
                            }
                        };

                        device_manager.increment_refcount(device_id).await;
                        metrics.proxies_started.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                        if let Err(e) = proxy_connection(
                            stream,
                            &device_manager.conn_mgr,
                            device_id,
                            sport,
                            data_tx,
                            metrics,
                            config,
                        ).await {
                            debug!("proxy ended: {e}");
                        }

                        device_manager.decrement_refcount(device_id).await;
                        metrics.proxies_ended.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return Ok(());
                    }
                    Err(e) => {
                        metrics.connect_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        warn!("connect failed: {e}");
                        let resp = protocol::make_result_response(packet.tag, result::CONNECTION_REFUSED);
                        resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                    }
                }
            }

            "ReadBUID" => {
                let buid = get_or_create_buid(&config.lockdown_dir).await;
                let mut resp_plist = plist::Dictionary::new();
                resp_plist.insert("BUID".into(), plist::Value::String(buid));
                let resp = RawPacket::new(resp_plist, protocol::XML_PLIST_VERSION, protocol::PLIST_MESSAGE_TYPE, packet.tag);
                resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
            }

            "ReadPairRecord" => {
                let pair_record_id = match packet.plist.get("PairRecordID") {
                    Some(plist::Value::String(s)) => s.clone(),
                    _ => {
                        warn!("ReadPairRecord without PairRecordID");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                        let _ = resp.write_to(stream).await;
                        continue;
                    }
                };

                if let Err(e) = validate_udid(&pair_record_id) {
                    warn!("ReadPairRecord invalid PairRecordID: {e}");
                    let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                    let _ = resp.write_to(stream).await;
                    continue;
                }

                match read_pair_record(&pair_record_id, &config.lockdown_dir).await {
                    Ok(data) => {
                        metrics.pair_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let mut resp_plist = plist::Dictionary::new();
                        resp_plist.insert("PairRecordData".into(), plist::Value::Data(data));
                        let resp = RawPacket::new(resp_plist, protocol::XML_PLIST_VERSION, protocol::PLIST_MESSAGE_TYPE, packet.tag);
                        resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                    }
                    Err(e) => {
                        warn!("failed to read pair record for {pair_record_id}: {e}");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_DEVICE);
                        let _ = resp.write_to(stream).await;
                    }
                }
            }

            "SavePairRecord" => {
                let pair_record_id = match packet.plist.get("PairRecordID") {
                    Some(plist::Value::String(s)) => s.clone(),
                    _ => {
                        warn!("SavePairRecord without PairRecordID");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                        let _ = resp.write_to(stream).await;
                        continue;
                    }
                };

                let pair_data = match packet.plist.get("PairRecordData") {
                    Some(plist::Value::Data(d)) => d.clone(),
                    _ => {
                        warn!("SavePairRecord without PairRecordData");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                        let _ = resp.write_to(stream).await;
                        continue;
                    }
                };

                if pair_data.len() > MAX_PAIR_RECORD_BYTES {
                    warn!("SavePairRecord payload too large: {} bytes", pair_data.len());
                    let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                    let _ = resp.write_to(stream).await;
                    continue;
                }

                if let Err(e) = validate_udid(&pair_record_id) {
                    warn!("SavePairRecord invalid PairRecordID: {e}");
                    let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                    let _ = resp.write_to(stream).await;
                    continue;
                }

                match save_pair_record(&pair_record_id, &pair_data, &config.lockdown_dir).await {
                    Ok(()) => {
                        metrics.pair_writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        info!("pair record saved for {pair_record_id}");
                        let resp = protocol::make_result_response(packet.tag, result::OK);
                        resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                    }
                    Err(e) => {
                        warn!("failed to save pair record for {pair_record_id}: {e}");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                        let _ = resp.write_to(stream).await;
                    }
                }
            }

            "DeletePairRecord" => {
                let pair_record_id = match packet.plist.get("PairRecordID") {
                    Some(plist::Value::String(s)) => s.clone(),
                    _ => {
                        warn!("DeletePairRecord without PairRecordID");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                        let _ = resp.write_to(stream).await;
                        continue;
                    }
                };

                if let Err(e) = validate_udid(&pair_record_id) {
                    warn!("DeletePairRecord invalid PairRecordID: {e}");
                    let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                    let _ = resp.write_to(stream).await;
                    continue;
                }

                match delete_pair_record(&pair_record_id, &config.lockdown_dir).await {
                    Ok(()) => {
                        metrics.pair_deletes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        info!("pair record deleted for {pair_record_id}");
                        let resp = protocol::make_result_response(packet.tag, result::OK);
                        resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
                    }
                    Err(e) => {
                        warn!("failed to delete pair record for {pair_record_id}: {e}");
                        let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                        let _ = resp.write_to(stream).await;
                    }
                }
            }

            "MeridianStats" => {
                let json = metrics.to_json();
                let resp = protocol::make_stats_response(packet.tag, &json);
                resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
            }

            _ => {
                warn!("unknown message type: {message_type}");
                let resp = protocol::make_result_response(packet.tag, result::BAD_COMMAND);
                resp.write_to(stream).await.map_err(|e| format!("write failed: {e}"))?;
            }
        }
    }
}

async fn has_pair_record(udid: &str, lockdown_dir: &std::path::Path) -> bool {
    if tokio_fs::metadata(lockdown_dir).await.is_err() {
        return false;
    }

    let raw = udid.trim().trim_end_matches('\0');
    let dashed = if raw.len() == 24 && !raw.contains('-') {
        format!("{}-{}", &raw[..8], &raw[8..])
    } else {
        raw.to_string()
    };

    let plist_path = lockdown_dir.join(format!("{raw}.plist"));
    if tokio_fs::metadata(&plist_path).await.is_ok() {
        return true;
    }
    let plist_path = lockdown_dir.join(format!("{dashed}.plist"));
    if tokio_fs::metadata(&plist_path).await.is_ok() {
        return true;
    }

    if let Ok(mut entries) = tokio_fs::read_dir(lockdown_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".plist") {
                let stem = name_str.trim_end_matches(".plist");
                if stem == raw || stem == dashed {
                    return true;
                }
            }
        }
    }

    false
}

async fn read_pair_record(udid: &str, lockdown_dir: &std::path::Path) -> Result<Vec<u8>, std::io::Error> {
    if tokio_fs::metadata(lockdown_dir).await.is_err() {
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
    if tokio_fs::metadata(&plist_path).await.is_ok() {
        return tokio_fs::read(&plist_path).await;
    }
    let plist_path = lockdown_dir.join(format!("{dashed}.plist"));
    if tokio_fs::metadata(&plist_path).await.is_ok() {
        return tokio_fs::read(&plist_path).await;
    }

    let mut entries = tokio_fs::read_dir(lockdown_dir).await.map_err(|e| {
        std::io::Error::new(e.kind(), format!("failed to read lockdown dir: {e}"))
    })?;

    while let Some(entry) = entries.next_entry().await.map_err(|e| {
        std::io::Error::new(e.kind(), format!("failed to read lockdown entry: {e}"))
    })? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".plist") {
            let stem = name_str.trim_end_matches(".plist");
            if stem == raw || stem == dashed {
                return tokio_fs::read(entry.path()).await;
            }
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("pair record not found for {udid}"),
    ))
}

async fn save_pair_record(udid: &str, data: &[u8], lockdown_dir: &std::path::Path) -> Result<(), std::io::Error> {
    if tokio_fs::metadata(lockdown_dir).await.is_err() {
        tokio_fs::create_dir_all(lockdown_dir).await?;
    }

    let safe_name = sanitize_udid_for_path(udid)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid UDID for path"))?;

    let plist_path = lockdown_dir.join(format!("{safe_name}.plist"));

    // Hardened write per platform: O_NOFOLLOW+0600 on unix,
    // symlink-refusal + SYSTEM/BA DACL on windows.
    platform::secure_write_secret(&plist_path, data).await
}

async fn delete_pair_record(udid: &str, lockdown_dir: &std::path::Path) -> Result<(), std::io::Error> {
    if tokio_fs::metadata(lockdown_dir).await.is_err() {
        return Ok(());
    }

    let raw = udid.trim().trim_end_matches('\0');
    let dashed = if raw.len() == 24 && !raw.contains('-') {
        format!("{}-{}", &raw[..8], &raw[8..])
    } else {
        raw.to_string()
    };

    let plist_path = lockdown_dir.join(format!("{raw}.plist"));
    if tokio_fs::metadata(&plist_path).await.is_ok() {
        tokio_fs::remove_file(&plist_path).await?;
        return Ok(());
    }
    let plist_path = lockdown_dir.join(format!("{dashed}.plist"));
    if tokio_fs::metadata(&plist_path).await.is_ok() {
        tokio_fs::remove_file(&plist_path).await?;
        return Ok(());
    }

    Ok(())
}

async fn get_or_create_buid(lockdown_dir: &std::path::Path) -> String {
    let buid_path = lockdown_dir.join("buid");
    if let Ok(buid) = tokio_fs::read_to_string(&buid_path).await {
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
        platform::random_u64().await,
    );

    if tokio_fs::metadata(lockdown_dir).await.is_err() {
        let _ = tokio_fs::create_dir_all(lockdown_dir).await;
    }
    // BUID is a secret: write it with full platform protections.
    let _ = platform::secure_write_secret(&buid_path, buid.as_bytes()).await;

    buid
}

async fn proxy_connection(
    stream: &mut TransportStream,
    conn_mgr: &ConnectionManager,
    device_id: u32,
    sport: u16,
    data_tx: mpsc::Sender<MuxOutMsg>,
    metrics: &Arc<Metrics>,
    config: &DaemonConfig,
) -> Result<(), String> {
    let notify = conn_mgr.get_data_notify(device_id, sport).await
        .ok_or("connection not found".to_string())?;

    let mut client_buf = vec![0u8; config.client_read_buf];

    let result = proxy_loop(stream, conn_mgr, device_id, sport, &data_tx, &notify, &mut client_buf, metrics).await;

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
    stream: &mut TransportStream,
    conn_mgr: &ConnectionManager,
    device_id: u32,
    sport: u16,
    data_tx: &mpsc::Sender<MuxOutMsg>,
    notify: &Arc<tokio::sync::Notify>,
    client_buf: &mut Vec<u8>,
    metrics: &Arc<Metrics>,
) -> Result<(), String> {
    loop {
        // Atomically drain the inbound buffer under a single write lock.
        // (Cloning under a read lock and clearing later loses any bytes the
        // device appended in between — a data-corruption race.)
        let data_to_client = {
            let mut devices = conn_mgr.devices.write().await;
            match devices.get_mut(&device_id) {
                Some(device) => match device.connections.get_mut(&sport) {
                    Some(conn) => {
                        if conn.state == ConnState::Dead {
                            return Ok(());
                        }
                        if conn.state != ConnState::Connected {
                            return Err("connection not connected".into());
                        }
                        if !conn.ib_buf.is_empty() {
                            Some(std::mem::take(&mut conn.ib_buf))
                        } else {
                            None
                        }
                    }
                    None => return Err("connection not found".into()),
                },
                None => return Err("device not found".into()),
            }
        };

        if let Some(data) = data_to_client {
            metrics.client_tx_bytes.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
            stream.write_all(&data).await.map_err(|e| e.to_string())?;
            // Buffer fully drained — reopen the TCP window so the device can
            // resume sending.
            if let Err(e) = data_tx.send(MuxOutMsg::WindowUpdate { sport }).await {
                return Err(format!("data channel send failed: {e}"));
            }
            continue;
        }

        let notified = notify.notified();
        tokio::pin!(notified);

        tokio::select! {
            _ = notified => {}
            result = stream.read(client_buf.as_mut_slice()) => {
                match result {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        metrics.client_rx_bytes.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        let data = client_buf[..n].to_vec();
                        if let Err(e) = data_tx.send(MuxOutMsg::Data { sport, payload: data }).await {
                            return Err(format!("data channel send failed: {e}"));
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_allowlist_uid() {
        let mut config = crate::config::DaemonConfig::default();
        let ident = |uid| crate::platform::PeerIdentity {
            credentials: Some(crate::security::PeerCredentials { uid, gid: uid, pid: None }),
            sid: None,
        };
        // Empty allowlist → allow everyone.
        assert!(peer_is_allowed(&ident(1234), &config));

        config.allowed_uids = vec![1000];
        assert!(peer_is_allowed(&ident(1000), &config));
        assert!(!peer_is_allowed(&ident(1001), &config));
        // Fail-closed: no identity + configured allowlist → deny.
        assert!(!peer_is_allowed(&crate::platform::PeerIdentity::anonymous(), &config));
    }

    #[test]
    fn test_peer_allowlist_sid() {
        let mut config = crate::config::DaemonConfig::default();
        config.allowed_sids = vec!["S-1-5-32-545".into()];
        let ident = |sid: &str| crate::platform::PeerIdentity {
            credentials: None,
            sid: Some(sid.into()),
        };
        assert!(peer_is_allowed(&ident("S-1-5-32-545"), &config));
        assert!(!peer_is_allowed(&ident("S-1-5-18"), &config));
        assert!(!peer_is_allowed(&crate::platform::PeerIdentity::anonymous(), &config));
    }

    #[test]
    fn test_validate_udid() {
        assert!(validate_udid("00008110-000C694914F3801E").is_ok());
        assert!(validate_udid("00008110000C694914F3801E").is_ok());
        assert!(validate_udid("../etc/passwd").is_err());
    }

    #[test]
    fn test_sanitize_udid_for_path() {
        assert!(sanitize_udid_for_path("00008110-000C694914F3801E").is_some());
        assert!(sanitize_udid_for_path("../../../etc/passwd").is_none());
    }

    #[tokio::test]
    async fn test_pair_record_exact_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        tokio_fs::write(path.join("ABCDEF1234567890.plist"), b"target").await.unwrap();
        tokio_fs::write(path.join("ABCDEF1234567890ABCD.plist"), b"other").await.unwrap();

        delete_pair_record("ABCDEF1234567890", path).await.unwrap();

        assert!(!path.join("ABCDEF1234567890.plist").exists());
        assert!(path.join("ABCDEF1234567890ABCD.plist").exists());
    }

    #[tokio::test]
    async fn test_pair_record_save_safeguard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let result = save_pair_record("../../../etc/evil", b"data", path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_pair_record_read_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_pair_record("NONEXISTENT12345678901", dir.path()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_pair_record_save_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let udid = "00008110-000C694914F3801E";
        let target = dir.path().join("victim.plist");
        tokio_fs::write(&target, b"sensitive").await.unwrap();
        std::os::unix::fs::symlink(&target, path.join(format!("{udid}.plist"))).unwrap();

        let result = save_pair_record(udid, b"overwrite", path).await;
        assert!(result.is_err(), "O_NOFOLLOW must reject symlinked pair record files");
        let victim = tokio_fs::read(&target).await.unwrap();
        assert_eq!(victim, b"sensitive", "symlink target must be untouched");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_pair_record_save_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let udid = "00008110-000C694914F3801E";
        save_pair_record(udid, b"data", dir.path()).await.unwrap();
        let meta = std::fs::metadata(dir.path().join(format!("{udid}.plist"))).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
