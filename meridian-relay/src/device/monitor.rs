use tracing::{debug, error, info};

use super::detect::{UsbmuxdClient, ListenEvent};
use super::info::enrich_all;
use super::{Device, DeviceEvent};

pub async fn get_devices_snapshot() -> Result<Vec<Device>, std::io::Error> {
    let mut client = UsbmuxdClient::connect().await?;
    let raws = client.send_list_devices().await?;

    let mut devices: Vec<Device> = raws.iter().map(|r| {
        Device::from_usb_device(&super::detect::raw_to_usb_device(r))
    }).collect();

    enrich_all(&mut devices).await;

    Ok(devices)
}

pub async fn watch_devices(mut on_event: impl FnMut(DeviceEvent)) {
    loop {
        match run_watch_loop(&mut on_event).await {
            Ok(()) => {
                info!("listen stream ended, reconnecting in 2s");
            }
            Err(e) => {
                error!("watch error: {e}, reconnecting in 2s");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn run_watch_loop(
    on_event: &mut impl FnMut(DeviceEvent),
) -> Result<(), std::io::Error> {
    let mut client = UsbmuxdClient::connect().await?;

    let raws = client.send_list_devices().await?;
    let mut devices: Vec<Device> = raws.iter().map(|r| {
        Device::from_usb_device(&super::detect::raw_to_usb_device(r))
    }).collect();
    enrich_all(&mut devices).await;

    let mut known: std::collections::HashMap<u32, Device> = std::collections::HashMap::new();
    for dev in devices {
        on_event(DeviceEvent::Connected(dev.clone()));
        known.insert(dev.device_id, dev);
    }

    debug!("entering listen loop with {} known device(s)", known.len());

    client.send_listen().await?;

    loop {
        match client.read_event().await {
            Ok(ListenEvent::Attached(raw)) => {
                info!("device connected: {}", raw.udid);
                let mut dev = Device::from_usb_device(&super::detect::raw_to_usb_device(&raw));
                super::info::enrich_device_info(&mut dev).await;
                on_event(DeviceEvent::Connected(dev.clone()));
                known.insert(raw.device_id, dev);
            }
            Ok(ListenEvent::Detached(device_id)) => {
                info!("device disconnected: mux_id={device_id}");
                if let Some(dev) = known.remove(&device_id) {
                    on_event(DeviceEvent::Disconnected {
                        udid: dev.udid,
                        device_id,
                    });
                }
            }
            Err(e) => {
                error!("listen event error: {e}");
                break;
            }
        }
    }

    Ok(())
}
