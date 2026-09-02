//! Windows platform implementation.
//!
//! Endpoint = named pipe (`\\.\pipe\...`). Pair records live in
//! `%ProgramData%\Apple\Lockdown` for interoperability with Apple tooling.
//! Secrets are protected with a DACL granting SYSTEM + Administrators only.

use std::io;
use std::path::{Path, PathBuf};
use tokio::fs as tokio_fs;

use super::PeerIdentity;

pub const OS_NAME: &str = "windows";

/// Well-known SIDs used in SDDL strings.
pub const SID_SYSTEM: &str = "SY";
pub const SID_ADMINISTRATORS: &str = "BA";
pub const SID_AUTHENTICATED_USERS: &str = "AU";

pub fn default_endpoint() -> String {
    "pipe:meridian-relay".into()
}

pub fn default_lockdown_dir() -> PathBuf {
    program_data().join("Apple").join("Lockdown")
}

pub fn default_config_path() -> PathBuf {
    program_data().join("Meridian").join("config.toml")
}

pub fn default_log_dir() -> PathBuf {
    program_data().join("Meridian").join("Logs")
}

pub fn program_data() -> PathBuf {
    std::env::var("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"))
}

pub fn anonymous_peer() -> PeerIdentity {
    PeerIdentity::anonymous()
}

/// Default SDDL for the daemon's named pipe: SYSTEM + Administrators full,
/// interactive users full (usbmuxd's default mode-666 equivalent).
pub const DEFAULT_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)";

/// SDDL for secret files (pair records, BUID): SYSTEM + Administrators only.
pub const SECRET_FILE_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";

/// Convert an SDDL string into a raw SECURITY_DESCRIPTOR (must be freed with
/// `LocalFree` by the caller).
///
/// # Safety
/// Wraps ConvertStringSecurityDescriptorToSecurityDescriptorW.
pub type RawSecurityDescriptor = std::ffi::c_void;

pub unsafe fn sddl_to_security_descriptor(
    sddl: &str,
) -> io::Result<(*mut RawSecurityDescriptor, usize)> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };

    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: *mut RawSecurityDescriptor = std::ptr::null_mut();
    let mut size: u32 = 0;
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((sd, size as usize))
}

/// Free a security descriptor returned by `sddl_to_security_descriptor`.
///
/// # Safety
/// `sd` must have come from `sddl_to_security_descriptor`.
pub unsafe fn free_security_descriptor(sd: *mut RawSecurityDescriptor) {
    unsafe { windows_sys::Win32::Foundation::LocalFree(sd as _) };
}

/// Apply a DACL (expressed as SDDL) to a path. Used for secret files.
pub fn apply_sddl_to_path(path: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION};

    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let (sd, _size) = unsafe { sddl_to_security_descriptor(sddl) }?;
    let result = unsafe {
        // Get the DACL out of the descriptor.
        let mut present: i32 = 0;
        let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
        let mut defaulted: i32 = 0;
        let got = windows_sys::Win32::Security::GetSecurityDescriptorDacl(
            sd,
            &mut present,
            &mut dacl,
            &mut defaulted,
        );
        if got == 0 || present == 0 {
            windows_sys::Win32::Foundation::LocalFree(sd as _);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "SDDL has no DACL"));
        }
        SetNamedSecurityInfoW(
            wide.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    unsafe { free_security_descriptor(sd) };

    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

/// Securely write a secret file (pair record, BUID): SYSTEM+BA only ACL.
pub async fn secure_write_secret(path: &Path, data: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;

    // Refuse to write through a symlink/junction.
    if let Ok(meta) = tokio_fs::symlink_metadata(path).await {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to write secret through a symlink",
            ));
        }
    }

    let mut file = tokio_fs::File::create(path).await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    drop(file);

    apply_sddl_to_path(path, SECRET_FILE_SDDL)?;
    Ok(())
}

/// Randomness for BUID generation via rand (BCryptGenRandom underneath).
pub async fn random_u64() -> u64 {
    rand::random::<u64>()
}

/// Peer identity for a connected named-pipe client: PID + best-effort user SID.
pub fn peer_identity_of_pipe(
    server: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> PeerIdentity {
    use std::os::windows::io::AsRawHandle;

    let mut pid: u32 = 0;
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(
            server.as_raw_handle() as _,
            &mut pid,
        )
    };
    if ok == 0 {
        tracing::debug!("GetNamedPipeClientProcessId failed");
    }

    let sid = impersonate_and_read_sid(server);

    PeerIdentity {
        credentials: None,
        sid,
    }
    .with_pid(pid)
}

fn impersonate_and_read_sid(
    _server: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> Option<String> {
    // NOTE: SID resolution via ImpersonateNamedPipeClient requires the
    // SeImpersonatePrivilege dance and complicates state; the DACL on the pipe
    // is the real enforcement mechanism. We surface the PID (above) and leave
    // SID plumbing as Optional for a future hardening pass.
    None
}

/// Service-control manager notification is handled by the service module;
/// this stays a no-op for the plain process path.
pub fn notify_service_ready() {}

/// Default named-pipe DACL. SYSTEM + Administrators + interactive users;
/// tune with `--pipe-security` for locked-down deployments.
pub fn default_pipe_security() -> String {
    DEFAULT_PIPE_SDDL.to_string()
}

/// Waits for Ctrl+C or Ctrl+Break (console processes). The Windows service
/// path uses the SCM control channel instead (see `service` module).
pub async fn wait_for_shutdown_signal() {
    use tokio::signal::windows::{ctrl_break, ctrl_c};
    let mut brk = match ctrl_break() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("failed to install Ctrl+Break handler: {e}");
            if let Ok(mut c) = ctrl_c() {
                let _ = c.recv().await;
            }
            return;
        }
    };
    let mut cc = match ctrl_c() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to install Ctrl+C handler: {e}");
            let _ = brk.recv().await;
            return;
        }
    };
    tokio::select! {
        _ = cc.recv() => tracing::info!("received Ctrl+C, shutting down"),
        _ = brk.recv() => tracing::info!("received Ctrl+Break, shutting down"),
    }
}
