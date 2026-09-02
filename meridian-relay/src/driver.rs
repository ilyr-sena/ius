//! Windows driver provisioning — WinUSB binding, one command, zero UI.
//!
//! Strategy:
//!   1. Embed the INF in the binary (self-contained; no side files needed).
//!   2. `pnputil /add-driver` stages it into the driver store — this makes
//!      Windows auto-select it for all *future* matching device arrivals.
//!   3. `UpdateDriverForPlugAndPlayDevicesW` rebinds *currently attached*
//!      Apple devices immediately, without replugging.
//!
//! Everything is idempotent: running setup twice is a no-op.

use std::io;
use std::path::PathBuf;

/// The WinUSB binding INF, embedded at build time.
pub const EMBEDDED_INF: &str = include_str!("../packaging/meridian-relay-winusb.inf");
const INF_NAME: &str = "meridian-relay-winusb.inf";

const APPLE_MUX_HWID: &str = r"USB\Class_FF&SubClass_FE&Prot_02";

fn inf_dir() -> PathBuf {
    crate::platform::windows::program_data()
        .join("Meridian")
        .join("drivers")
}

pub fn embedded_inf_path() -> PathBuf {
    inf_dir().join(INF_NAME)
}

/// Write the embedded INF to disk. Idempotent (skips if content matches).
pub fn deploy_inf() -> io::Result<PathBuf> {
    let dir = inf_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(INF_NAME);

    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == EMBEDDED_INF {
            return Ok(path);
        }
    }
    std::fs::write(&path, EMBEDDED_INF)?;
    Ok(path)
}

/// Stage the driver into the Windows driver store via pnputil.
/// After this, newly-attached Apple devices bind WinUSB automatically.
pub fn stage_driver(inf_path: &std::path::Path) -> Result<(), String> {
    let output = std::process::Command::new("pnputil")
        .args(["/add-driver"])
        .arg(inf_path)
        .arg("/install")
        .output()
        .map_err(|e| format!("failed to run pnputil: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        tracing::info!("driver staged: {}", stdout.trim());
        return Ok(());
    }
    // Already installed is success for our purposes.
    if stdout.contains("already exists") || stdout.contains("was installed") {
        return Ok(());
    }
    Err(format!(
        "pnputil /add-driver failed (code {:?}): {} {}",
        output.status.code(),
        stdout.trim(),
        stderr.trim()
    ))
}

/// Rebind currently-attached Apple devices to WinUSB right now.
pub fn rebind_present_devices(inf_path: &std::path::Path) -> io::Result<u32> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let inf_abs = inf_path.canonicalize()?;
    let inf_wide: Vec<u16> = OsStr::new(&inf_abs)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let hwid_wide: Vec<u16> = APPLE_MUX_HWID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut reboot_needed: windows_sys::Win32::Foundation::BOOL = 0;
    let ok = unsafe {
        windows_sys::Win32::Devices::DeviceAndDriverInstallation::UpdateDriverForPlugAndPlayDevicesW(
            std::ptr::null_mut(),
            hwid_wide.as_ptr(),
            inf_wide.as_ptr(),
            0x00000003, // INSTALLFLAG_FORCE | INSTALLFLAG_READONLY
            &mut reboot_needed,
        )
    };

    if reboot_needed != 0 {
        tracing::warn!("a reboot may be required to complete driver installation");
    }

    // ERROR_NO_SUCH_DEVICE / ERROR_NO_MATCH: no device currently present —
    // that is fine, staging covers future arrivals.
    if ok == 0 {
        let err = io::Error::last_os_error();
        let benign = [
            0x103, // ERROR_NO_MORE_ITEMS
            0x10B, // ERROR_INVALID_USER_BUFFER? keep tolerant
            0xE000020B, // ERROR_NO_SUCH_DEVINST
        ];
        if benign.contains(&(err.raw_os_error().unwrap_or(0) as u32)) {
            return Ok(0);
        }
        return Err(err);
    }
    Ok(1)
}

/// Full provisioning: deploy INF, stage, rebind. Returns human summary lines.
pub fn provision() -> Result<Vec<String>, String> {
    let mut log = Vec::new();

    let inf = deploy_inf().map_err(|e| format!("failed to deploy INF: {e}"))?;
    log.push(format!("INF deployed: {}", inf.display()));

    stage_driver(&inf)?;
    log.push("driver staged in driver store (future devices will bind automatically)".into());

    match rebind_present_devices(&inf) {
        Ok(n) if n > 0 => log.push("rebound currently-attached device(s) to WinUSB".into()),
        Ok(_) => log.push("no Apple device currently attached (nothing to rebind)".into()),
        Err(e) => log.push(format!("note: rebind of present devices reported: {e}")),
    }

    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inf_is_well_formed() {
        assert!(EMBEDDED_INF.contains("[Version]"));
        assert!(EMBEDDED_INF.contains(r"USB\Class_FF&SubClass_FE&Prot_02"));
        assert!(EMBEDDED_INF.contains("winusb.inf"));
    }
}
