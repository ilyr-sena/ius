mod device;

use clap::{Parser, Subcommand};
use tracing::{info, warn, error};
use tracing_subscriber::EnvFilter;

use device::detect::ensure_daemon_socket_env;
use device::monitor::{get_devices_snapshot, watch_devices};

#[derive(Parser)]
#[command(name = "meridian-relay", version, about = "USB device management daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List connected devices and exit
    List,
    /// Watch for device connect/disconnect events
    Watch,
    /// Show detailed info for a specific device
    Info {
        /// Device UDID
        udid: String,
    },
    /// Start the usbmuxd daemon (runs in foreground)
    Daemon,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_thread_ids(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::List => cmd_list().await,
        Commands::Watch => cmd_watch().await,
        Commands::Info { udid } => cmd_info(&udid).await,
        Commands::Daemon => cmd_daemon().await,
    }
}

async fn cmd_daemon() {
    info!("starting meridian-relay usbmuxd daemon");
    if let Err(e) = meridian_relay::daemon::run_daemon().await {
        eprintln!("daemon error: {e}");
        std::process::exit(1);
    }
}

async fn cmd_list() {
    start_daemon_if_needed().await;
    ensure_daemon_socket_env();

    match get_devices_snapshot().await {
        Ok(devices) if devices.is_empty() => {
            println!("No devices connected.");
        }
        Ok(devices) => {
            println!("Found {} device(s):\n", devices.len());
            for dev in &devices {
                print_device(dev);
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_watch() {
    start_daemon_if_needed().await;
    ensure_daemon_socket_env();

    info!("watching for device events (Ctrl+C to quit)\n");

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let watch = watch_devices(|event| {
        println!("{event}");
    });

    tokio::select! {
        _ = watch => {}
        _ = ctrl_c => {
            println!("\nexiting.");
        }
    }
}

async fn cmd_info(udid: &str) {
    start_daemon_if_needed().await;
    ensure_daemon_socket_env();

    match get_devices_snapshot().await {
        Ok(devices) => {
            if let Some(dev) = devices.iter().find(|d| d.udid == udid) {
                print_device(dev);
                println!();
                println!("Raw JSON:");
                println!(
                    "{}",
                    serde_json::to_string_pretty(dev).unwrap_or_default()
                );
            } else {
                eprintln!("Device not found: {udid}");
                eprintln!("\nConnected devices:");
                for dev in &devices {
                    println!("  {} — {}", dev.udid, dev.name.as_deref().unwrap_or("Unknown"));
                }
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

async fn start_daemon_if_needed() {
    let sock = std::path::Path::new("/tmp/meridian-relay-usbmuxd.sock");

    // If socket exists, verify daemon is actually alive
    if sock.exists() {
        match tokio::net::UnixStream::connect(sock).await {
            Ok(_) => {
                info!("daemon already running");
                return;
            }
            Err(_) => {
                warn!("stale daemon socket found, cleaning up");
                let _ = std::fs::remove_file(sock);
            }
        }
    }

    info!("starting usbmuxd daemon in background");
    tokio::spawn(async {
        if let Err(e) = meridian_relay::daemon::run_daemon().await {
            error!("daemon failed: {e}");
        }
    });

    // Wait for socket to appear
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if sock.exists() {
            info!("daemon ready");
            return;
        }
    }
    warn!("daemon socket not found after 5s, proceeding anyway");
}

fn print_device(dev: &device::Device) {
    let name = dev.name.as_deref().unwrap_or("Unknown");
    let model = dev.model.as_deref().unwrap_or("?");
    let ios = dev.ios_version.as_deref().unwrap_or("?");
    let build = dev.build_version.as_deref().unwrap_or("?");

    println!("  {name} ({model})");
    println!("    UDID:          {}", dev.udid);
    println!("    iOS:           {ios} ({build})");
    println!("    Connection:    {}", dev.connection_type);
    println!();
}
