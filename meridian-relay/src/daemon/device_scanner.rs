use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use rusb::{Context, UsbContext};
use tracing::{debug, info, warn};

const APPLE_VENDOR_ID: u16 = 0x05AC;

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub device_id: u32,
    pub udid: String,
    pub product_id: u16,
    pub usb_bus: u8,
    pub usb_address: u8,
}

#[derive(Debug, Clone)]
pub enum DeviceChange {
    Attached(UsbDevice),
    Detached { device_id: u32 },
}

pub struct DeviceScanner {
    devices: Arc<RwLock<HashMap<u32, UsbDevice>>>,
    next_id: u32,
    usb_context: Option<Context>,
}

impl DeviceScanner {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            next_id: 1,
            usb_context: None,
        }
    }

    fn get_context(&mut self) -> Option<&Context> {
        if self.usb_context.is_none() {
            self.usb_context = Context::new().ok();
        }
        self.usb_context.as_ref()
    }

    pub fn scan(&mut self) -> Vec<DeviceChange> {
        let mut changes = Vec::new();
        let current_devices = enumerate_apple_devices_with(self.get_context());

        let mut devices = match self.devices.write() {
            Ok(d) => d,
            Err(e) => {
                warn!("scanner devices lock poisoned: {e}");
                return Vec::new();
            }
        };
        let mut seen = std::collections::HashSet::new();

        for usb_dev in &current_devices {
            seen.insert(usb_dev.usb_address);

            if let Some(existing) = devices.values().find(|d| d.usb_address == usb_dev.usb_address && d.usb_bus == usb_dev.usb_bus) {
                debug!("device already known: {} (id={})", usb_dev.udid, existing.device_id);
            } else {
                let device_id = self.next_id;
                self.next_id += 1;

                let dev = UsbDevice {
                    device_id,
                    udid: usb_dev.udid.clone(),
                    product_id: usb_dev.product_id,
                    usb_bus: usb_dev.usb_bus,
                    usb_address: usb_dev.usb_address,
                };

                info!("+ USB device attached: {} (product=0x{:04X}, id={})", dev.udid, dev.product_id, device_id);
                devices.insert(device_id, dev.clone());
                changes.push(DeviceChange::Attached(dev));
            }
        }

        let to_remove: Vec<u32> = devices
            .iter()
            .filter(|(_, d)| !seen.contains(&d.usb_address))
            .map(|(id, _)| *id)
            .collect();

        for device_id in to_remove {
            if let Some(dev) = devices.remove(&device_id) {
                info!("- USB device detached: {} (id={})", dev.udid, device_id);
                changes.push(DeviceChange::Detached { device_id });
            }
        }

        changes
    }

    pub fn get_devices(&self) -> Vec<UsbDevice> {
        match self.devices.read() {
            Ok(d) => d.values().cloned().collect(),
            Err(e) => {
                warn!("scanner devices lock poisoned: {e}");
                Vec::new()
            }
        }
    }

    pub fn get_device_by_id(&self, id: u32) -> Option<UsbDevice> {
        match self.devices.read() {
            Ok(d) => d.get(&id).cloned(),
            Err(e) => {
                warn!("scanner devices lock poisoned: {e}");
                None
            }
        }
    }
}

struct RawUsbDevice {
    udid: String,
    product_id: u16,
    usb_bus: u8,
    usb_address: u8,
}

fn enumerate_apple_devices_with(context: Option<&Context>) -> Vec<RawUsbDevice> {
    let mut devices = Vec::new();

    let context_ref = match context {
        Some(c) => c,
        None => {
            warn!("no USB context available");
            return devices;
        }
    };

    let Ok(device_list) = context_ref.devices() else {
        warn!("failed to enumerate USB devices");
        return devices;
    };

    for device in device_list.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };

        if desc.vendor_id() != APPLE_VENDOR_ID {
            continue;
        }

        // Open device to read serial number
        let udid = match device.open() {
            Ok(handle) => {
                match handle.read_serial_number_string_ascii(&desc) {
                    Ok(s) if !s.is_empty() => s.trim().trim_end_matches('\0').to_string(),
                    _ => {
                        debug!("Apple device without serial, skipping");
                        continue;
                    }
                }
            }
            Err(_) => {
                debug!("cannot open Apple device to read serial, skipping");
                continue;
            }
        };

        devices.push(RawUsbDevice {
            udid,
            product_id: desc.product_id(),
            usb_bus: device.bus_number(),
            usb_address: device.address(),
        });
    }

    devices
}

pub fn product_id_to_model(pid: u16) -> &'static str {
    match pid {
        0x12A8..=0x12AF => "iPhone 5s / SE (1st gen)",
        0x12B0..=0x12B7 => "iPhone 6 / 6 Plus",
        0x12B8..=0x12BF => "iPhone 6s / 6s Plus / SE (2nd gen)",
        0x12C0..=0x12C7 => "iPhone 7 / 7 Plus",
        0x12C8..=0x12CF => "iPhone 8 / 8 Plus / X",
        0x12D0..=0x12D7 => "iPhone XR / XS / XS Max",
        0x12D8..=0x12DF => "iPhone 11 / 11 Pro / 11 Pro Max",
        0x12E0..=0x12E7 => "iPhone SE (2nd/3rd gen) / 12 mini / 12 / 12 Pro / 12 Pro Max",
        0x12E8..=0x12EF => "iPhone 13 / 13 mini / 13 Pro / 13 Pro Max",
        0x12F0..=0x12F7 => "iPhone 14 / 14 Plus / 14 Pro / 14 Pro Max",
        0x12F8..=0x12FF => "iPhone 15 / 15 Plus / 15 Pro / 15 Pro Max",
        0x1300..=0x1307 => "iPhone 16 / 16 Plus / 16 Pro / 16 Pro Max",
        0x1310..=0x1317 => "iPad (various models)",
        0x1318..=0x131F => "iPad Pro (various models)",
        0x1400..=0x1407 => "iPod Touch (7th gen)",
        _ => "Unknown Apple Device",
    }
}
