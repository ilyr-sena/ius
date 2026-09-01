use std::collections::HashMap;
use tokio::process::Command;
use tracing::{debug, warn};

use super::Device;

pub async fn enrich_device_info(device: &mut Device) {
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

    device.name = values.get("DeviceName").cloned();
    device.model = values
        .get("ProductType")
        .map(|s| model_name(s).unwrap_or(s).to_string());
    device.ios_version = values.get("ProductVersion").cloned();
    device.build_version = values.get("BuildVersion").cloned();

    debug!(
        "enriched {}: name={:?} model={:?} ios={:?} build={:?}",
        device.udid, device.name, device.model, device.ios_version, device.build_version
    );
}

pub async fn enrich_all(devices: &mut [Device]) {
    // Query all devices in parallel
    let handles: Vec<_> = devices
        .iter_mut()
        .map(|dev| {
            let udid = dev.udid.trim().trim_end_matches('\0').to_string();
            async move {
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
                        return (udid, None);
                    }
                    Err(_) => {
                        warn!("ideviceinfo timed out for {udid}");
                        return (udid, None);
                    }
                };

                if !output.status.success() {
                    return (udid, None);
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                let values = parse_ideviceinfo_output(&stdout);
                (udid, Some(values))
            }
        })
        .collect();

    let results = futures::future::join_all(handles).await;

    for (udid, values) in results {
        if let Some(values) = values {
            if let Some(dev) = devices.iter_mut().find(|d| d.udid.trim().trim_end_matches('\0') == udid.trim()) {
                dev.name = values.get("DeviceName").cloned();
                dev.model = values
                    .get("ProductType")
                    .map(|s| model_name(s).unwrap_or(s).to_string());
                dev.ios_version = values.get("ProductVersion").cloned();
                dev.build_version = values.get("BuildVersion").cloned();
            }
        }
    }
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

fn model_name(identifier: &str) -> Option<&'static str> {
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
