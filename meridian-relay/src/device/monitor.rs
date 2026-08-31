use futures::StreamExt;
use idevice::usbmuxd::UsbmuxdListenEvent;
use tracing::{debug, error, info};

use super::detect::{connect_usbmuxd, list_raw_devices};
use super::info::enrich_device_info;
use super::{Device, DeviceEvent};

pub async fn get_devices_snapshot() -> Result<Vec<Device>, idevice::IdeviceError> {
    let mut conn = connect_usbmuxd().await?;
    let raws = list_raw_devices(&mut conn).await?;

    let mut devices = Vec::with_capacity(raws.len());
    for raw in &raws {
        let mut dev = Device::from_usbmuxd(raw);
        enrich_device_info(raw, &mut dev).await;
        devices.push(dev);
    }

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
) -> Result<(), idevice::IdeviceError> {
    let mut conn = connect_usbmuxd().await?;

    // snapshot existing devices
    let raws = list_raw_devices(&mut conn).await?;
    let mut known: std::collections::HashMap<u32, Device> = std::collections::HashMap::new();
    for raw in &raws {
        let mut dev = Device::from_usbmuxd(raw);
        enrich_device_info(raw, &mut dev).await;
        on_event(DeviceEvent::Connected(dev.clone()));
        known.insert(raw.device_id, dev);
    }

    debug!("entering listen loop with {} known device(s)", known.len());

    let mut stream = conn.listen().await?;
    while let Some(item) = stream.next().await {
        match item {
            Ok(UsbmuxdListenEvent::Connected(raw)) => {
                info!("device connected: {}", raw.udid);
                let mut dev = Device::from_usbmuxd(&raw);
                enrich_device_info(&raw, &mut dev).await;
                on_event(DeviceEvent::Connected(dev.clone()));
                known.insert(raw.device_id, dev);
            }
            Ok(UsbmuxdListenEvent::Disconnected(device_id)) => {
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
