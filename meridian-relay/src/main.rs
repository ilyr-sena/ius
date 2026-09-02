use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};
use std::sync::Arc;

use meridian_relay::config::{DaemonConfig, DaemonArgs, LogFormat};
use meridian_relay::metrics::Metrics;
use meridian_relay::daemon::{self, transport::Endpoint};
use meridian_relay::device;
use meridian_relay::device::info::enrich_device_info;
use meridian_relay::device::monitor::watch_devices_from;
use meridian_relay::platform;

#[derive(Parser)]
#[command(name = "meridian-relay")]
#[command(version, about = "Meridian — cross-platform USB mux relay daemon (iOS / usbmuxd-compatible)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Daemon endpoint (unix:/path, pipe:name, tcp:127.0.0.1:port). Global flag;
    /// for client commands it selects where the daemon is reached.
    #[arg(short, long, global = true)]
    endpoint: Option<String>,

    /// [back-compat] unix socket path — same as `--endpoint unix:PATH`.
    #[arg(long, global = true)]
    socket_path: Option<std::path::PathBuf>,

    /// Daemon backend: usb (default) or relay (proxy to an upstream usbmuxd, e.g.
    /// Apple's AppleMobileDeviceService on Windows at tcp:127.0.0.1:27015).
    #[arg(long, global = true, default_value = "usb")]
    backend: meridian_relay::config::Backend,

    /// Upstream endpoint for relay mode.
    #[arg(long, global = true)]
    upstream: Option<String>,

    #[arg(long, global = true, default_value = "pretty")]
    log_format: String,
}

#[derive(Subcommand)]
enum Commands {
    List {
        #[arg(short, long, default_value = "json")]
        format: String,

        #[arg(long)]
        json: bool,

        #[arg(long)]
        hide_sensitive: bool,
    },
    Info {
        udid: Option<String>,
        #[arg(long)]
        no_color: bool,
    },
    Watch {
        udid: Option<String>,
    },
    Daemon {
        #[arg(long, default_value = "false")]
        print: bool,

        /// Run under the Service Control Manager (windows only; set by the
        /// installed service itself, not meant for interactive use).
        #[cfg(windows)]
        #[arg(long, default_value = "false", hide = true)]
        service_run: bool,
    },
    Stats {
        #[arg(short, long)]
        count: Option<u32>,
    },
    /// One-command host provisioning: installs drivers/rules and the service.
    /// Safe to re-run; idempotent.
    Setup {
        /// Only provision drivers/rules; skip installing the background service.
        #[arg(long)]
        skip_service: bool,
    },
    /// Manage the Windows service installation (windows only).
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceAction {
    /// Install the SCM service (auto-start).
    Install {
        /// Path to the meridian-relay.exe image; defaults to current exe.
        #[arg(long)]
        bin_path: Option<std::path::PathBuf>,
    },
    /// Remove the SCM service.
    Uninstall,
}

fn resolve_endpoint(cli: &Cli) -> Option<String> {
    cli.endpoint.clone().or_else(|| {
        cli.socket_path.as_ref().map(|p| format!("unix:{}", p.display()))
    })
}

fn make_daemon_args(cli: &Cli) -> DaemonArgs {
    DaemonArgs {
        config_path: None,
        endpoint: cli.endpoint.clone(),
        socket_path: cli.socket_path.clone(),
        socket_mode: None,
        socket_group: None,
        pipe_security: None,
        lockdown_dir: None,
        no_require_pair_record: false,
        scan_interval_ms: None,
        usb_timeout_ms: None,
        connect_timeout_ms: None,
        max_clients: None,
        read_workers: None,
        allow_uids: Vec::new(),
        allow_sids: Vec::new(),
        backend: cli.backend,
        upstream: cli.upstream.clone(),
        log_format: match cli.log_format.as_str() {
            "json" => LogFormat::Json,
            _ => LogFormat::Text,
        },
    }
}

fn init_tracing(kind: &str) {
    let filter = EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into());
    match kind {
        "json" => fmt().with_env_filter(filter).json().init(),
        "compact" => fmt().with_env_filter(filter).compact().init(),
        _ => fmt().with_env_filter(filter).pretty().init(),
    }
}

