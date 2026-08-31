use idevice::lockdown::LockdownClient;
use idevice::provider::IdeviceProvider;
use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdDevice};
use idevice::IdeviceService;
use tracing::{debug, warn};

use super::Device;

const QUERY_KEYS: &[&str] = &[
    "DeviceName",
    "ProductType",
    "ProductVersion",
    "BuildVersion",
];

pub async fn enrich_device_info(raw: &UsbmuxdDevice, device: &mut Device) {
    let addr = match UsbmuxdAddr::from_env_var() {
        Ok(a) => a,
        Err(_) => UsbmuxdAddr::default(),
    };

    let provider = raw.to_provider(addr, "meridian-relay");

    let mut lockdown: LockdownClient = match LockdownClient::connect(&provider).await {
        Ok(l) => l,
        Err(e) => {
            warn!("lockdown connect failed for {}: {e}", device.udid);
            return;
        }
    };

    let pairing_file = match provider.get_pairing_file().await {
        Ok(pf) => pf,
        Err(e) => {
            warn!("pairing file failed for {}: {e}", device.udid);
            return;
        }
    };

    if let Err(e) = lockdown.start_session(&pairing_file).await {
        warn!("lockdown session failed for {}: {e}", device.udid);
        return;
    }

    for key in QUERY_KEYS {
        match lockdown.get_value(Some(*key), None).await {
            Ok(val) => {
                let s: Option<String> = match val {
                    plist::Value::String(s) => Some(s),
                    _ => None,
                };
                debug!("{}.{key} = {s:?}", device.udid);
                match *key {
                    "DeviceName" => device.name = s,
                    "ProductType" => device.model = s,
                    "ProductVersion" => device.ios_version = s,
                    "BuildVersion" => device.build_version = s,
                    _ => {}
                }
            }
            Err(e) => {
                debug!("{}.{key}: {e}", device.udid);
            }
        }
    }
}
