//! End-to-end tests: spawn the real daemon on a temp socket and speak the
//! usbmuxd client protocol to it. No USB hardware required.

use std::sync::Arc;
use std::time::Duration;

use meridian_relay::config::DaemonConfig;
use meridian_relay::daemon::{self, protocol};
use meridian_relay::daemon::protocol::RawPacket;
use meridian_relay::metrics::Metrics;

const TEST_UDID: &str = "00008110-000C694914F3801E";
const TIMEOUT: Duration = Duration::from_secs(5);

struct TestDaemon {
    socket_path: std::path::PathBuf,
    lockdown_dir: std::path::PathBuf,
    handle: tokio::task::JoinHandle<()>,
    // Keep the tempdir alive for the duration of the test.
    _dir: tempfile::TempDir,
}

impl TestDaemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("test-daemon.sock");
        let lockdown_dir = dir.path().join("lockdown");

        let mut config = DaemonConfig::default();
        config.socket_path = socket_path.clone();
        config.lockdown_dir = lockdown_dir.clone();

        let metrics = Arc::new(Metrics::new());
        let handle = tokio::spawn(async move {
            let _ = daemon::run_daemon(config, metrics).await;
        });

        // Wait until the socket is accepting connections.
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "daemon socket never appeared");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        Self { socket_path, lockdown_dir, handle, _dir: dir }
    }

    async fn rpc(&self, plist: plist::Dictionary, tag: u32) -> plist::Dictionary {
        let stream = tokio::net::UnixStream::connect(&self.socket_path).await.expect("connect");
        let mut stream = stream;
        let pkt = RawPacket::new(plist, protocol::XML_PLIST_VERSION, protocol::PLIST_MESSAGE_TYPE, tag);
        protocol::write_packet(&mut stream, &pkt).await.expect("write");
        let resp = tokio::time::timeout(TIMEOUT, protocol::read_packet(&mut stream, 16 * 1024 * 1024))
            .await
            .expect("read timeout")
            .expect("read");
        assert_eq!(resp.tag, tag, "response tag must echo request tag");
        resp.plist
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn msg(message_type: &str) -> plist::Dictionary {
    let mut d = plist::Dictionary::new();
    d.insert("MessageType".into(), plist::Value::String(message_type.into()));
    d
}

#[tokio::test]
async fn list_devices_returns_valid_shape() {
    let d = TestDaemon::start().await;
    let resp = d.rpc(msg("ListDevices"), 1).await;
    let list = resp.get("DeviceList").and_then(|v| v.as_array());
    assert!(list.is_some(), "DeviceList must be present and an array: {resp:?}");
}

#[tokio::test]
async fn stats_command_returns_json() {
    let d = TestDaemon::start().await;
    let resp = d.rpc(msg("MeridianStats"), 1).await;
    let stats_str = resp.get("Stats").and_then(|v| v.as_string()).expect("Stats field");
    let json: serde_json::Value = serde_json::from_str(stats_str).expect("Stats must be JSON");
    assert!(json.get("uptime_secs").is_some());
    assert!(json.get("clients_accepted").is_some());
}

#[tokio::test]
async fn unknown_command_rejected_with_bad_command() {
    let d = TestDaemon::start().await;
    let resp = d.rpc(msg("TotallyNotARealCommand"), 1).await;
    assert_eq!(
        resp.get("Number").and_then(|v| v.as_unsigned_integer()),
        Some(protocol::result::BAD_COMMAND as u64)
    );
}

#[tokio::test]
async fn missing_message_type_rejected() {
    let d = TestDaemon::start().await;
    let resp = d.rpc(plist::Dictionary::new(), 1).await;
    assert_eq!(
        resp.get("Number").and_then(|v| v.as_unsigned_integer()),
        Some(protocol::result::BAD_COMMAND as u64)
    );
}

#[tokio::test]
async fn connect_to_nonexistent_device_is_bad_device() {
    let d = TestDaemon::start().await;
    let mut p = msg("Connect");
    p.insert("DeviceID".into(), plist::Value::Integer(99.into()));
    p.insert("PortNumber".into(), plist::Value::Integer(62078u16.to_be() .into()));
    let resp = d.rpc(p, 1).await;
    assert_eq!(
        resp.get("Number").and_then(|v| v.as_unsigned_integer()),
        Some(protocol::result::BAD_DEVICE as u64)
    );
}

#[tokio::test]
async fn pair_record_crud_roundtrip() {
    let d = TestDaemon::start().await;
    let data = b"\x00\x01test-pair-record-payload".to_vec();

    // Save.
    let mut p = msg("SavePairRecord");
    p.insert("PairRecordID".into(), plist::Value::String(TEST_UDID.into()));
    p.insert("PairRecordData".into(), plist::Value::Data(data.clone()));
    let resp = d.rpc(p, 1).await;
    assert_eq!(resp.get("Number").and_then(|v| v.as_unsigned_integer()), Some(0), "save failed: {resp:?}");

    // File exists on disk with 0600 perms.
    let file = d.lockdown_dir.join(format!("{TEST_UDID}.plist"));
    assert!(file.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // Read back.
    let mut p = msg("ReadPairRecord");
    p.insert("PairRecordID".into(), plist::Value::String(TEST_UDID.into()));
    let resp = d.rpc(p, 1).await;
    let got = resp.get("PairRecordData").and_then(|v| v.as_data());
    assert_eq!(got, Some(data.as_slice()), "roundtrip data mismatch");

    // Delete.
    let mut p = msg("DeletePairRecord");
    p.insert("PairRecordID".into(), plist::Value::String(TEST_UDID.into()));
    let resp = d.rpc(p, 1).await;
    assert_eq!(resp.get("Number").and_then(|v| v.as_unsigned_integer()), Some(0));
    assert!(!file.exists(), "pair record file must be deleted");

    // Read after delete fails.
    let mut p = msg("ReadPairRecord");
    p.insert("PairRecordID".into(), plist::Value::String(TEST_UDID.into()));
    let resp = d.rpc(p, 1).await;
    assert_ne!(resp.get("Number").and_then(|v| v.as_unsigned_integer()), Some(0));
}

#[tokio::test]
async fn pair_record_path_traversal_rejected() {
    let d = TestDaemon::start().await;
    let evil = "../../../../etc/evil";

    let mut p = msg("SavePairRecord");
    p.insert("PairRecordID".into(), plist::Value::String(evil.into()));
    p.insert("PairRecordData".into(), plist::Value::Data(b"pwned".to_vec()));
    let resp = d.rpc(p, 1).await;
    assert_eq!(
        resp.get("Number").and_then(|v| v.as_unsigned_integer()),
        Some(protocol::result::BAD_COMMAND as u64)
    );

    // Nothing may have been written outside (or inside) the lockdown dir.
    let entries: Vec<_> = std::fs::read_dir(&d.lockdown_dir)
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(entries.is_empty(), "no files must be created for invalid UDIDs");
}

#[tokio::test]
async fn read_buid_is_stable_and_protected() {
    let d = TestDaemon::start().await;
    let resp1 = d.rpc(msg("ReadBUID"), 1).await;
    let buid1 = resp1.get("BUID").and_then(|v| v.as_string()).expect("BUID").to_string();
    assert!(!buid1.is_empty());

    let resp2 = d.rpc(msg("ReadBUID"), 2).await;
    let buid2 = resp2.get("BUID").and_then(|v| v.as_string()).unwrap();
    assert_eq!(buid1, buid2, "BUID must be stable across requests");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let buid_file = d.lockdown_dir.join("buid");
        assert!(buid_file.exists());
        assert_eq!(std::fs::metadata(buid_file).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[tokio::test]
async fn oversized_packet_rejected() {
    let d = TestDaemon::start().await;
    let stream = tokio::net::UnixStream::connect(&d.socket_path).await.expect("connect");
    let mut stream = stream;

    // Header claims a body far beyond the daemon's max_packet_bytes.
    use tokio::io::AsyncWriteExt;
    let size: u32 = 16 + 256 * 1024 * 1024;
    let mut header = Vec::new();
    header.extend_from_slice(&size.to_le_bytes());
    header.extend_from_slice(&1u32.to_le_bytes());
    header.extend_from_slice(&8u32.to_le_bytes());
    header.extend_from_slice(&1u32.to_le_bytes());
    stream.write_all(&header).await.unwrap();

    // The daemon must drop the connection (read fails or EOFs).
    let mut buf = [0u8; 16];
    let res = tokio::time::timeout(TIMEOUT, tokio::io::AsyncReadExt::read(&mut stream, &mut buf)).await
        .expect("timeout");
    match res {
        Ok(0) => {} // clean EOF — connection closed
        Ok(_) => panic!("daemon accepted an oversized packet"),
        Err(_) => {} // also acceptable
    }
}

#[tokio::test]
async fn concurrent_clients_served() {
    let d = TestDaemon::start().await;
    let mut handles = Vec::new();
    for i in 0..10 {
        let socket_path = d.socket_path.clone();
        handles.push(tokio::spawn(async move {
            let stream = tokio::net::UnixStream::connect(&socket_path).await.expect("connect");
            let mut stream = stream;
            let pkt = RawPacket::new(msg("ListDevices"), 1, 8, i);
            protocol::write_packet(&mut stream, &pkt).await.expect("write");
            let resp = tokio::time::timeout(TIMEOUT, protocol::read_packet(&mut stream, 16 * 1024 * 1024))
                .await.expect("timeout").expect("read");
            assert!(resp.plist.get("DeviceList").is_some());
        }));
    }
    for h in handles {
        h.await.expect("client task panicked");
    }
}
