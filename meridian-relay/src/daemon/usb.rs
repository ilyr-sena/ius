use std::sync::Arc;
use std::time::Duration;

use rusb::{DeviceDescriptor, Direction, TransferType, GlobalContext};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub const APPLE_VENDOR_ID: u16 = 0x05AC;
pub const USB_MRU: usize = 16384;
pub const USB_MTU: usize = 49152;
pub const ZLP_THRESHOLD: usize = 512;

const MUX_INTERFACE_CLASS: u8 = 0xFF;
const MUX_INTERFACE_SUBCLASS: u8 = 0xFE;
const MUX_INTERFACE_PROTOCOL: u8 = 0x02;

const USB_TIMEOUT: Duration = Duration::from_secs(5);

const NUM_READ_WORKERS: usize = 3;

#[derive(Debug, Error)]
pub enum UsbError {
    #[error("USB error: {0}")]
    Rusb(#[from] rusb::Error),

    #[error("no mux interface found on device")]
    NoMuxInterface,

    #[error("mux interface has no bulk endpoints")]
    NoBulkEndpoints,

    #[error("short write: {written} bytes written, expected {expected}")]
    ShortWrite { written: usize, expected: usize },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("worker task panicked: {0}")]
    TaskJoin(String),
}

pub type Result<T> = std::result::Result<T, UsbError>;

struct MuxInterfaceInfo {
    config_number: u8,
    interface_number: u8,
    ep_in: u8,
    ep_out: u8,
    max_packet_size: u16,
}

fn find_mux_interface(device: &rusb::Device<GlobalContext>) -> Result<MuxInterfaceInfo> {
    let desc: DeviceDescriptor = device.device_descriptor()?;

    for config_idx in (0..desc.num_configurations()).rev() {
        let config = match device.config_descriptor(config_idx) {
            Ok(c) => c,
            Err(e) => {
                debug!("failed to read config descriptor {config_idx}: {e}");
                continue;
            }
        };

        for iface in config.interfaces() {
            for iface_desc in iface.descriptors() {
                if iface_desc.class_code() != MUX_INTERFACE_CLASS
                    || iface_desc.sub_class_code() != MUX_INTERFACE_SUBCLASS
                    || iface_desc.protocol_code() != MUX_INTERFACE_PROTOCOL
                {
                    continue;
                }

                let mut ep_in: Option<u8> = None;
                let mut ep_out: Option<u8> = None;
                let mut max_packet_size: u16 = 0;

                for ep in iface_desc.endpoint_descriptors() {
                    if ep.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match ep.direction() {
                        Direction::In => {
                            ep_in = Some(ep.address());
                            max_packet_size = ep.max_packet_size();
                        }
                        Direction::Out => {
                            ep_out = Some(ep.address());
                        }
                    }
                }

                let ep_in = ep_in.ok_or(UsbError::NoBulkEndpoints)?;
                let ep_out = ep_out.ok_or(UsbError::NoBulkEndpoints)?;

                return Ok(MuxInterfaceInfo {
                    config_number: config.number(),
                    interface_number: iface_desc.interface_number(),
                    ep_in,
                    ep_out,
                    max_packet_size,
                });
            }
        }
    }

    Err(UsbError::NoMuxInterface)
}

pub struct AppleMuxInterface {
    handle: rusb::DeviceHandle<rusb::GlobalContext>,
    ep_out: u8,
    ep_in: u8,
    max_packet_size: u16,
}

impl AppleMuxInterface {
    pub fn open(device: &rusb::Device<GlobalContext>) -> Result<Self> {
        let info = find_mux_interface(device)?;

        let handle = device.open()?;

        for iface_idx in 0..16 {
            if handle.kernel_driver_active(iface_idx).unwrap_or(false) {
                debug!("detaching kernel driver on interface {iface_idx}");
                let _ = handle.detach_kernel_driver(iface_idx);
            }
        }

        if let Err(e) = handle.set_active_configuration(info.config_number) {
            debug!("set_active_configuration({}) failed: {e} (may already be active)", info.config_number);
        }

        match handle.claim_interface(info.interface_number) {
            Ok(()) => {}
            Err(rusb::Error::Busy) => {
                warn!("interface {} busy (another process may hold the device), retrying after 200ms", info.interface_number);
                std::thread::sleep(Duration::from_millis(200));
                handle.claim_interface(info.interface_number)?;
            }
            Err(e) => return Err(e.into()),
        }

        info!(
            "mux interface claimed: config={}, interface={}, ep_in=0x{:02X}, ep_out=0x{:02X}, max_packet={}",
            info.config_number, info.interface_number, info.ep_in, info.ep_out, info.max_packet_size
        );

        Ok(Self {
            handle,
            ep_out: info.ep_out,
            ep_in: info.ep_in,
            max_packet_size: info.max_packet_size,
        })
    }

