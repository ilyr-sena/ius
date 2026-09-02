//! Relay-mode integration tests: the daemon splices clients to an upstream
//! usbmuxd-compatible service (here: a fake in-process one). Cross-platform.

use std::sync::Arc;
use std::time::Duration;

use meridian_relay::config::{Backend, DaemonConfig};
use meridian_relay::daemon::{self, protocol};
use meridian_relay::daemon::protocol::RawPacket;
use meridian_relay::daemon::transport::Endpoint;
use meridian_relay::metrics::Metrics;

const TIMEOUT: Duration = Duration::from_secs(5);

fn test_endpoint(dir: &std::path::Path, name: &str) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::parse(&format!("unix:{}", dir.join(name).with_extension("sock").display())).unwrap()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Endpoint::parse(&format!("pipe:meridian-test-{name}-{}", std::process::id())).unwrap()
    }
}

/// A fake upstream usbmuxd: answers ListDevices with a canned list, and
/// echoes everything else frame-by-frame.
async fn spawn_fake_upstream() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            tokio::spawn(async move {
                loop {
                    let pkt = match protocol::read_packet(&mut stream, 16 * 1024 * 1024).await {
                        Ok(p) => p,
                        Err(_) => return, // client went away
                    };
                    let mt = pkt.plist.get("MessageType").and_then(|v| v.as_string()).unwrap_or("");
                    if mt == "ListDevices" {
                        let resp = protocol::make_device_list_response(pkt.tag, &[daemon::device_scanner::UsbDevice {
                            device_id: 1,
                            udid: "00008110-000C694914F3801E".into(),
                            product_id: 0x12A8,
                            usb_bus: 3,
                            usb_address: 12,
                        }]);
                        if protocol::write_packet(&mut stream, &resp).await.is_err() {
                            return;
                        }
                    } else if mt == "Connect" {
                        // Accept, then play a fake lockdown:
                        let resp = protocol::make_result_response(pkt.tag, protocol::result::OK);
                        if protocol::write_packet(&mut stream, &resp).await.is_err() {
                            return;
                        }
                        if let Err(_) = fake_lockdown_loop(&mut stream).await {
                            return;
                        }
                    } else {
                        // Echo any other frame back verbatim — proves the splice is transparent.
                        let mut echo = pkt.plist.clone();
                        echo.insert("Echoed".into(), plist::Value::Boolean(true));
                        let resp = RawPacket::new(echo, pkt.version, pkt.message, pkt.tag);
                        if protocol::write_packet(&mut stream, &resp).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });

    addr
}

async fn start_relay(upstream_addr: std::net::SocketAddr) -> (Endpoint, tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = test_endpoint(dir.path(), "relay");

    let mut config = DaemonConfig::default();
    config.endpoint = endpoint.clone();
    config.backend = Backend::Relay;
    config.upstream = Endpoint::parse(&format!("tcp:{upstream_addr}")).unwrap();

    let metrics = Arc::new(Metrics::new());
    let handle = tokio::spawn(async move {
        let _ = daemon::relay::run_relay(config, metrics, std::future::pending()).await;
    });

    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        if endpoint.connect().await.is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "relay endpoint never came up");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    (endpoint, dir, handle)
}

#[tokio::test]
async fn relay_list_devices_passthrough() {
    let upstream = spawn_fake_upstream().await;
    let (endpoint, _dir, relay) = start_relay(upstream).await;

    let mut stream = endpoint.connect().await.unwrap();
    let pkt = RawPacket::new(msg("ListDevices"), 1, 8, 42);
    protocol::write_packet(&mut stream, &pkt).await.unwrap();
    let resp = tokio::time::timeout(TIMEOUT, protocol::read_packet(&mut stream, 16 * 1024 * 1024))
        .await.expect("timeout").expect("read");

    let list = resp.plist.get("DeviceList").and_then(|v| v.as_array()).expect("DeviceList");
    assert_eq!(list.len(), 1, "must pass through the upstream's device");
    relay.abort();
}

