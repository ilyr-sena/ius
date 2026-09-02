//! OS abstraction layer — keeps *all* platform divergence in one place.
//!
//! Everything the rest of the daemon needs from the OS is re-exported here:
//! default paths, secure file writes, endpoint permission application, peer
//! identification, service manager glue, and randomness. No other module
//! contains `#[cfg(windows)]` blocks.

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

use crate::security::PeerCredentials;

/// How a client proved its identity, in a form policy checks can consume.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    pub credentials: Option<PeerCredentials>,
    /// Windows: client SID string. Unix: always `None`.
    pub sid: Option<String>,
}

impl PeerIdentity {
    pub fn anonymous() -> Self {
        Self { credentials: None, sid: None }
    }

    /// Attach a PID to a partially-filled identity (used by Windows pipe peers).
    pub fn with_pid(mut self, pid: u32) -> Self {
        if pid != 0 {
            let pid_opt = Some(pid);
            match &mut self.credentials {
                Some(c) => c.pid = pid_opt,
                None => {
                    self.credentials = Some(PeerCredentials {
                        uid: 0,
                        gid: 0,
                        pid: pid_opt,
                    });
                }
            }
        }
        self
    }
}