/// The daemon needs an exit condition: on unix we react to SIGINT/SIGTERM,
/// on windows to console signals (SCM stop is wired in the service module).
fn shutdown_signal() -> impl std::future::Future<Output = ()> + Send {
    platform::wait_for_shutdown_signal()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // The daemon takes its log format from the merged config (file + CLI);
    // interactive commands use the CLI flag.
    if !matches!(cli.command, Commands::Daemon { .. }) {
        init_tracing(&cli.log_format);
    }

    // Resolve the endpoint once, before `cli.command` is moved by the match.
    let endpoint = resolve_endpoint(&cli).unwrap_or_else(platform::default_endpoint);

    match cli.command {
        Commands::List { format, json, hide_sensitive } => {
            let devices = list_devices_from_str(&endpoint).await?;
            if devices.is_empty() {
                println!("no devices found");
                return Ok(());
            }
            if json || format == "json" {
                let output_devices: Vec<_> = devices.iter().map(|d| {
                    let mut dev = d.clone();
                    if hide_sensitive {
                        dev.udid = "redacted".into();
                    }
                    dev
                }).collect();
                println!("{}", serde_json::to_string_pretty(&output_devices)?);
            } else {
                println!("{:<4} {:<40} {:<12} {:<30}", "#", "UDID", "ID", "Model");
                println!("{}", "-".repeat(90));
                for (i, device) in devices.iter().enumerate() {
                    let udid_display = if hide_sensitive {
                        "***".to_string()
                    } else {
                        device.udid.clone()
                    };
                    let model = device.model.as_deref().unwrap_or("?");
                    println!("{:<4} {:<40} {:<12} {:<30}", i + 1, udid_display, device.device_id, model);
                }
            }
        }

        Commands::Info { udid, no_color } => {
            let devices = list_devices_from_str(&endpoint).await?;
            if devices.is_empty() {
                eprintln!("no devices found");
                std::process::exit(1);
            }

            let device = if let Some(ref udid) = udid {
                devices.iter().find(|d| &d.udid == udid).unwrap_or_else(|| {
                    eprintln!("device not found: {udid}");
                    std::process::exit(1);
                })
            } else {
                &devices[0]
            };

            let mut enriched = device.clone();
            let ep = Endpoint::parse(&endpoint)?;
            enrich_device_info(&mut enriched, &ep).await;
            print_device_info(&enriched, no_color);
            if enriched.name.is_none() {
                eprintln!("\n(note: device info missing — if the phone shows \"Trust This Computer?\", tap Trust and retry.)");
            }
        }

        Commands::Watch { ref udid } => {
            match udid {
                Some(u) => println!("watching for device {u}"),
                None => println!("watching all devices"),
            }
            println!("press ctrl-c to stop");
            println!();

            watch_devices_from(&endpoint, udid.as_deref()).await?;
        }

        #[cfg(not(windows))]
        Commands::Daemon { print, .. } => {
            return run_daemon_entry(&cli, print).await;
        }

        #[cfg(windows)]
        Commands::Daemon { print, service_run } => {
            if service_run {
                // Under SCM: dispatch to the service entry point, which runs
                // the daemon inside the SCM-hosted runtime.
                return meridian_relay::service::run_as_service()
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>);
            }
            return run_daemon_entry(&cli, print).await;
        }

        Commands::Setup { skip_service } => {
            println!("provisioning meridian-relay on this host...");
            match meridian_relay::setup::provision(!skip_service) {
                Ok(lines) => {
                    for line in lines {
                        println!("  ✓ {line}");
                    }
                    println!("\nready — start the daemon with: meridian-relay daemon");
                }
                Err(e) => {
                    eprintln!("setup failed: {e}");
                    eprintln!("(hint: this command needs administrator/root privileges)");
                    std::process::exit(1);
                }
            }
        }

        Commands::Stats { count } => {
            let endpoint = Endpoint::parse(&endpoint)?;

            println!("connecting to daemon at {}...", endpoint.display_string());

            let mut stream = endpoint.connect().await?;

            let count = count.unwrap_or(1);
            for i in 0..count {
                let tag = i + 1;
                let packet = meridian_relay::daemon::protocol::make_stats_command(tag);
                meridian_relay::daemon::protocol::write_packet(&mut stream, &packet).await?;

                let response = meridian_relay::daemon::protocol::read_packet(&mut stream, 65536).await?;

                if let Some(json) = response.plist.get("Stats").and_then(|v| v.as_string()) {
                    println!("{json}");
                } else if let Some(status) = response.plist.get("Status").and_then(|v| v.as_unsigned_integer()) {
                    println!("error: status={status}");
                } else {
                    println!("unexpected response: {:?}", response.plist);
                }

                if i < count - 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }

        #[cfg(windows)]
        Commands::Service { action } => {
            match action {
                ServiceAction::Install { bin_path } => {
                    let bin = match bin_path {
                        Some(p) => p,
                        None => std::env::current_exe()?,
                    };
                    meridian_relay::service::install_service(&bin)?;
                    println!("service installed (Start=auto). Start with: sc.exe start meridian-relay");
                }
                ServiceAction::Uninstall => {
                    meridian_relay::service::uninstall_service()?;
                    println!("service uninstalled");
                }
            }
        }
    }

    Ok(())
}

