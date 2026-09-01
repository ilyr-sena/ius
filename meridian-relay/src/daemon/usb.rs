use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::error;

use super::mux::{MuxError, MuxEvent, CONTROL_DEVICE_ADD, CONTROL_DEVICE_REMOVE};
use crate::metrics::Metrics;

pub struct AppleMuxInterface {
    handle: rusb::DeviceHandle<rusb::GlobalContext>,
    _interface: u8,
    read_endpoint: u8,
    write_endpoint: u8,
    io_timeout: std::time::Duration,
}

impl AppleMuxInterface {
    pub fn open(device: &rusb::Device<rusb::GlobalContext>, io_timeout: std::time::Duration) -> Result<Self, MuxError> {
        let handle: rusb::DeviceHandle<rusb::GlobalContext> = device.open().map_err(|e: rusb::Error| MuxError::UsbError(e.to_string()))?;
        let config = device.active_config_descriptor()
            .map_err(|e: rusb::Error| MuxError::UsbError(e.to_string()))?;

        let mut interface_num = 0u8;
        let mut read_ep = 0u8;
        let mut write_ep = 0u8;

        for iface in config.interfaces() {
            for desc in iface.descriptors() {
                for ep in desc.endpoint_descriptors() {
                    if ep.transfer_type() == rusb::TransferType::Bulk {
                        match ep.direction() {
                            rusb::Direction::In => {
                                read_ep = ep.address();
                                interface_num = iface.number() as u8;
                            }
                            rusb::Direction::Out => {
                                write_ep = ep.address();
                            }
                        }
                    }
                }
            }
        }

        if read_ep == 0 || write_ep == 0 {
            return Err(MuxError::UsbError("could not find mux endpoints".into()));
        }

        handle.claim_interface(interface_num)
            .map_err(|e: rusb::Error| MuxError::UsbError(e.to_string()))?;

        Ok(Self {
            handle,
            _interface: interface_num,
            read_endpoint: read_ep,
            write_endpoint: write_ep,
            io_timeout,
        })
    }

    pub fn send(&self, data: &[u8]) -> Result<(), MuxError> {
        self.handle.write_bulk(self.write_endpoint, data, self.io_timeout)
            .map_err(|e: rusb::Error| MuxError::UsbError(e.to_string()))?;
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8], timeout: std::time::Duration) -> Result<usize, std::io::Error> {
        self.handle.read_bulk(self.read_endpoint, buf, timeout)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }
}

pub struct UsbReader {
    usb: Arc<AppleMuxInterface>,
    packet_tx: mpsc::Sender<Vec<u8>>,
    metrics: Arc<Metrics>,
}

impl UsbReader {
    pub fn new(usb: Arc<AppleMuxInterface>, packet_tx: mpsc::Sender<Vec<u8>>, metrics: Arc<Metrics>) -> Self {
        Self { usb, packet_tx, metrics }
    }

    /// Spawn a single blocking read worker.
    ///
    /// NOTE: exactly one reader is used per device on purpose. The mux
    /// protocol is an ordered byte stream; concurrent bulk reads on the same
    /// endpoint can complete out of order and corrupt reassembly. A single
    /// 48 KiB-per-poll reader is sufficient for lockdown-class traffic.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let usb = self.usb;
            let tx = self.packet_tx;
            let metrics = self.metrics;
            let _ = tokio::task::spawn_blocking(move || {
                Self::read_worker(usb, tx, metrics);
            })
            .await;
        })
    }

    fn read_worker(usb: Arc<AppleMuxInterface>, tx: mpsc::Sender<Vec<u8>>, metrics: Arc<Metrics>) {
        let mut buf = vec![0u8; 49152];
        loop {
            match usb.read(&mut buf, std::time::Duration::from_millis(100)) {
                Ok(n) if n > 0 => {
                    metrics.usb_rx_bytes.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    let data = buf[..n].to_vec();
                    if tx.blocking_send(data).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Timeout") || msg.contains("timed out") {
                        continue;
                    }
                    error!("USB read error: {e}");
                    break;
                }
            }
        }
    }
}

pub struct UsbPacket {
    pub data: Vec<u8>,
}

impl UsbPacket {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

pub fn parse_control_message(message: u8, payload: &[u8]) -> Option<MuxEvent> {
    match message {
        CONTROL_DEVICE_ADD => {
            if payload.len() >= 4 {
                let device_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let product_id = if payload.len() >= 6 {
                    u16::from_be_bytes([payload[4], payload[5]])
                } else {
                    0
                };
                Some(MuxEvent::DeviceAdded {
                    device_id,
                    product_id,
                    connection_type: "USB".into(),
                    usb_speed: "Unknown".into(),
                })
            } else {
                None
            }
        }
        CONTROL_DEVICE_REMOVE => {
            if payload.len() >= 4 {
                let device_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Some(MuxEvent::DeviceRemoved { device_id })
            } else {
                None
            }
        }
        _ => None,
    }
}
