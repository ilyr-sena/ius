use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use super::device_scanner::{DeviceScanner, DeviceChange};
use super::device_manager::DeviceManager;
use super::mux::{self, ConnState, ConnectionManager};
use super::protocol::{self, RawPacket};

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
                debug!("returning {} device(s)", devices.len());
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
                        i.as_unsigned().unwrap_or(0) as u16
                    }
                    _ => 0,
                };

                info!("Connect request: device_id={device_id} port={port}");

                // Connect through the device manager
                match device_manager.connect(device_id, port, packet.tag).await {
                    Ok(sport) => {
                        info!("connection established: device_id={device_id} sport={sport}");

                        // Send success response to client
                        let resp = protocol::make_result_response(packet.tag, 0);
                        if let Err(e) = resp.write_to(&mut stream).await {
                            warn!("failed to write connect response: {e}");
                            return;
                        }

                        // Enter bidirectional proxy loop
                        if let Err(e) = proxy_connection(
                            &mut stream,
                            &device_manager.conn_mgr,
                            device_id,
                            sport,
                        ).await {
                            debug!("proxy ended: {e}");
                        }
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
                let mut resp_plist = plist::Dictionary::new();
                resp_plist.insert("BUID".into(), plist::Value::String("meridian-relay-buid".into()));
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

                // Try to read the pairing record from the system usbmuxd socket
                match read_system_pair_record(&pair_record_id).await {
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
                debug!("SavePairRecord (stub)");
                let resp = protocol::make_result_response(packet.tag, 0);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write response: {e}");
                    return;
                }
            }

            "DeletePairRecord" => {
                debug!("DeletePairRecord (stub)");
                let resp = protocol::make_result_response(packet.tag, 0);
                if let Err(e) = resp.write_to(&mut stream).await {
                    warn!("failed to write response: {e}");
                    return;
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

async fn proxy_connection(
    stream: &mut UnixStream,
    conn_mgr: &ConnectionManager,
    device_id: u32,
    sport: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client_buf = [0u8; 65536];

    loop {
        // Check for data from device (in ib_buf) first
        let data_to_client = {
            let devices = conn_mgr.devices.read().await;
            if let Some(device) = devices.get(&device_id) {
                if let Some(conn) = device.connections.get(&sport) {
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
            stream.write_all(&data).await?;
            debug!("proxy: wrote {} bytes to client", data.len());
            // Clear the buffer
            let mut devices = conn_mgr.devices.write().await;
            if let Some(device) = devices.get_mut(&device_id) {
                if let Some(conn) = device.connections.get_mut(&sport) {
                    conn.ib_buf.clear();
                }
            }
            continue;
        }

        // Try to read from client (with timeout)
        match tokio::time::timeout(
            std::time::Duration::from_millis(10),
            stream.read(&mut client_buf),
        ).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(n)) => {
                let data = client_buf[..n].to_vec();

                // Send data to device through the connection manager
                let devices = conn_mgr.devices.read().await;
                if let Some(device) = devices.get(&device_id) {
                    if let Some(conn) = device.connections.get(&sport) {
                        let tcp_seq = conn.tx_seq;
                        let tcp_ack = conn.tx_ack;
                        let version = device.version;

                        let _pkt = mux::build_tcp_packet(
                            version,
                            device.tx_seq,
                            device.rx_seq,
                            sport,
                            conn.dport,
                            tcp_seq,
                            tcp_ack,
                            mux::TCP_ACK,
                            (mux::INITIAL_WINDOW >> 8) as u16,
                            Some(&data),
                        );

                        debug!("proxy: sending {} bytes to device {} sport={}", data.len(), device_id, sport);
                        // Note: actual USB send is handled by the device task
                    }
                }
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                // Timeout, continue loop
            }
        }

        // Small yield to prevent busy loop
        tokio::task::yield_now().await;
    }
}

async fn read_system_pair_record(udid: &str) -> Result<Vec<u8>, std::io::Error> {
    let socket_path = std::path::Path::new("/var/run/usbmuxd");
    if !socket_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "system usbmuxd not available",
        ));
    }

    let mut stream = tokio::net::UnixStream::connect(socket_path).await?;

    let mut req = plist::Dictionary::new();
    req.insert("MessageType".into(), "ReadPairRecord".into());
    req.insert("PairRecordID".into(), udid.into());

    let packet = RawPacket::new(req, protocol::XML_PLIST_VERSION, protocol::PLIST_MESSAGE_TYPE, 0);
    packet.write_to(&mut stream).await?;

    let response = protocol::read_packet(&mut stream).await?;
    match response.plist.get("PairRecordData") {
        Some(plist::Value::Data(d)) => Ok(d.clone()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no PairRecordData in response",
        )),
    }
}
