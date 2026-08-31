#![allow(dead_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("lockdown connect failed: {0}")]
    LockdownConnect(#[from] idevice::IdeviceError),

    #[error("device not paired or not accessible")]
    NotAccessible,
}
