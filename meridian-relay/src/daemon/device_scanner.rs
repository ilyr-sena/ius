use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use rusb::{Context, UsbContext};
use tracing::{debug, info, warn};

const APPLE_VENDOR_ID: u16 = 0x05AC;

/// A known device must be absent from this many *consecutive successful*
/// scans before we declare it detached. Transient USB enumeration hiccups
/// (short reads right at plug/unplug time) must never tear down live
/// connections.
const DETACH_AFTER_MISSES: u32 = 2;

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

/// What one enumeration saw at a physical port. `udid` is `None` when the
/// device was present but its serial descriptor could not be read this round
/// (happens transiently on plug; must not trigger a detach).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanObservation {
    pub usb_bus: u8,
    pub usb_address: u8,
    pub product_id: u16,
    pub udid: Option<String>,
}

pub struct DeviceScanner {
    devices: Arc<RwLock<HashMap<u32, UsbDevice>>>,
    /// Consecutive-miss counters keyed by physical port.
    miss_counts: HashMap<(u8, u8), u32>,
    next_id: u32,
    usb_context: Option<Context>,
}

impl DeviceScanner {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            miss_counts: HashMap::new(),
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

    /// Enumerate and diff. On enumeration failure the previous device set is
    /// kept untouched (a failed scan must never masquerade as "zero devices").
    pub fn scan(&mut self) -> Vec<DeviceChange> {
        let current = match enumerate_apple_devices(self.get_context()) {
            Ok(c) => c,
            Err(e) => {
                warn!("USB enumeration failed ({e}); keeping current device state");
                return Vec::new();
            }
        };

        let mut devices = match self.devices.write() {
            Ok(d) => d,
            Err(e) => {
                warn!("scanner devices lock poisoned: {e}");
                return Vec::new();
            }
        };

        diff_devices(&mut devices, &mut self.miss_counts, &mut self.next_id, &current)
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

/// Pure attach/detach diff — extracted for unit testing without real USB.
///
/// Invariants:
/// - Ports are keyed by (bus, address): address numbers are only unique per
///   bus, two devices may share an address number on different buses.
/// - A *readable* device that stays put keeps its identity and id.
/// - A device at a port with a *different* UDID is a replacement: detach+attach.
/// - A port that reports present-but-unreadable (udid=None) counts as alive
///   for detach purposes but is never attached until readable.
fn diff_devices(
    devices: &mut HashMap<u32, UsbDevice>,
    miss_counts: &mut HashMap<(u8, u8), u32>,
    next_id: &mut u32,
    current: &[ScanObservation],
) -> Vec<DeviceChange> {
    let mut changes = Vec::new();

    let current_ports: std::collections::HashSet<(u8, u8)> =
        current.iter().map(|o| (o.usb_bus, o.usb_address)).collect();

    // Attach / replace.
    for obs in current {
        let key = (obs.usb_bus, obs.usb_address);
        miss_counts.remove(&key);

        let existing = devices.values().find(|d| d.usb_bus == obs.usb_bus && d.usb_address == obs.usb_address);
        match (existing, &obs.udid) {
            (Some(existing), Some(udid)) if existing.udid != *udid => {
                // Same physical port, different device: detach then attach.
                let old_id = existing.device_id;
                info!("- USB device replaced at {:03}/{:03}: {} → {}", obs.usb_bus, obs.usb_address, existing.udid, udid);
                devices.remove(&old_id);
                let device_id = *next_id;
                *next_id += 1;
                let dev = UsbDevice {
                    device_id,
                    udid: udid.clone(),
                    product_id: obs.product_id,
                    usb_bus: obs.usb_bus,
                    usb_address: obs.usb_address,
                };
                info!("+ USB device attached: {} (product=0x{:04X}, id={})", dev.udid, dev.product_id, device_id);
                devices.insert(device_id, dev.clone());
                changes.push(DeviceChange::Detached { device_id: old_id });
                changes.push(DeviceChange::Attached(dev));
            }
            (None, Some(udid)) => {
                let device_id = *next_id;
                *next_id += 1;
                let dev = UsbDevice {
                    device_id,
                    udid: udid.clone(),
                    product_id: obs.product_id,
                    usb_bus: obs.usb_bus,
                    usb_address: obs.usb_address,
                };
                info!("+ USB device attached: {} (product=0x{:04X}, id={})", dev.udid, dev.product_id, device_id);
                devices.insert(device_id, dev.clone());
                changes.push(DeviceChange::Attached(dev));
            }
            (None, None) => {
                debug!("Apple device at port {:03}/{:03} present but serial unreadable — will retry", obs.usb_bus, obs.usb_address);
            }
            (Some(_), _) => {
                // Known device still visible (serial confirmed or transiently unreadable).
                debug!("device at port {:03}/{:03} confirmed alive", obs.usb_bus, obs.usb_address);
            }
        }
    }

    // Detach (with flap suppression).
    let known_ports: Vec<((u8, u8), u32, String)> = devices
        .values()
        .map(|d| ((d.usb_bus, d.usb_address), d.device_id, d.udid.clone()))
        .collect();

    for (port, device_id, udid) in known_ports {
        if current_ports.contains(&port) {
            continue;
        }
        let misses = miss_counts.entry(port).or_insert(0);
        *misses += 1;
        if *misses >= DETACH_AFTER_MISSES {
            info!("- USB device detached: {udid} (id={device_id}, port {:03}/{:03})", port.0, port.1);
            devices.remove(&device_id);
            miss_counts.remove(&port);
            changes.push(DeviceChange::Detached { device_id });
        } else {
            debug!("device {udid} missed scan {} of {DETACH_AFTER_MISSES}", *miss_counts.get(&port).unwrap_or(&0));
        }
    }

    changes
}

/// Enumerate USB devices and return observations for Apple devices.
/// Returns an error (rather than an empty list) when enumeration itself fails,
/// so callers can distinguish "no devices" from "couldn't look".
fn enumerate_apple_devices(context: Option<&Context>) -> Result<Vec<ScanObservation>, String> {
    let context = context.ok_or("no USB context available")?;

    let device_list = context.devices()
        .map_err(|e| format!("failed to enumerate USB devices: {e}"))?;

    let mut out = Vec::new();
    for device in device_list.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() != APPLE_VENDOR_ID {
            continue;
        }

        let udid = match device.open() {
            Ok(handle) => match handle.read_serial_number_string_ascii(&desc) {
                Ok(s) if !s.trim_matches('\0').trim().is_empty() => {
                    Some(s.trim().trim_end_matches('\0').to_string())
                }
                _ => None,
            },
            Err(_) => None,
        };

        out.push(ScanObservation {
            usb_bus: device.bus_number(),
            usb_address: device.address(),
            product_id: desc.product_id(),
            udid,
        });
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(bus: u8, addr: u8, udid: Option<&str>) -> ScanObservation {
        ScanObservation {
            usb_bus: bus,
            usb_address: addr,
            product_id: 0x12A8,
            udid: udid.map(String::from),
        }
    }

    fn fixture() -> (HashMap<u32, UsbDevice>, HashMap<(u8, u8), u32>, u32) {
        (HashMap::new(), HashMap::new(), 1)
    }

    #[test]
    fn attach_and_detach() {
        let (mut devs, mut misses, mut next) = fixture();
        let changes = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 5, Some("AAAA"))]);
        assert!(matches!(changes.as_slice(), [DeviceChange::Attached(_)]));
        assert_eq!(devs.len(), 1);