#[tokio::test]
async fn relay_transparent_arbitrary_frames() {
    let upstream = spawn_fake_upstream().await;
    let (endpoint, _dir, relay) = start_relay(upstream).await;

    let mut stream = endpoint.connect().await.unwrap();
    let mut p = msg("SomeCustomThing");
    p.insert("Payload".into(), plist::Value::String("hello".into()));
    let pkt = RawPacket::new(p, 1, 8, 7);
    protocol::write_packet(&mut stream, &pkt).await.unwrap();

    let resp = tokio::time::timeout(TIMEOUT, protocol::read_packet(&mut stream, 16 * 1024 * 1024))
        .await.expect("timeout").expect("read");
    assert_eq!(resp.tag, 7);
    assert_eq!(
        resp.plist.get("Echoed").and_then(|v| v.as_boolean()),
        Some(true),
        "relay must pass arbitrary frames through transparently"
    );
    assert_eq!(resp.plist.get("Payload").and_then(|v| v.as_string()), Some("hello"));
    relay.abort();
}

#[tokio::test]
async fn relay_upstream_down_closes_client_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = test_endpoint(dir.path(), "relay-down");

    let mut config = DaemonConfig::default();
    config.endpoint = endpoint.clone();
    config.backend = Backend::Relay;
    // Point at a port nothing listens on.
    config.upstream = Endpoint::parse("tcp:127.0.0.1:59998").unwrap();

    let metrics = Arc::new(Metrics::new());
    let relay = tokio::spawn(async move {
        let _ = daemon::relay::run_relay(config, metrics, std::future::pending()).await;
    });

    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        if endpoint.connect().await.is_ok() { break; }
        assert!(std::time::Instant::now() < deadline, "relay endpoint never came up");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mut stream = endpoint.connect().await.unwrap();
    let pkt = RawPacket::new(msg("ListDevices"), 1, 8, 1);
    protocol::write_packet(&mut stream, &pkt).await.unwrap();

    // The relay should drop the connection promptly (not hang forever).
    let mut buf = [0u8; 4];
    let res = tokio::time::timeout(TIMEOUT, tokio::io::AsyncReadExt::read(&mut stream, &mut buf))
        .await
        .expect("relay should close connection when upstream is down");
    match res {
        Ok(0) | Err(_) => {} // clean EOF or error — both acceptable
        Ok(n) => panic!("unexpected data when upstream down: {n} bytes"),
    }
    relay.abort();
}

/// A tiny fake lockdown service: raw plist request/response on the channel.
async fn fake_lockdown_loop<S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin>(
    stream: &mut S,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle"))??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);

        // Try to parse a complete plist.
        if let Ok(plist::Value::Dictionary(d)) = plist::from_bytes::<plist::Value>(&buf) {
            let key = d.get("Key").and_then(|v| v.as_string()).unwrap_or("").to_string();
            let val = match key.as_str() {
                "DeviceName" => "Steve's iPhone 16 Pro",
                "ProductType" => "iPhone17,1",
                "ProductVersion" => "26.0.1",
                "BuildVersion" => "23A345",
                _ => "",
            };
            let mut resp = plist::Dictionary::new();
            resp.insert("Request".into(), plist::Value::String("GetValue".into()));
            resp.insert("Key".into(), plist::Value::String(key));
            resp.insert("Value".into(), plist::Value::String(val.into()));
            let mut out = Vec::new();
            plist::Value::Dictionary(resp).to_writer_xml(&mut out).unwrap();
            stream.write_all(&out).await?;
            buf.clear();
        }
        use tokio::io::AsyncWriteExt;
    }
}

#[tokio::test]
async fn relay_lockdown_enrichment_end_to_end() {
    let upstream = spawn_fake_upstream().await;
    let (endpoint, _dir, relay) = start_relay(upstream).await;

    // Reproduce the `info` flow: list, then lockdown-enrich over the relay.
    let devices = meridian_relay::device::detect::list_devices_from(&endpoint.display_string())
        .await
        .expect("list");
    assert_eq!(devices.len(), 1);

    let mut dev = devices[0].clone();
    meridian_relay::device::info::enrich_device_info(&mut dev, &endpoint).await;

    assert_eq!(dev.name.as_deref(), Some("Steve's iPhone 16 Pro"));
    assert_eq!(dev.model.as_deref(), Some("iPhone 16 Pro")); // mapped from iPhone17,1
    assert_eq!(dev.ios_version.as_deref(), Some("26.0.1"));
    assert_eq!(dev.build_version.as_deref(), Some("23A345"));
    relay.abort();
}

fn msg(mt: &str) -> plist::Dictionary {
    let mut d = plist::Dictionary::new();
    d.insert("MessageType".into(), plist::Value::String(mt.into()));
    d
}
