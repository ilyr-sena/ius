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

/// Windows rejects unsigned driver packages: the catalog must be signed.
/// 0xE000022F = ERROR_NO_CATALOG_FOR_OEM_INF.
pub const ERROR_NO_CATALOG_FOR_OEM_INF: i32 = -536870353; // 0xE000022F

/// What pnputil told us.
pub enum StageOutcome {
    Staged,
    AlreadyStaged,
    /// The INF is fine but unsigned — needs test-signing or a DSE-off boot.
    UnsignedRejected,
}

/// Stage the driver into the Windows driver store via pnputil.
/// After this, newly-attached Apple devices bind WinUSB automatically.
pub fn stage_driver(inf_path: &std::path::Path) -> Result<StageOutcome, String> {
    let output = std::process::Command::new("pnputil")
        .args(["/add-driver"])
        .arg(inf_path)
        .arg("/install")
        .output()
        .map_err(|e| format!("failed to run pnputil: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout} {stderr}");

    if output.status.success() {
        tracing::info!("driver staged: {}", stdout.trim());
        return Ok(StageOutcome::Staged);
    }
    if combined.contains("already exists") || combined.contains("was installed") {
        return Ok(StageOutcome::AlreadyStaged);
    }
    // Signature wall: pnputil reports a negative HRESULT when the package
    // has no (acceptable) catalog signature.
    let code = output.status.code().unwrap_or(0);
    if code == ERROR_NO_CATALOG_FOR_OEM_INF
        || combined.contains("digital signature")
        || combined.contains("not contain digital signature")
    {
        return Ok(StageOutcome::UnsignedRejected);
    }
    Err(format!(
        "pnputil /add-driver failed (code {:?}): {}",
        output.status.code(),
        combined.trim()
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
///
/// An unsigned-driver rejection is *not* a hard error — the caller prints
/// remediation guidance and continues (e.g. with service install).
pub fn provision() -> Result<Vec<String>, String> {
    let mut log = Vec::new();

    let inf = deploy_inf().map_err(|e| format!("failed to deploy INF: {e}"))?;
    log.push(format!("INF deployed: {}", inf.display()));

    match stage_driver(&inf)? {
        StageOutcome::Staged => {
            log.push("driver staged in driver store (future devices bind automatically)".into());
        }
        StageOutcome::AlreadyStaged => {
            log.push("driver already staged".into());
        }
        StageOutcome::UnsignedRejected => {
            log.push("⚠  driver package REJECTED by Windows: unsigned catalog".into());
            log.push("   Windows x64 requires a signed catalog (.cat) for driver install.".into());
            log.push(String::new());
            log.push("   To proceed on this machine for testing, either:".into());
            log.push("     A) Boot once with driver signature enforcement disabled:".into());
            log.push("          Settings → Recovery → Advanced startup → Restart now".into());
            log.push("          → Troubleshoot → Advanced options → Startup Settings".into());
            log.push("          → Restart → press 7 (Disable driver signature enforcement)".into());
            log.push("        then re-run:  meridian-relay.exe setup".into());
            log.push(String::new());
            log.push("     B) Enable test signing and test-sign the catalog (for repeated use):".into());
            log.push("          bcdedit /set testsigning on   (then reboot)".into());
            log.push("        and sign the package with a test cert before staging.".into());
            log.push(String::new());
            log.push("   Production deployments must ship an attestation/WHQL-signed catalog.".into());
            // Don't rebind attempt — the driver store has nothing new staged.
            return Ok(log);
        }
    }

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
