//! Unix (Linux/macOS) platform implementation.

use std::io;
use std::path::{Path, PathBuf};
use tokio::fs as tokio_fs;

use super::PeerIdentity;
use crate::security::PeerCredentials;

pub const OS_NAME: &str = "unix";

/// Default client endpoint (unix socket path).
pub fn default_endpoint() -> String {
    "/var/run/usbmuxd".into()
}

pub fn default_lockdown_dir() -> PathBuf {
    PathBuf::from("/var/lib/lockdown")
}

pub fn default_config_path() -> PathBuf {
    PathBuf::from("/etc/meridian-relay.toml")
}

pub fn default_log_dir() -> PathBuf {
    PathBuf::from("/var/log")
}

/// Extract peer credentials from a connected unix socket.
pub fn peer_identity_of_unix(stream: &tokio::net::UnixStream) -> PeerIdentity {
    PeerIdentity {
        credentials: PeerCredentials::from_unix_stream(stream),
        sid: None,
    }
}

/// Caller decided the policy on TCP / anonymous transports.
pub fn anonymous_peer() -> PeerIdentity {
    PeerIdentity::anonymous()
}

/// Pipe SDDL is a windows concept; unix endpoints use file modes instead.
pub fn default_pipe_security() -> String {
    String::new()
}

/// Apply permission bits (and optional group ownership) to a socket file.
pub fn apply_endpoint_security(path: &Path, mode: u32, gid: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;

    if let Some(gid) = gid {
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // (uid_t)-1 = keep the current owner.
        let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Securely write a secret file (pair record, BUID): symlink-proof, mode 0600.
pub async fn secure_write_secret(path: &Path, data: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio_fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    Ok(())
}

/// Read from the kernel CSPRNG.
pub async fn random_u64() -> u64 {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = tokio_fs::File::open("/dev/urandom").await {
        use tokio::io::AsyncReadExt;
        if f.read_exact(&mut buf).await.is_ok() {
            return u64::from_ne_bytes(buf);
        }
    }
    // Fallback: /dev/urandom must never fail on unix, but if it does, warn loudly.
    tracing::error!("/dev/urandom unavailable — BUID entropy degraded");
    rand::random::<u64>()
}

/// systemd READY=1 notification (no-op unless launched with NOTIFY_SOCKET).
pub fn notify_service_ready() {
    let Ok(notify_socket) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let addr = match notify_socket.strip_prefix('@') {
        Some(name) => format!("\0{name}"), // abstract namespace
        None => notify_socket,
    };
    if let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() {
        if let Err(e) = sock.send_to(b"READY=1", addr) {
            tracing::warn!("failed to send systemd READY=1: {e}");
        }
    }
}

/// Waits for SIGINT or SIGTERM, whichever comes first.
pub async fn wait_for_shutdown_signal() {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to install SIGTERM handler: {e}");
            // Fall back to ctrl-c only.
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_secure_write_secret_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");
        secure_write_secret(&p, b"data").await.unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"data");
    }

    #[tokio::test]
    async fn test_secure_write_secret_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        std::fs::write(&target, b"original").unwrap();
        let link = dir.path().join("secret");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(secure_write_secret(&link, b"overwrite").await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }

    #[tokio::test]
    async fn test_random_u64_varies() {
        let a = random_u64().await;
        let b = random_u64().await;
        assert_ne!(a, b, "CSPRNG must not repeat");
    }
}
