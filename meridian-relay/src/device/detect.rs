use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice};
use tracing::debug;

const DAEMON_SOCKET: &str = "/tmp/meridian-relay-usbmuxd.sock";

pub fn ensure_daemon_socket_env() {
    if std::path::Path::new(DAEMON_SOCKET).exists() {
        // SAFETY: called early in main before threads
        unsafe { std::env::set_var("USBMUXD_SOCKET_ADDRESS", DAEMON_SOCKET); }
    }
}

pub async fn connect_usbmuxd() -> Result<UsbmuxdConnection, idevice::IdeviceError> {
    let addr = UsbmuxdAddr::from_env_var().unwrap_or_default();
    debug!("connecting to usbmuxd at {addr:?}");
    addr.connect(0).await
}

pub async fn list_raw_devices(
    conn: &mut UsbmuxdConnection,
) -> Result<Vec<UsbmuxdDevice>, idevice::IdeviceError> {
    let devs = conn.get_devices().await?;
    debug!("found {} raw device(s)", devs.len());
    Ok(devs)
}