async fn list_devices_from_str(endpoint_str: &str) -> Result<Vec<meridian_relay::device::Device>, Box<dyn std::error::Error>> {
    // Thin shim so CLI paths never need to know about USBMUXD_SOCKET_ADDRESS.
    meridian_relay::device::detect::list_devices_from(endpoint_str).await
}

/// Daemon entry point shared by the OS-specific match arms.
async fn run_daemon_entry(cli: &Cli, print: bool) -> Result<(), Box<dyn std::error::Error>> {
    let args = make_daemon_args(cli);
    let config = DaemonConfig::from_sources(args.config_path.as_deref(), &args)
        .map_err(std::io::Error::other)?;

    // Initialize logging per the merged daemon config.
    init_tracing(match config.log_format {
        LogFormat::Json => "json",
        LogFormat::Text => "pretty",
    });

    config.validate().map_err(std::io::Error::other)?;

    if print {
        println!("endpoint = {}", config.endpoint.display_string());
        println!("backend = {}", config.backend);
        println!("upstream = {}", config.upstream.display_string());
        println!("socket_mode = {:o}", config.socket_mode);
        println!("pipe_security = {}", config.pipe_security);
        println!("lockdown_dir = {:?}", config.lockdown_dir);
        println!("require_pair_record = {}", config.require_pair_record);
        println!("scan_interval = {:?}", config.scan_interval);
        println!("usb_timeout = {:?}", config.usb_timeout);
        println!("connect_timeout = {:?}", config.connect_timeout);
        println!("max_clients = {}", config.max_clients);
        println!("read_workers = {}", config.read_workers);
        println!("max_reassembly_bytes = {}", config.max_reassembly_bytes);
        println!("max_packet_bytes = {}", config.max_packet_bytes);
        println!("max_conn_buffer = {}", config.max_conn_buffer);
        println!("client_read_buf = {}", config.client_read_buf);
        println!("max_data_channel = {}", config.max_data_channel);
        println!("connect_channel = {}", config.connect_channel);
        println!("allowed_uids = {:?}", config.allowed_uids);
        println!("allowed_sids = {:?}", config.allowed_sids);
        println!("log_format = {}", config.log_format);
        return Ok(());
    }

    let metrics = Arc::new(Metrics::new());
    let metrics_log = metrics.clone();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let snapshot = metrics_log.snapshot();
            tracing::info!("metrics: {}", serde_json::to_string(&snapshot).unwrap_or_default());
        }
    });

    match config.backend {
        meridian_relay::config::Backend::Relay => {
            daemon::relay::run_relay(config, metrics, shutdown_signal()).await
        }
        meridian_relay::config::Backend::Usb => {
            daemon::run_daemon(config, metrics, shutdown_signal()).await
        }
    }
}

fn print_device_info(info: &device::Device, _no_color: bool) {
    println!();
    println!("Device Information");
    println!("{}", "-".repeat(60));
    println!("  Name:             {}", info.name.as_deref().unwrap_or("Unknown"));
    println!("  UDID:             {}", info.udid);
    println!("  Connection:       {}", info.connection_type);
    println!("  Model:            {}", info.model.as_deref().unwrap_or("Unknown"));
    println!("  iOS Version:      {}", info.ios_version.as_deref().unwrap_or("Unknown"));
    println!("  Build Version:    {}", info.build_version.as_deref().unwrap_or("Unknown"));
    println!();
}
