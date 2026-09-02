//! Windows Service Control Manager integration (install / uninstall / run).

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::service::{
    ServiceErrorControl, ServiceInfo, ServiceStartType,
};
use windows_service::{define_windows_service, service_dispatcher};

use crate::config::DaemonConfig;
use crate::metrics::Metrics;
use crate::daemon::run_daemon;

pub const SERVICE_NAME: &str = "meridian-relay";
const SERVICE_DISPLAY: &str = "Meridian Relay (iOS USB mux)";
const SERVICE_DESC: &str = "Cross-platform usbmuxd-compatible USB multiplexing relay for iOS devices.";

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

define_windows_service!(ffi_service_main, service_main);

/// Install the daemon as a Windows service (auto-start, own process, LocalSystem).
pub fn install_service(binary_path: &Path) -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: binary_path.to_path_buf(),
        launch_arguments: vec![OsString::from("daemon"), OsString::from("--service-run")],
        dependencies: vec![],
        account_name: None, // LocalSystem — required for claiming USB driver interfaces
        account_password: None,
    };

    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
    service.set_description(SERVICE_DESC)?;
    Ok(())
}

/// Remove the service from the SCM (stops it first if running).
pub fn uninstall_service() -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;

    let status = service.query_status()?;
    if status.current_state != ServiceState::Stopped {
        let _ = service.stop();
    }
    service.delete()?;
    Ok(())
}

/// Entry point used by the SCM. Blocks until the service process exits.
/// When running interactively, this returns an error immediately.
pub fn run_as_service() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        eprintln!("service failed: {e}");
    }
}

fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let status_handle = service_control_handler::register(SERVICE_NAME, move |control_event| {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                STOP_REQUESTED.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let config = DaemonConfig::default();
    let metrics = Arc::new(Metrics::new());

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let shutdown = async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if STOP_REQUESTED.load(Ordering::SeqCst) {
                break;
            }
        }
    };

    let result = runtime.block_on(run_daemon(config, metrics, shutdown));

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    result
}
