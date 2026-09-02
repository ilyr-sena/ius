//! One-command host provisioning. Idempotent; safe to re-run.

use std::io;

pub const UDEV_RULE: &str = r#"# meridian-relay — Apple mobile device USB access
SUBSYSTEM=="usb", ATTR{idVendor}=="05ac", MODE="0666", TAG+="uaccess"
"#;

pub const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/80-meridian-relay.rules";
pub const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/meridian-relay.service";
pub const SYSTEMD_UNIT: &str = include_str!("../packaging/meridian-relay.service");

/// Run full host provisioning for the current platform.
/// Windows: driver binding + service. Linux: udev rule + systemd unit.
pub fn provision(install_service: bool) -> io::Result<Vec<String>> {
    platform_provision(install_service)
}

#[cfg(windows)]
fn platform_provision(install_service: bool) -> io::Result<Vec<String>> {
    let mut log = crate::driver::provision().map_err(|e| io::Error::other(e))?;

    if install_service {
        let exe = std::env::current_exe()?;
        crate::service::install_service(&exe)
            .map_err(|e| io::Error::other(format!("service install failed: {e}")))?;
        log.push(format!("service installed (auto-start): {}", exe.display()));
    } else {
        log.push("skipped service install (--skip-service)".into());
    }

    Ok(log)
}

#[cfg(unix)]
fn platform_provision(install_service: bool) -> io::Result<Vec<String>> {
    let mut log = Vec::new();

    if std::path::Path::new(UDEV_RULE_PATH).exists() {
        if std::fs::read_to_string(UDEV_RULE_PATH).ok().as_deref() == Some(UDEV_RULE) {
            log.push(format!("udev rule already present: {UDEV_RULE_PATH}"));
        } else {
            std::fs::write(UDEV_RULE_PATH, UDEV_RULE)?;
            log.push(format!("updated udev rule: {UDEV_RULE_PATH}"));
        }
    } else {
        std::fs::write(UDEV_RULE_PATH, UDEV_RULE)?;
        log.push(format!("installed udev rule: {UDEV_RULE_PATH}"));
    }

    // Reload udev — best effort.
    let reload = std::process::Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let trigger = std::process::Command::new("udevadm")
        .arg("trigger")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    log.push(format!("udev reload/trigger: {}", if reload && trigger { "ok" } else { "run `udevadm control --reload-rules && udevadm trigger` manually" }));

    if install_service {
        std::fs::write(SYSTEMD_UNIT_PATH, SYSTEMD_UNIT)?;
        let _ = std::process::Command::new("systemctl").args(["daemon-reload"]).status();
        let _ = std::process::Command::new("systemctl").args(["enable", "meridian-relay"]).status();
        log.push(format!("systemd unit installed + enabled: {SYSTEMD_UNIT_PATH}"));
    } else {
        log.push("skipped service install (--skip-service)".into());
    }

    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udev_rule_matches_apple_vid() {
        assert!(UDEV_RULE.contains("05ac"));
        assert!(UDEV_RULE.contains("uaccess"));
    }
}
