use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};
use std::sync::Arc;
use std::path::PathBuf;

use meridian_relay::config::{DaemonConfig, DaemonArgs, LogFormat, DEFAULT_SOCKET_PATH};
use meridian_relay::metrics::Metrics;
use meridian_relay::daemon;
use meridian_relay::device;
use meridian_relay::device::detect::list_devices;
use meridian_relay::device::info::enrich_device_info;
use meridian_relay::device::monitor::watch_devices_from;

#[derive(Parser)]
#[command(name = "meridian-relay")]
#[command(version, about = "Meridian — USB mux relay daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, global = true)]
    socket_path: Option<PathBuf>,

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
    },
    Stats {
        #[arg(short, long)]
        count: Option<u32>,
    },
}

fn make_daemon_args(cli: &Cli) -> DaemonArgs {
    DaemonArgs {
        config_path: None,
        socket_path: cli.socket_path.clone(),
        socket_mode: None,
        socket_group: None,
        lockdown_dir: None,
        no_require_pair_record: false,
        scan_interval_ms: None,
        usb_timeout_ms: None,
        connect_timeout_ms: None,
        max_clients: None,
        read_workers: None,
        allow_uids: Vec::new(),
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // The daemon takes its log format from the merged config (file + CLI);
    // interactive commands use the CLI flag.
    if !matches!(cli.command, Commands::Daemon { .. }) {
        init_tracing(&cli.log_format);
    }

    match cli.command {
        Commands::List { format, json, hide_sensitive } => {
            let devices = list_devices().await?;
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
            let devices = list_devices().await?;
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
            enrich_device_info(&mut enriched).await;
            print_device_info(&enriched, no_color);
        }

        Commands::Watch { ref udid } => {
            match udid {
                Some(u) => println!("watching for device {u}"),
                None => println!("watching all devices"),
            }
            println!("press ctrl-c to stop");
            println!();

            watch_devices_from(DEFAULT_SOCKET_PATH, udid.as_deref()).await?;
        }

        Commands::Daemon { print } => {
            let args = make_daemon_args(&cli);
            let config = DaemonConfig::from_sources(args.config_path.as_deref(), &args)
                .map_err(std::io::Error::other)?;

            // Initialize logging per the merged daemon config.
            init_tracing(match config.log_format {
                LogFormat::Json => "json",
                LogFormat::Text => "pretty",
            });

            config.validate().map_err(std::io::Error::other)?;

            if print {
                println!("socket_path = {:?}", config.socket_path);
                println!("socket_mode = {:o}", config.socket_mode);
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

            daemon::run_daemon(config, metrics).await?;
        }

        Commands::Stats { count } => {
            let args = make_daemon_args(&cli);
            let config = DaemonConfig::from_sources(args.config_path.as_deref(), &args)
                .map_err(std::io::Error::other)?;

            let socket_path = cli.socket_path.clone().unwrap_or(config.socket_path.clone());

            println!("connecting to daemon at {}...", socket_path.display());

            let stream = tokio::net::UnixStream::connect(&socket_path).await?;
            let mut stream = stream;

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
    }

    Ok(())
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