        // Detach is deferred across DETACH_AFTER_MISSES scans.
        let c1 = diff_devices(&mut devs, &mut misses, &mut next, &[]);
        assert!(c1.is_empty(), "first miss must not detach");
        let c2 = diff_devices(&mut devs, &mut misses, &mut next, &[]);
        assert!(matches!(c2.as_slice(), [DeviceChange::Detached { device_id: 1 }]));
        assert!(devs.is_empty());
    }

    #[test]
    fn crossbus_same_address_no_collision() {
        let (mut devs, mut misses, mut next) = fixture();
        let changes = diff_devices(&mut devs, &mut misses, &mut next, &[
            obs(1, 7, Some("AAAA")),
            obs(2, 7, Some("BBBB")), // same address number, different bus
        ]);
        assert_eq!(changes.len(), 2, "both devices must attach distinctly");
        assert_eq!(devs.len(), 2);

        // Removing the bus-1 device must detach it despite same address on bus 2.
        for _ in 0..DETACH_AFTER_MISSES {
            diff_devices(&mut devs, &mut misses, &mut next, &[obs(2, 7, Some("BBBB"))]);
        }
        assert_eq!(devs.len(), 1);
        let remaining = devs.values().next().unwrap();
        assert_eq!(remaining.udid, "BBBB");
        assert_eq!(remaining.usb_bus, 2);
    }

    #[test]
    fn replacement_same_port() {
        let (mut devs, mut misses, mut next) = fixture();
        diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 3, Some("AAAA"))]);
        let changes = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 3, Some("BBBB"))]);
        assert!(
            matches!(changes[0], DeviceChange::Detached { .. }) && matches!(changes[1], DeviceChange::Attached(_)),
            "replacement must detach-then-attach: {changes:?}"
        );
        assert_eq!(devs.len(), 1);
        assert_eq!(devs.values().next().unwrap().udid, "BBBB");
    }

    #[test]
    fn unreadable_serial_never_defaches() {
        let (mut devs, mut misses, mut next) = fixture();
        diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 9, Some("AAAA"))]);
        // Transiently unreadable serial — device still present at the port.
        for _ in 0..10 {
            let c = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 9, None)]);
            assert!(c.is_empty(), "unreadable serial must not flap");
        }
        assert_eq!(devs.len(), 1);
    }

    #[test]
    fn unreadable_new_device_waits() {
        let (mut devs, mut misses, mut next) = fixture();
        let c = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 9, None)]);
        assert!(c.is_empty());
        assert!(devs.is_empty(), "serial-less device must not be attached");
        // Once readable, it attaches.
        let c = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 9, Some("AAAA"))]);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn reconnect_after_detach_gets_fresh_id() {
        let (mut devs, mut misses, mut next) = fixture();
        let c1 = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 5, Some("AAAA"))]);
        let id1 = match &c1[0] { DeviceChange::Attached(d) => d.device_id, _ => panic!() };
        for _ in 0..DETACH_AFTER_MISSES {
            diff_devices(&mut devs, &mut misses, &mut next, &[]);
        }
        let c2 = diff_devices(&mut devs, &mut misses, &mut next, &[obs(1, 5, Some("AAAA"))]);
        let id2 = match &c2[0] { DeviceChange::Attached(d) => d.device_id, _ => panic!() };
        assert!(id2 > id1, "reconnect must mint a fresh device id");
    }

    #[test]
    fn multi_device_partial_outage() {
        let (mut devs, mut misses, mut next) = fixture();
        diff_devices(&mut devs, &mut misses, &mut next, &[
            obs(1, 1, Some("AAAA")),
            obs(1, 2, Some("BBBB")),
            obs(1, 3, Some("CCCC")),
        ]);
        assert_eq!(devs.len(), 3);

        // One device disappears (cable yank), others keep working.
        let mut detaches = 0;
        for _ in 0..DETACH_AFTER_MISSES {
            let c = diff_devices(&mut devs, &mut misses, &mut next, &[
                obs(1, 1, Some("AAAA")),
                obs(1, 3, Some("CCCC")),
            ]);
            detaches += c.iter().filter(|ch| matches!(ch, DeviceChange::Detached { .. })).count();
        }
        assert_eq!(detaches, 1);
        assert_eq!(devs.len(), 2);
        let udids: Vec<_> = devs.values().map(|d| d.udid.as_str()).collect();
        assert!(udids.contains(&"AAAA") && udids.contains(&"CCCC"));
    }
}
