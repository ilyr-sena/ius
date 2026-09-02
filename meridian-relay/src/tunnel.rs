//! TCP tunnel: listens on localhost ports and splices every connection to a
//! device port through the active daemon — replaces iproxy entirely.
//!
//! Usage: `meridian-relay tunnel 9100:9100 8100:8100`

use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::daemon::protocol::{self, RawPacket, PLIST_MESSAGE_TYPE, XML_PLIST_VERSION};
use crate::daemon::transport::Endpoint;
use crate::metrics::Metrics;

/// Splice a client socket through a meridian connect to a device port.
pub async fn run(
    endpoint: Endpoint,
    pairs: Vec<(u16, u16)>,
    device_id: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    if pairs.is_empty() {
        return Err("at least one local:device port pair is required".into());
    }

    let mut listeners = Vec::new();
    for (lp, dp) in &pairs {
        let l = TcpListener::bind(("127.0.0.1", *lp)).await?;
        info!("tunnel: 127.0.0.1:{lp} → device port {dp}");
        listeners.push((l, *dp));
    }

    let endpoint = Arc::new(endpoint);
    let calldown = std::future::pending::<()>(); // no explicit shutdown; ctrl-C via process
    tokio::pin!(calldown);

    let shared_metrics = Arc::new(Metrics::new());

    loop {
        tokio::select! {
            _ = &mut calldown => break,
            acc = accept_any(&listeners) => {
                let (stream, dport) = acc;
                let ep = endpoint.clone();
                let metrics = shared_metrics.clone();
                let dev = device_id;
                tokio::spawn(async move {
                    if let Err(e) = handle_one(stream, ep, dev, dport, metrics).await {
                        warn!("tunnel connection failed: {e}");
                    }
                });
            }
        }
    }
    Ok(())
}

async fn accept_any(listeners: &[(TcpListener, u16)]) -> (TcpStream, u16) {
    // Round-robin select over all listeners; futures::future::select_all
    // owns the futures so the borrow is clean.
    let futs = listeners.iter().map(|(l, dp)| {
        Box::pin(async move {
            let (s, _) = l.accept().await.expect("accept");
            (s, *dp)
        })
    });
    futures::future::select_all(futs).await.0
}

async fn handle_one(
    mut client: TcpStream,
    daemon: Arc<Endpoint>,
    device_id: Option<u32>,
    dport: u16,
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error>> {
    metrics.clients_accepted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let mut mux = daemon.connect().await?;

    let device_id = match device_id {
        Some(id) => id,
        None => resolve_first_device(&mut mux).await?,
    };

    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), plist::Value::String("Connect".into()));
    p.insert("DeviceID".into(), plist::Value::Integer((device_id as u64).into()));
    p.insert("ClientVersion".into(), plist::Value::Integer(7.into()));
    p.insert("ProgName".into(), plist::Value::String("meridian-relay".into()));
    p.insert("kLibUSBMuxVersion".into(), plist::Value::Integer(3.into()));
    // Port in network byte order as the usbmuxd contract expects.
    p.insert("PortNumber".into(), plist::Value::Integer(((dport.to_be()) as u64).into()));

    let pkt = RawPacket::new(p, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 1);
    protocol::write_packet(&mut mux, &pkt).await?;

    let resp = protocol::read_packet(&mut mux, 64 * 1024).await?;
    let num = resp.plist.get("Number").and_then(|v| v.as_unsigned_integer()).unwrap_or(1);
    if num != 0 {
        return Err(format!("device rejected Connect with code {num}").into());
    }

    debug!("tunnel established: :{} → device :{}", client.local_addr().map(|a| a.port()).unwrap_or(0), dport);
    metrics.connects_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let (c2d, d2c) = tokio::io::copy_bidirectional(&mut client, &mut mux).await?;
    metrics.client_rx_bytes.fetch_add(c2d, std::sync::atomic::Ordering::Relaxed);
    metrics.client_tx_bytes.fetch_add(d2c, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

async fn resolve_first_device(mux: &mut crate::daemon::transport::TransportStream)
    -> Result<u32, Box<dyn std::error::Error>> {
    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), plist::Value::String("ListDevices".into()));
    p.insert("ClientVersion".into(), plist::Value::Integer(1.into()));
    p.insert("ProgName".into(), plist::Value::String("meridian-relay".into()));
    p.insert("kLibUSBMuxVersion".into(), plist::Value::Integer(3.into()));
    let pkt = RawPacket::new(p, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 1);
    protocol::write_packet(mux, &pkt).await?;
    let resp = protocol::read_packet(mux, 16 * 1024 * 1024).await?;
    let list = resp.plist.get("DeviceList").and_then(|v| v.as_array())
        .ok_or("no DeviceList in upstream reply")?;
    let first = list.first().and_then(|v| v.as_dictionary())
        .and_then(|d| d.get("DeviceID")).and_then(|v| v.as_unsigned_integer())
        .ok_or("no devices connected")?;
    Ok(first as u32)
}

pub fn parse_pairs(pairs: &[String]) -> Result<Vec<(u16, u16)>, String> {
    pairs.iter().map(|s| {
        let (l, d) = s.split_once(':')
            .ok_or_else(|| format!("invalid pair '{s}' — expected local:device"))?;
        let lp: u16 = l.parse().map_err(|_| format!("invalid local port in '{s}'"))?;
        let dp: u16 = d.parse().map_err(|_| format!("invalid device port in '{s}'"))?;
        if lp == 0 || dp == 0 {
            return Err(format!("ports must be non-zero in '{s}'"));
        }
        Ok((lp, dp))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pairs_good() {
        let p = parse_pairs(&["9100:9100".into(), "8100:8100".into()]).unwrap();
        assert_eq!(p, vec![(9100, 9100), (8100, 8100)]);
    }

    #[test]
    fn parse_pairs_rejects_garbage() {
        assert!(parse_pairs(&["nope".into()]).is_err());
        assert!(parse_pairs(&["0:9100".into()]).is_err());
        assert!(parse_pairs(&["9100:0".into()]).is_err());
        assert!(parse_pairs(&["1:x".into()]).is_err());
    }
}
