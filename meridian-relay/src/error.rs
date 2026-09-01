#![allow(dead_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("usbmuxd connection failed: {0}")]
    Connection(#[from] std::io::Error),

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("no devices connected")]
    NoDevices,
}
