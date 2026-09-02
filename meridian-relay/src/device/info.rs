use std::collections::HashMap;
use tokio::process::Command;
use tracing::{debug, warn};

use super::Device;
use crate::daemon::transport::Endpoint;

/// Enrich a device's fields. Primary: lockdown GetValue over the daemon's own
/// endpoint (works on both platforms and both backends — direct USB or relay
/// to an upstream mux service; no external tools required). Fallback: the
/// `ideviceinfo` binary when present (a nice-to-have on top, not a dep).
pub async fn enrich_device_info(device: &mut Device, endpoint: &Endpoint) {
    if let Err(e) = super::lockdown::enrich_via_lockdown(device, endpoint).await {
        warn!("lockdown enrichment failed: {e}");
    }

    // Fill in anything still missing via ideviceinfo when available (unix
    // power tool; simply absent by default on Windows).
    if device.name.is_none() || device.ios_version.is_none() {
        enrich_via_ideviceinfo(device).await;
    }
}

async fn enrich_via_ideviceinfo(device: &mut Device) {
    let udid = device.udid.trim().trim_end_matches('\0').to_string();

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new("ideviceinfo")
            .args(["-u", &udid])
            .output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            warn!("ideviceinfo failed for {udid}: {e}");
            return;
        }
        Err(_) => {
            warn!("ideviceinfo timed out for {udid}");
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("ideviceinfo failed for {udid}: {stderr}");
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let values = parse_ideviceinfo_output(&stdout);

    // Fill gaps only — lockdown already gave us the primary values.
    if device.name.is_none() {
        device.name = values.get("DeviceName").cloned();
    }
    if device.model.is_none() {
        device.model = values
            .get("ProductType")
            .map(|s| model_name(s).unwrap_or(s).to_string());
    }
    if device.ios_version.is_none() {
        device.ios_version = values.get("ProductVersion").cloned();
    }
    if device.build_version.is_none() {
        device.build_version = values.get("BuildVersion").cloned();
    }

    debug!(
        "enriched {}: name={:?} model={:?} ios={:?} build={:?}",
        device.udid, device.name, device.model, device.ios_version, device.build_version
    );
}

pub async fn enrich_all(devices: &mut [Device], endpoint: &Endpoint) {
    // Parallel lockdown GetValue enrichment across all devices.
    let futures = devices
        .iter_mut()
        .map(|d| super::lockdown::enrich_via_lockdown(d, endpoint));
    let _ = futures::future::join_all(futures).await;
}

fn parse_ideviceinfo_output(output: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in output.lines() {
        if let Some((key, value)) = line.split_once(": ") {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

pub fn model_name(identifier: &str) -> Option<&'static str> {
    match identifier {
        // iPhone
        "iPhone13,1" => Some("iPhone 12 mini"),
        "iPhone13,2" => Some("iPhone 12"),
        "iPhone13,3" => Some("iPhone 12 Pro"),
        "iPhone13,4" => Some("iPhone 12 Pro Max"),
        "iPhone14,4" => Some("iPhone 13 mini"),
        "iPhone14,5" => Some("iPhone 13"),
        "iPhone14,2" => Some("iPhone 13 Pro"),
        "iPhone14,3" => Some("iPhone 13 Pro Max"),
        "iPhone14,7" => Some("iPhone 14"),
        "iPhone14,8" => Some("iPhone 14 Plus"),
        "iPhone15,2" => Some("iPhone 14 Pro"),
        "iPhone15,3" => Some("iPhone 14 Pro Max"),
        "iPhone15,4" => Some("iPhone 15"),
        "iPhone15,5" => Some("iPhone 15 Plus"),
        "iPhone16,1" => Some("iPhone 15 Pro"),
        "iPhone16,2" => Some("iPhone 15 Pro Max"),
        "iPhone17,1" => Some("iPhone 16 Pro"),
        "iPhone17,2" => Some("iPhone 16 Pro Max"),
        "iPhone17,3" => Some("iPhone 16"),
        "iPhone17,4" => Some("iPhone 16 Plus"),
        "iPhone17,5" => Some("iPhone 16e"),
        // iPad
        "iPad13,18" => Some("iPad (10th gen)"),
        "iPad13,19" => Some("iPad (10th gen)"),
        "iPad14,3" => Some("iPad Pro 11\" (4th gen)"),
        "iPad14,4" => Some("iPad Pro 11\" (4th gen)"),
        "iPad14,5" => Some("iPad Pro 12.9\" (6th gen)"),
        "iPad14,6" => Some("iPad Pro 12.9\" (6th gen)"),
        "iPad14,8" => Some("iPad Air 11\" (M2)"),
        "iPad14,9" => Some("iPad Air 11\" (M2)"),
        "iPad14,10" => Some("iPad Air 13\" (M2)"),
        "iPad14,11" => Some("iPad Air 13\" (M2)"),
        "iPad15,3" => Some("iPad Air 11\" (M3)"),
        "iPad15,4" => Some("iPad Air 11\" (M3)"),
        "iPad15,5" => Some("iPad Air 13\" (M3)"),
        "iPad15,6" => Some("iPad Air 13\" (M3)"),
        "iPad15,7" => Some("iPad mini (A17 Pro)"),
        "iPad15,8" => Some("iPad mini (A17 Pro)"),
        "iPad16,3" => Some("iPad Pro 11\" (M4)"),
        "iPad16,4" => Some("iPad Pro 11\" (M4)"),
        "iPad16,5" => Some("iPad Pro 13\" (M4)"),
        "iPad16,6" => Some("iPad Pro 13\" (M4)"),
        // iPod Touch
        "iPod9,1" => Some("iPod touch (7th gen)"),
        _ => None,
    }
}