    pub fn send(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let written = self.handle.write_bulk(self.ep_out, data, USB_TIMEOUT)?;
        if written != data.len() {
            return Err(UsbError::ShortWrite {
                written,
                expected: data.len(),
            });
        }

        debug!("USB send: {} bytes", data.len());

        if data.len() > 0 && data.len() % self.max_packet_size as usize == 0 {
            self.handle.write_bulk(self.ep_out, &[], USB_TIMEOUT)?;
            debug!("sent ZLP after {} bytes", data.len());
        }

        Ok(())
    }

    pub fn receive(&self, buf: &mut [u8]) -> Result<usize> {
        let n = self.handle.read_bulk(self.ep_in, buf, USB_TIMEOUT)?;
        Ok(n)
    }

    pub fn max_packet_size(&self) -> u16 {
        self.max_packet_size
    }
}

pub struct UsbReader {
    interface: Arc<AppleMuxInterface>,
    sender: mpsc::Sender<Vec<u8>>,
}

impl UsbReader {
    pub fn new(interface: Arc<AppleMuxInterface>, sender: mpsc::Sender<Vec<u8>>) -> Self {
        Self { interface, sender }
    }

    pub fn spawn(&self) {
        for worker_id in 0..NUM_READ_WORKERS {
            let interface = self.interface.clone();
            let sender = self.sender.clone();
            tokio::spawn(async move {
                read_loop(worker_id, interface, sender).await;
            });
        }

        info!("spawned {NUM_READ_WORKERS} USB read workers");
    }
}

async fn read_loop(
    worker_id: usize,
    interface: Arc<AppleMuxInterface>,
    sender: mpsc::Sender<Vec<u8>>,
) {
    info!("USB read worker {worker_id} started");

    loop {
        let iface = interface.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; USB_MRU];
            let n = iface.receive(&mut buf)?;
            buf.truncate(n);
            Ok::<_, UsbError>(buf)
        })
        .await;

        match result {
            Ok(Ok(data)) => {
                if data.is_empty() {
                    continue;
                }
                debug!("USB read worker {worker_id}: received {} bytes", data.len());
                if sender.send(data).await.is_err() {
                    debug!("USB read worker {worker_id}: receiver dropped, exiting");
                    break;
                }
            }
            Ok(Err(UsbError::Rusb(rusb::Error::Timeout))) => {
                continue;
            }
            Ok(Err(e)) => {
                warn!("USB read worker {worker_id}: {e}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(join_err) => {
                error!("USB read worker {worker_id}: task panicked: {join_err}");
                break;
            }
        }
    }

    info!("USB read worker {worker_id} stopped");
}

pub struct PacketReassembler {
    pktbuf: Vec<u8>,
    pktlen: usize,
}

impl PacketReassembler {
    pub fn new() -> Self {
        Self {
            pktbuf: Vec::new(),
            pktlen: 0,
        }
    }

    pub fn feed(&mut self, data: &[u8], expected_len: Option<usize>) -> Option<Vec<u8>> {
        if self.pktlen > 0 {
            self.pktbuf.extend_from_slice(data);

            if data.len() < USB_MRU {
                let pkt = std::mem::take(&mut self.pktbuf);
                self.pktlen = 0;
                return Some(pkt);
            }

            let total_len = if let Some(exp) = expected_len {
                exp
            } else if self.pktbuf.len() >= 4 {
                u32::from_be_bytes([
                    self.pktbuf[0],
                    self.pktbuf[1],
                    self.pktbuf[2],
                    self.pktbuf[3],
                ]) as usize
            } else {
                return None;
            };

            if self.pktbuf.len() >= total_len {
                let pkt = std::mem::take(&mut self.pktbuf);
                self.pktlen = 0;
                return Some(pkt);
            }

            None
        } else {
            if data.len() == USB_MRU && data.len() >= 4 {
                let total_len = u32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ]) as usize;

                if total_len > USB_MRU {
                    self.pktbuf.extend_from_slice(data);
                    self.pktlen = total_len;
                    return None;
                }
            }

            Some(data.to_vec())
        }
    }

    pub fn reset(&mut self) {
        self.pktbuf.clear();
        self.pktlen = 0;
    }
}
