#![allow(dead_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("usbmuxd connection failed: {0}")]
    Connection(#[from] std::io::Error),

    #[error("device not paired or not accessible")]
    NotAccessible,
}
