//! Direct lockdown client over the mux channel — no external tools.
//!
//! Full flow:
//!   1. `Connect` to lockdown (port 62078) via the daemon endpoint.
//!   2. Probe `GetValue` — devices with relaxed pair requirements may answer
//!      directly (older iOS lets basic keys through unpaired).
//!   3. If refused: fetch the pair record from the endpoint, do
//!      `StartSession`, upgrade to TLS with the pair-record client cert when
//!      `EnableSessionSSL` is set, then `GetValue` for the keys.
//!
//! Works identically against the USB backend and the relay backend.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use crate::daemon::protocol::{self, RawPacket, PLIST_MESSAGE_TYPE, XML_PLIST_VERSION};
use crate::daemon::transport::{Endpoint, TransportStream};
use crate::daemon::connection::LOCKDOWN_PORT;

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

type LockdownResult<T> = Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Channel: whichever of plain or TLS the device settled into.
// ---------------------------------------------------------------------------
enum Channel {
    Plain(TransportStream),
    Tls(Box<tokio_rustls::client::TlsStream<TransportStream>>),
}

impl Channel {
    async fn write_endpoint(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Channel::Plain(s) => s.write_all(bytes).await,
            Channel::Tls(s) => s.write_all(bytes).await,
        }
    }
    async fn read_into(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Channel::Plain(s) => s.read(buf).await,
            Channel::Tls(s) => s.read(buf).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Core client
// ---------------------------------------------------------------------------
async fn connect_lockdown(endpoint: &Endpoint, device_id: u32) -> LockdownResult<TransportStream> {
    let mut stream = endpoint.connect().await?;
    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), plist::Value::String("Connect".into()));
    p.insert("DeviceID".into(), plist::Value::Integer((device_id as u64).into()));
    p.insert("PortNumber".into(), plist::Value::Integer((LOCKDOWN_PORT.to_be() as u64).into()));
    let pkt = RawPacket::new(p, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 1);
    protocol::write_packet(&mut stream, &pkt).await?;

    let resp = protocol::read_packet(&mut stream, 64 * 1024).await?;
    let number = resp.plist.get("Number").and_then(|v| v.as_unsigned_integer()).unwrap_or(1);
    if number != 0 {
        return Err(format!("Connect to lockdown failed: result code {number}").into());
    }
    Ok(stream)
}

async fn mux_read_pair_record(endpoint: &Endpoint, udid: &str) -> LockdownResult<plist::Dictionary> {
    let mut stream = endpoint.connect().await?;
    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), plist::Value::String("ReadPairRecord".into()));
    p.insert("PairRecordID".into(), plist::Value::String(udid.into()));
    let pkt = RawPacket::new(p, XML_PLIST_VERSION, PLIST_MESSAGE_TYPE, 2);
    protocol::write_packet(&mut stream, &pkt).await?;

    let resp = protocol::read_packet(&mut stream, 16 * 1024 * 1024).await?;
    // Errors: a numeric `Number` header without data means failure.
    let data = resp
        .plist
        .get("PairRecordData")
        .and_then(|v| v.as_data())
        .ok_or("pair record absent or unreadable")?;
    let rec = plist::from_bytes::<plist::Value>(data)?;
    rec.as_dictionary()
        .cloned()
        .ok_or_else(|| "pair record payload is not a dictionary".into())
}

async fn tls_upgrade(
    stream: TransportStream,
    pair_record: &plist::Dictionary,
) -> LockdownResult<Channel> {
    let host_cert_pem = pair_record
        .get("HostCertificate")
        .and_then(|v| v.as_data())
        .ok_or("pair record has no HostCertificate")?;
    let host_key_pem = pair_record
        .get("HostPrivateKey")
        .and_then(|v| v.as_data())
        .ok_or("pair record has no HostPrivateKey")?;

    let cert_chain = rustls_pemfile::certs(&mut std::io::Cursor::new(host_cert_pem))
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(host_key_pem))?
        .ok_or("failed to parse host private key from pair record")?;

    // The device cert is self-signed; verification is skipped entirely.
    // (Custom verifier — skip cert chain validation.)
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _m: &[u8], _c: &rustls::pki_types::CertificateDer<'_>, _d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _m: &[u8], _c: &rustls::pki_types::CertificateDer<'_>, _d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA384,
                rustls::SignatureScheme::RSA_PKCS1_SHA512,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
                rustls::SignatureScheme::ED25519,
            ]
        }
    }

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_client_auth_cert(cert_chain, key)?;

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    // iOS lockdown doesn't validate SNI hostnames; any name is fine.
    let server_name = rustls::pki_types::ServerName::try_from("device.internal")?;
    let tls = connector.connect(server_name, stream).await?;
    Ok(Channel::Tls(Box::new(tls)))
}

async fn write_plist(chan: &mut Channel, dict: &plist::Dictionary) -> LockdownResult<()> {
    let mut buf = Vec::new();
    plist::Value::Dictionary(dict.clone()).to_writer_xml(&mut buf)?;
    chan.write_endpoint(&buf).await?;
    Ok(())
}

