//! Direct lockdown queries over the mux channel — no external tools.
//!
//! Connects to port 62078 through the daemon endpoint (works identically for
//! the USB and relay backends) and issues `GetValue` requests for the basic
//! device identity keys. These keys are answered by lockdown without a pair
//! session, so this works on unpaired devices too.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::daemon::protocol::{self, RawPacket, PLIST_MESSAGE_TYPE, XML_PLIST_VERSION};
use crate::daemon::transport::Endpoint;
use crate::daemon::connection::LOCKDOWN_PORT;

/// Maximum accumulation while waiting for a lockdown response.
const MAX_LOCKDOWN_RESPONSE: usize = 1024 * 1024;
const IDLE_END_MS: u64 = 300;
const REQUEST_TIMEOUT_SECS: u64 = 10;

pub const BASIC_KEYS: &[&str] = &[
    "DeviceName",
    "ProductType",
    "ProductVersion",
    "BuildVersion",
    "ModelNumber",
    "SerialNumber",
];

/// Query the given keys from lockdown on the given device.
/// Returns key → value map of what the device actually answered.
pub async fn get_value(
    endpoint: &Endpoint,
    device_id: u32,
    keys: &[&str],
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    debug!("lockdown query: device_id={device_id} keys={keys:?} via {}", endpoint.display_string());

    let mut stream = endpoint.connect().await?;

    // usbmuxd Connect: PortNumber is sent byte-swapped (historical quirk).
    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), plist::Value::String("Connect".into()));
    p.insert("DeviceID".into(), plist::Value::Integer((device_id as u64).into()));
    p.insert("PortNumber".into(), plist::Value::Integer((LOCKDOWN_PORT.to_be() as u64).into()));
    let pkt = RawPacket::new(p, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 1);
    protocol::write_packet(&mut stream, &pkt).await?;

    let resp = protocol::read_packet(&mut stream, 64 * 1024).await?;
    let number = resp.plist.get("Number").and_then(|v| v.as_unsigned_integer()).unwrap_or(1);
    if number != 0 {
        return Err(format!("lockdown connect failed: result code {number}").into());
    }

    debug!("lockdown channel open; sending GetValue requests");

    let mut out = Vec::new();
    for key in keys {
        let mut req = plist::Dictionary::new();
        req.insert("Request".into(), plist::Value::String("GetValue".into()));
        req.insert("Key".into(), plist::Value::String((*key).into()));
        req.insert("Label".into(), plist::Value::String("meridian-relay".into()));

        // lockdown speaks raw plists over the spliced channel; XML is fine.
        let mut buf = Vec::new();
        plist::Value::Dictionary(req).to_writer_xml(&mut buf)?;
        stream.write_all(&buf).await?;

        let body = read_plist_response(&mut stream).await?;
        if let Some(v) = body.get("Value") {
            let s = match v {
                plist::Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            };
            out.push(((*key).to_string(), s));
        }
    }

    Ok(out)
}

/// Read until we can parse a complete plist (XML sentinel "</plist>" or a
/// successful binary/XML parse), with an idle-window terminator and a hard cap.
async fn read_plist_response<S>(stream: &mut S) -> Result<plist::Dictionary, Box<dyn std::error::Error>>
where S: AsyncReadExt + Unpin {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 16384];
    let hard_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS);

    loop {
        if buf.len() > MAX_LOCKDOWN_RESPONSE {
            return Err("lockdown response exceeded cap".into());
        }
        // Try to parse what we have.
        if let Ok(v) = plist::from_bytes::<plist::Value>(&buf) {
            if let Some(d) = v.as_dictionary() {
                return Ok(d.clone());
            }
        }
        if buf.windows(8).any(|w| w == b"</plist>") {
            return Err("lockdown plist had closing sentinel but failed to parse".into());
        }

        let idle = tokio::time::sleep(std::time::Duration::from_millis(IDLE_END_MS));
        let read = tokio::time::timeout_at(hard_deadline, stream.read(&mut chunk));
        tokio::pin!(idle);

        tokio::select! {
            n = read => {
                match n {
                    Ok(Ok(0)) => {
                        if buf.is_empty() {
                            return Err("lockdown closed connection without responding".into());
                        }
                        break; // EOF with partial data — final parse below handles it
                    }
                    Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                    Ok(Err(e)) => return Err(Box::new(e)),
                    Err(_) => return Err("lockdown response timed out".into()),
                }
            }
            _ = idle => {
                break; // idle window elapsed with whatever we have
            }
        }
    }

    if buf.is_empty() {
        return Err("lockdown sent no data".into());
    }
    match plist::from_bytes::<plist::Value>(&buf) {
        Ok(v) => v.as_dictionary().cloned().ok_or_else(|| "lockdown response was not a dictionary".into()),
        Err(e) => Err(format!("lockdown plist parse failed: {e}").into()),
    }
}

/// Convenience: enrich the standard fields of a `Device` from lockdown.
pub async fn enrich_via_lockdown(device: &mut crate::device::Device, endpoint: &Endpoint) {
    let keys: &[&str] = &["DeviceName", "ProductType", "ProductVersion", "BuildVersion"];
    match get_value(endpoint, device.device_id, keys).await {
        Ok(values) => {
            for (k, v) in values {
                match k.as_str() {
                    "DeviceName" => if device.name.is_none() { device.name = Some(v) },
                    "ProductType" => if device.model.is_none() {
                        // Map hardware identifier to friendly name.
                        let friendly = crate::device::info::model_name(&v).unwrap_or(&v);
                        device.model = Some(friendly.to_string());
                    },
                    "ProductVersion" => if device.ios_version.is_none() { device.ios_version = Some(v) },
                    "BuildVersion" => if device.build_version.is_none() { device.build_version = Some(v) },
                    _ => {}
                }
            }
            debug!("lockdown enrichment of {} succeeded", device.udid);
        }
        Err(e) => {
            warn!("lockdown enrichment failed for {}: {e}", device.udid);
        }
    }
}
