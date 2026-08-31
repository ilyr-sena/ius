#![allow(dead_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("usbmuxd connection failed: {0}")]
    Usbmuxd(#[from] idevice::IdeviceError),

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("lockdown connection failed for {udid}: {source}")]
    Lockdown {
        udid: String,
        source: idevice::IdeviceError,
    },

    #[error("no devices connected")]
    NoDevices,
}
