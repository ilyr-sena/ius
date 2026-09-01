use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("usbmuxd connection failed: {0}")]
    Connection(#[from] std::io::Error),
}