async fn read_plist(chan: &mut Channel) -> LockdownResult<plist::Dictionary> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 16384];
    let hard_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS);

    loop {
        if buf.len() > MAX_LOCKDOWN_RESPONSE {
            return Err("lockdown response exceeded cap".into());
        }
        // A parse attempt each iteration keeps framing implicit.
        if let Ok(v) = plist::from_bytes::<plist::Value>(&buf) {
            if let Some(d) = v.as_dictionary() {
                return Ok(d.clone());
            }
        }

        let idle = tokio::time::sleep(std::time::Duration::from_millis(IDLE_END_MS));
        tokio::pin!(idle);

        tokio::select! {
            n = tokio::time::timeout_at(hard_deadline, chan.read_into(&mut chunk)) => {
                match n {
                    Ok(Ok(0)) => {
                        if buf.is_empty() {
                            return Err("lockdown closed connection".into());
                        }
                        break;
                    }
                    Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                    Ok(Err(e)) => return Err(Box::new(e)),
                    Err(_) => break, // deadline: deliver whatever we have
                }
            }
            _ = idle => { break; }
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

async fn get_value_raw(chan: &mut Channel, key: &str) -> LockdownResult<Option<String>> {
    let mut req = plist::Dictionary::new();
    req.insert("Request".into(), plist::Value::String("GetValue".into()));
    req.insert("Key".into(), plist::Value::String(key.into()));
    req.insert("Label".into(), plist::Value::String("meridian-relay".into()));

    write_plist(chan, &req).await?;
    let resp = read_plist(chan).await?;

    if let Some(err) = resp.get("Error").and_then(|v| v.as_string()) {
        debug!("GetValue({key}) error: {err}");
        return Ok(None);
    }
    match resp.get("Value") {
        Some(v) => Ok(Some(match v {
            plist::Value::String(s) => s.clone(),
            other => format!("{other:?}"),
        })),
        None => Ok(None),
    }
}

async fn start_session(chan: &mut Channel, pair_record: &plist::Dictionary) -> LockdownResult<bool> {
    let host_id = pair_record
        .get("HostID")
        .and_then(|v| v.as_string())
        .ok_or("pair record missing HostID")?;
    let system_buid = pair_record
        .get("SystemBUID")
        .and_then(|v| v.as_string())
        .ok_or("pair record missing SystemBUID")?;

    let mut req = plist::Dictionary::new();
    req.insert("Label".into(), plist::Value::String("meridian-relay".into()));
    req.insert("Request".into(), plist::Value::String("StartSession".into()));
    req.insert("ProtocolVersion".into(), plist::Value::String("2".into()));
    req.insert("HostID".into(), plist::Value::String(host_id.to_string()));
    req.insert("SystemBUID".into(), plist::Value::String(system_buid.to_string()));

    write_plist(chan, &req).await?;
    let resp = read_plist(chan).await?;

    if let Some(err) = resp.get("Error").and_then(|v| v.as_string()) {
        return Err(format!("StartSession failed: {err}").into());
    }
    let enable_ssl = resp.get("EnableSessionSSL").and_then(|v| v.as_boolean()).unwrap_or(false);
    Ok(enable_ssl)
}

/// Query lockdown "GetValue" for all requested keys, with full pairing
/// fallback path. Returns one entry per successfully-read key and a descriptive
/// error otherwise.
pub async fn get_value(
    endpoint: &Endpoint,
    device_id: u32,
    keys: &[&str],
    udid: &str,
) -> LockdownResult<Vec<(String, String)>> {
    let stream = connect_lockdown(endpoint, device_id).await?;
    let mut chan = Channel::Plain(stream);

    // Try one probe first — maybe the device answers without a session.
    let first = keys.first().copied().unwrap_or("DeviceName");
    let mut out = Vec::new();
    match get_value_raw(&mut chan, first).await {
        Ok(Some(v)) => {
            out.push((first.to_string(), v));
            for key in &keys[1..] {
                if let Ok(Some(v)) = get_value_raw(&mut chan, key).await {
                    out.push(((*key).to_string(), v));
                }
            }
            return Ok(out);
        }
        _ => {}
    }

    // Session path: fetch pair record, start session, possibly TLS, retry.
    let pair_record = mux_read_pair_record(endpoint, udid).await?;

    // Start session over the current plain channel.
    let enable_ssl = start_session(&mut chan, &pair_record).await?;

    if enable_ssl {
        debug!("lockdown upgraded to TLS for {udid}");
        let Channel::Plain(s) = chan else {
            return Err("internal: expected plain channel at TLS upgrade".into());
        };
        chan = tls_upgrade(s, &pair_record).await?;
    }

    for key in keys {
        if let Ok(Some(v)) = get_value_raw(&mut chan, key).await {
            out.push(((*key).to_string(), v));
        }
    }

    if out.is_empty() {
        return Err("started session but lockdown returned no values (device may need to be unlocked/trusted)".into());
    }
    Ok(out)
}

/// Convenience: enrich the standard fields of a `Device` from lockdown.
/// Only fills fields that aren't already set.
pub async fn enrich_via_lockdown(device: &mut crate::device::Device, endpoint: &Endpoint) -> LockdownResult<Vec<(String, String)>> {
    let udid = device.udid.clone();
    let values = get_value(endpoint, device.device_id, &["DeviceName", "ProductType", "ProductVersion", "BuildVersion"], &udid).await?;

    let mut filled = Vec::new();
    for (k, v) in values.iter() {
        match k.as_str() {
            "DeviceName" if device.name.is_none() => {
                device.name = Some(v.clone());
                filled.push((k.clone(), v.clone()));
            }
            "ProductType" if device.model.is_none() => {
                let friendly = crate::device::info::model_name(v).unwrap_or(v);
                device.model = Some(friendly.to_string());
                filled.push((k.clone(), v.clone()));
            }
            "ProductVersion" if device.ios_version.is_none() => {
                device.ios_version = Some(v.clone());
                filled.push((k.clone(), v.clone()));
            }
            "BuildVersion" if device.build_version.is_none() => {
                device.build_version = Some(v.clone());
                filled.push((k.clone(), v.clone()));
            }
            _ => {}
        }
    }
    debug!("lockdown enrichment of {} filled {} fields", device.udid, filled.len());
    Ok(filled)
}
