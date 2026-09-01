use tracing::warn;

/// Maximum allowed length for a UDID string.
const MAX_UDID_LEN: usize = 64;

/// Characters allowed in a UDID (hex digits + dash separator).
const UDID_CHARS: &[u8] = b"0123456789abcdefABCDEF-";

/// Validate a UDID string — must be non-empty, printable, no path separators or dangerous chars.
pub fn validate_udid(udid: &str) -> Result<(), &'static str> {
    let trimmed = udid.trim().trim_end_matches('\0');
    if trimmed.is_empty() {
        return Err("UDID is empty");
    }
    if trimmed.len() > MAX_UDID_LEN {
        return Err("UDID too long");
    }
    for b in trimmed.bytes() {
        if !UDID_CHARS.contains(&b) {
            return Err("UDID contains invalid characters");
        }
    }
    // Reject path traversal patterns
    if trimmed.contains("..") || trimmed.contains('/') || trimmed.contains('\\') {
        return Err("UDID contains path traversal characters");
    }
    Ok(())
}

/// Sanitize a UDID for safe use in filesystem paths.
/// Returns None if the UDID is invalid.
pub fn sanitize_udid_for_path(udid: &str) -> Option<String> {
    let trimmed = udid.trim().trim_end_matches('\0');
    if validate_udid(trimmed).is_err() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Resolve a group name to a GID by parsing /etc/group.
pub fn resolve_group_gid(group_name: &str) -> Option<u32> {
    let content = std::fs::read_to_string("/etc/group").ok()?;
    for line in content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == group_name {
            if let Ok(gid) = parts[2].parse::<u32>() {
                return Some(gid);
            }
        }
    }
    warn!("could not resolve group '{}' to GID", group_name);
    None
}

/// Peer credentials from a Unix socket connection.
#[derive(Debug, Clone)]
pub struct PeerCredentials {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<u32>,
}

impl PeerCredentials {
    /// Extract peer credentials from a tokio UnixStream.
    pub fn from_unix_stream(stream: &tokio::net::UnixStream) -> Option<Self> {
        // tokio::net::UnixStream doesn't have peer_cred() on all platforms.
        // Use std library for Linux.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = stream.as_raw_fd();
            let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ret = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    &mut cred as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            };
            if ret == 0 {
                return Some(PeerCredentials {
                    uid: cred.uid,
                    gid: cred.gid,
                    pid: if cred.pid > 0 { Some(cred.pid as u32) } else { None },
                });
            }
        }
        None
    }

    /// Check if this peer UID is in the allowed list.
    /// Empty allowlist means all UIDs are permitted.
    pub fn is_allowed(&self, allowed_uids: &[u32]) -> bool {
        if allowed_uids.is_empty() {
            return true;
        }
        allowed_uids.contains(&self.uid)
    }
}

/// RAII guard that ensures a Unix socket file is cleaned up on drop.
pub struct SocketCleanupGuard {
    path: std::path::PathBuf,
    remove_on_drop: bool,
}

impl SocketCleanupGuard {
    pub fn new(path: std::path::PathBuf, remove_on_drop: bool) -> Self {
        Self { path, remove_on_drop }
    }

    pub fn disarm(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        if self.remove_on_drop && self.path.exists() {
            if let Err(e) = std::fs::remove_file(&self.path) {
                warn!("failed to clean up socket {}: {e}", self.path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_udid_valid() {
        assert!(validate_udid("00008110-000C694914F3801E").is_ok());
        assert!(validate_udid("00008110000C694914F3801E").is_ok());
        assert!(validate_udid("ABCDEF1234567890").is_ok());
    }

    #[test]
    fn test_validate_udid_invalid() {
        assert!(validate_udid("").is_err());
        assert!(validate_udid("../etc/passwd").is_err());
        assert!(validate_udid("abc/def").is_err());
        assert!(validate_udid("abc\\def").is_err());
        assert!(validate_udid("abc..def").is_err());
        assert!(validate_udid(&"a".repeat(100)).is_err());
        assert!(validate_udid("abc def").is_err()); // space
    }

    #[test]
    fn test_validate_udid_trimmed() {
        assert!(validate_udid("  00008110-000C694914F3801E  ").is_ok());
    }

    #[test]
    fn test_sanitize_udid_for_path() {
        assert_eq!(
            sanitize_udid_for_path("00008110-000C694914F3801E").unwrap(),
            "00008110-000C694914F3801E"
        );
        assert!(sanitize_udid_for_path("../../../etc/passwd").is_none());
        assert!(sanitize_udid_for_path("").is_none());
    }

    #[test]
    fn test_peer_credentials_allowed_empty() {
        let cred = PeerCredentials { uid: 1000, gid: 1000, pid: Some(1234) };
        assert!(cred.is_allowed(&[])); // empty = allow all
    }

    #[test]
    fn test_peer_credentials_allowed_match() {
        let cred = PeerCredentials { uid: 1000, gid: 1000, pid: Some(1234) };
        assert!(cred.is_allowed(&[0, 1000, 65534]));
        assert!(!cred.is_allowed(&[0, 65534]));
    }

    #[test]
    fn test_socket_cleanup_guard() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        std::fs::write(&sock, b"").unwrap();
        assert!(sock.exists());
        {
            let _g = SocketCleanupGuard::new(sock.clone(), true);
        }
        assert!(!sock.exists());
    }

    #[test]
    fn test_socket_cleanup_guard_disarmed() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        std::fs::write(&sock, b"").unwrap();
        {
            let mut g = SocketCleanupGuard::new(sock.clone(), true);
            g.disarm();
        }
        assert!(sock.exists()); // not removed
    }

    #[test]
    fn test_parse_octal_via_config() {
        assert_eq!(crate::config::parse_octal("0660").unwrap(), 0o660);
        assert_eq!(crate::config::parse_octal("0o600").unwrap(), 0o600);
        assert_eq!(crate::config::parse_octal("600").unwrap(), 0o600);
    }
}
