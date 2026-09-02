use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde::Deserialize;
use tracing::warn;

use crate::daemon::transport::Endpoint;
use crate::platform;

const DEFAULT_SCAN_INTERVAL_MS: u64 = 2000;
const DEFAULT_USB_TIMEOUT_MS: u64 = 5000;
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5000;
const DEFAULT_MAX_CLIENTS: usize = 256;
const DEFAULT_BROADCAST_CAPACITY: usize = 256;
const DEFAULT_READ_WORKERS: usize = 3;
const DEFAULT_MAX_REASSEMBLY_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CONN_BUFFER: usize = 1024 * 1024;
const DEFAULT_CLIENT_READ_BUF: usize = 16384;
const DEFAULT_MAX_DATA_CHANNEL: usize = 128;
const DEFAULT_CONNECT_CHANNEL: usize = 16;
const DEFAULT_SOCKET_MODE: u32 = 0o666;
const MAX_CONFIG_FILE_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Args)]
pub struct DaemonArgs {
    /// Path to optional TOML config file
    #[arg(long = "config")]
    pub config_path: Option<PathBuf>,

    /// Daemon endpoint: unix:/path, pipe:name, or tcp:127.0.0.1:port
    #[arg(short, long)]
    pub endpoint: Option<String>,

    /// [unix-only, legacy] unix socket path — prefer --endpoint
    #[arg(long = "socket-path")]
    pub socket_path: Option<PathBuf>,

    /// Socket file permissions (octal, e.g. "0660") — unix only
    #[arg(long = "socket-mode")]
    pub socket_mode: Option<String>,

    /// Socket owning group name — unix only (maps to an ACL group on windows)
    #[arg(long = "socket-group")]
    pub socket_group: Option<String>,

    /// [windows] SDDL string for the named pipe DACL
    #[arg(long = "pipe-security")]
    pub pipe_security: Option<String>,

    /// Pair record directory path
    #[arg(long = "lockdown-dir")]
    pub lockdown_dir: Option<PathBuf>,

    /// Disable pair record requirement for lockdown connections
    #[arg(long = "no-require-pair-record")]
    pub no_require_pair_record: bool,

    /// USB scan interval in milliseconds
    #[arg(long = "scan-interval-ms")]
    pub scan_interval_ms: Option<u64>,

    /// USB I/O timeout in milliseconds
    #[arg(long = "usb-timeout-ms")]
    pub usb_timeout_ms: Option<u64>,

    /// TCP connect timeout in milliseconds
    #[arg(long = "connect-timeout-ms")]
    pub connect_timeout_ms: Option<u64>,

    /// Maximum concurrent client connections
    #[arg(long = "max-clients")]
    pub max_clients: Option<usize>,

    /// (deprecated; ordering requires exactly one USB reader)
    #[arg(long = "read-workers")]
    pub read_workers: Option<usize>,

    /// Allowlist of UIDs, unix only (empty = allow all). Comma-separated.
    #[arg(long = "allow-uid", value_delimiter = ',')]
    pub allow_uids: Vec<u32>,

    /// Allowlist of Windows user/group SIDs (empty = allow all). Comma-separated.
    #[arg(long = "allow-sid", value_delimiter = ',')]
    pub allow_sids: Vec<String>,

    /// Daemon mode: "usb" = own the USB device directly (default),
    /// "relay" = transparently proxy to another usbmuxd-compatible service
    #[arg(long = "backend", default_value = "usb")]
    pub backend: Backend,

    /// [relay mode] Where the upstream service listens (default 127.0.0.1:27015)
    #[arg(long = "upstream")]
    pub upstream: Option<String>,

    /// Log output format: text or json
    #[arg(long = "log-format", default_value = "text")]
    pub log_format: LogFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Usb,
    Relay,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Usb => write!(f, "usb"),
            Backend::Relay => write!(f, "relay"),
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Text => write!(f, "text"),
            LogFormat::Json => write!(f, "json"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    pub endpoint: Option<String>,
    pub socket_path: Option<PathBuf>,
    pub socket_mode: Option<String>,
    pub socket_group: Option<String>,
    pub pipe_security: Option<String>,
    pub lockdown_dir: Option<PathBuf>,
    pub require_pair_record: Option<bool>,
    pub scan_interval_ms: Option<u64>,
    pub usb_timeout_ms: Option<u64>,
    pub connect_timeout_ms: Option<u64>,
    pub max_clients: Option<usize>,
    pub read_workers: Option<usize>,
    pub allowed_uids: Option<Vec<u32>>,
    pub allowed_sids: Option<Vec<String>>,
    pub backend: Option<Backend>,
    pub upstream: Option<String>,
    pub log_format: Option<LogFormat>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            endpoint: None,
            socket_path: None,
            socket_mode: None,
            socket_group: None,
            pipe_security: None,
            lockdown_dir: None,
            require_pair_record: None,
            scan_interval_ms: None,
            usb_timeout_ms: None,
            connect_timeout_ms: None,
            max_clients: None,
            read_workers: None,
            allowed_uids: None,
            allowed_sids: None,
            backend: None,
            upstream: None,
            log_format: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Primary endpoint the daemon listens on.
    pub endpoint: Endpoint,
    /// Unparsed endpoint string, kept for `--print` and diagnostics.
    pub endpoint_raw: Option<String>,
    /// Unix socket path (mirror of `endpoint` when it is a unix socket; kept
    /// for status printing and socket cleanup).
    pub socket_path: PathBuf,
    pub socket_mode: u32,
    pub socket_group: Option<String>,
    pub pipe_security: String,
    pub lockdown_dir: PathBuf,
    pub require_pair_record: bool,
    pub scan_interval: Duration,
    pub usb_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_clients: usize,
    pub broadcast_capacity: usize,
    pub read_workers: usize,
    pub max_reassembly_bytes: usize,
    pub max_packet_bytes: usize,
    pub max_conn_buffer: usize,
    pub client_read_buf: usize,
    pub max_data_channel: usize,
    pub connect_channel: usize,
    pub allowed_uids: Vec<u32>,
    pub allowed_sids: Vec<String>,
    pub backend: Backend,
    pub upstream: Endpoint,
    /// Unparsed upstream string (pre-merge staging, like endpoint_raw).
    pub upstream_raw: Option<String>,
    pub log_format: LogFormat,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            endpoint: Endpoint::parse(&platform::default_endpoint())
                .expect("built-in default endpoint must parse"),
            endpoint_raw: None,
            socket_path: PathBuf::from(platform::default_endpoint()),
            socket_mode: DEFAULT_SOCKET_MODE,
            socket_group: None,
            pipe_security: platform::default_pipe_security(),
            lockdown_dir: platform::default_lockdown_dir(),
            require_pair_record: true,
            scan_interval: Duration::from_millis(DEFAULT_SCAN_INTERVAL_MS),
            usb_timeout: Duration::from_millis(DEFAULT_USB_TIMEOUT_MS),
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            max_clients: DEFAULT_MAX_CLIENTS,
            broadcast_capacity: DEFAULT_BROADCAST_CAPACITY,
            read_workers: DEFAULT_READ_WORKERS,
            max_reassembly_bytes: DEFAULT_MAX_REASSEMBLY_BYTES,
            max_packet_bytes: DEFAULT_MAX_PACKET_BYTES,
            max_conn_buffer: DEFAULT_MAX_CONN_BUFFER,
            client_read_buf: DEFAULT_CLIENT_READ_BUF,
            max_data_channel: DEFAULT_MAX_DATA_CHANNEL,
            connect_channel: DEFAULT_CONNECT_CHANNEL,
            allowed_uids: Vec::new(),
            allowed_sids: Vec::new(),
            backend: Backend::Usb,
            upstream: Endpoint::parse("tcp:127.0.0.1:27015").expect("default upstream must parse"),
            upstream_raw: None,
            log_format: LogFormat::Text,
        }
    }
}

impl DaemonConfig {
    pub fn from_sources(config_path: Option<&std::path::Path>, args: &DaemonArgs) -> Result<Self, String> {
        let mut cfg = Self::default();

        // Layer 1: TOML config file (explicit path, else platform default if it exists)
        let path = config_path
            .map(|p| p.to_path_buf())
            .or_else(|| {
                let d = platform::default_config_path();
                if d.exists() { Some(d) } else { None }
            });
        if let Some(path) = path {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read config file {}: {e}", path.display()))?;
            if content.len() > MAX_CONFIG_FILE_SIZE {
                return Err(format!("config file too large: {} bytes (max {})", content.len(), MAX_CONFIG_FILE_SIZE));
            }
            let file_cfg: ConfigFile = toml::from_str(&content)
                .map_err(|e| format!("failed to parse config file: {e}"))?;
            cfg.merge_file(&file_cfg);
        }

        // Layer 2: CLI arguments (override file)
        cfg.merge_args(args);

        cfg.finalize_endpoint()?;
        Ok(cfg)
    }

    fn merge_file(&mut self, file: &ConfigFile) {
        if let Some(ref e) = file.endpoint {
            self.endpoint_raw = Some(e.clone());
        }
        if let Some(ref p) = file.socket_path {
            self.socket_path = p.clone();
            self.endpoint_raw = Some(format!("unix:{}", p.display()));
        }
        if let Some(ref m) = file.socket_mode {
            match parse_octal(m) {
                Ok(mode) => self.socket_mode = mode,
                Err(e) => warn!("invalid socket_mode in config: {e}"),
            }
        }
        if let Some(ref g) = file.socket_group {
            self.socket_group = Some(g.clone());
        }
        if let Some(ref s) = file.pipe_security {
            self.pipe_security = s.clone();
        }
        if let Some(ref p) = file.lockdown_dir {
            self.lockdown_dir = p.clone();
        }
        if let Some(v) = file.require_pair_record {
            self.require_pair_record = v;
        }
        if let Some(v) = file.scan_interval_ms {
            self.scan_interval = Duration::from_millis(v);
        }
        if let Some(v) = file.usb_timeout_ms {
            self.usb_timeout = Duration::from_millis(v);
        }
        if let Some(v) = file.connect_timeout_ms {
            self.connect_timeout = Duration::from_millis(v);
        }
        if let Some(v) = file.max_clients {
            self.max_clients = v;
        }
        if let Some(v) = file.read_workers {
            self.read_workers = v;
        }
        if let Some(ref uids) = file.allowed_uids {
            self.allowed_uids = uids.clone();
        }
        if let Some(ref sids) = file.allowed_sids {
            self.allowed_sids = sids.clone();
        }
        if let Some(b) = file.backend {
            self.backend = b;
        }
        if let Some(ref u) = file.upstream {
            self.upstream_raw = Some(u.clone());
        }
        if let Some(f) = file.log_format {
            self.log_format = f;
        }
    }

    fn merge_args(&mut self, args: &DaemonArgs) {
        if let Some(ref e) = args.endpoint {
            self.endpoint_raw = Some(e.clone());
        }
        if let Some(ref p) = args.socket_path {
            self.socket_path = p.clone();
            self.endpoint_raw = Some(format!("unix:{}", p.display()));
        }
        if let Some(ref m) = args.socket_mode {
            match parse_octal(m) {
                Ok(mode) => self.socket_mode = mode,
                Err(e) => warn!("invalid socket-mode argument: {e}"),
            }
        }
        if let Some(ref g) = args.socket_group {
            self.socket_group = Some(g.clone());
        }
        if let Some(ref s) = args.pipe_security {
            self.pipe_security = s.clone();
        }
        if let Some(ref p) = args.lockdown_dir {
            self.lockdown_dir = p.clone();
        }
        if args.no_require_pair_record {
            self.require_pair_record = false;
        }
        if let Some(v) = args.scan_interval_ms {
            self.scan_interval = Duration::from_millis(v);
        }
        if let Some(v) = args.usb_timeout_ms {
            self.usb_timeout = Duration::from_millis(v);
        }
        if let Some(v) = args.connect_timeout_ms {
            self.connect_timeout = Duration::from_millis(v);
        }
        if let Some(v) = args.max_clients {
            self.max_clients = v;
        }
        if let Some(v) = args.read_workers {
            self.read_workers = v;
        }
        if !args.allow_uids.is_empty() {
            self.allowed_uids = args.allow_uids.clone();
        }
        if !args.allow_sids.is_empty() {
            self.allowed_sids = args.allow_sids.clone();
        }
        self.backend = args.backend;
        if let Some(ref u) = args.upstream {
            self.upstream_raw = Some(u.clone());
        }
        self.log_format = args.log_format;
    }

    /// Resolve the endpoint_raw string into the typed `endpoint` field.
    fn finalize_endpoint(&mut self) -> Result<(), String> {
        if let Some(ref raw) = self.endpoint_raw {
            self.endpoint = Endpoint::parse(raw)?;
            if let Endpoint::Unix(ref p) = self.endpoint {
                self.socket_path = p.clone();
            }
        }
        if let Some(ref raw) = self.upstream_raw {
            self.upstream = Endpoint::parse(raw)?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_clients == 0 {
            return Err("max_clients must be > 0".into());
        }
        if self.read_workers == 0 {
            return Err("read_workers must be > 0".into());
        }
        if self.scan_interval.is_zero() {
            return Err("scan_interval must be > 0".into());
        }
        if self.usb_timeout.is_zero() {
            return Err("usb_timeout must be > 0".into());
        }
        if self.connect_timeout.is_zero() {
            return Err("connect_timeout must be > 0".into());
        }
        if self.max_packet_bytes < 1024 {
            return Err("max_packet_bytes must be >= 1024".into());
        }
        if self.max_conn_buffer == 0 {
            return Err("max_conn_buffer must be > 0".into());
        }
        if self.socket_mode > 0o777 {
            return Err(format!("invalid socket mode: {:o}", self.socket_mode));
        }
        if self.socket_mode & 0o002 != 0 && matches!(self.endpoint, Endpoint::Unix(_)) {
            warn!(
                "socket mode {:o} is world-writable — consider restricting for production",
                self.socket_mode
            );
        }
        if self.socket_mode & 0o004 != 0 && matches!(self.endpoint, Endpoint::Unix(_)) {
            warn!(
                "socket mode {:o} is world-readable — consider restricting for production",
                self.socket_mode
            );
        }
        if self.require_pair_record && !self.lockdown_dir.exists() {
            warn!(
                "lockdown directory {} does not exist — pair record enforcement may fail",
                self.lockdown_dir.display()
            );
        }
        Ok(())
    }

    pub fn resolve_group_gid(&self) -> Option<u32> {
        let group_name = self.socket_group.as_ref()?;
        if let Ok(content) = std::fs::read_to_string("/etc/group") {
            for line in content.lines() {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 3 && parts[0] == group_name {
                    if let Ok(gid) = parts[2].parse::<u32>() {
                        return Some(gid);
                    }
                }
            }
        }
        warn!("could not resolve group '{}' to GID", group_name);
        None
    }
}

pub fn parse_octal(s: &str) -> Result<u32, String> {
    let s = s.trim().trim_start_matches("0o").trim_start_matches("0");
    u32::from_str_radix(s, 8).map_err(|e| format!("invalid octal value '{s}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_octal() {
        assert_eq!(parse_octal("0666").unwrap(), 0o666);
        assert_eq!(parse_octal("0o660").unwrap(), 0o660);
        assert_eq!(parse_octal("600").unwrap(), 0o600);
        assert_eq!(parse_octal("0755").unwrap(), 0o755);
        assert!(parse_octal("invalid").is_err());
    }

    #[test]
    fn test_defaults() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.socket_mode, 0o666);
        assert!(cfg.require_pair_record);
        assert_eq!(cfg.max_clients, 256);
        assert_eq!(cfg.read_workers, 3);
        assert!(cfg.allowed_uids.is_empty());
    }

    #[test]
    fn test_endpoint_unix_scheme() {
        let mut cfg = DaemonConfig::default();
        cfg.endpoint_raw = Some("unix:/tmp/foo.sock".into());
        cfg.finalize_endpoint().unwrap();
        assert!(matches!(cfg.endpoint, Endpoint::Unix(_)));
        assert_eq!(cfg.socket_path, PathBuf::from("/tmp/foo.sock"));
    }

    #[test]
    fn test_validate_rejects_zero_max_clients() {
        let mut cfg = DaemonConfig::default();
        cfg.max_clients = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_read_workers() {
        let mut cfg = DaemonConfig::default();
        cfg.read_workers = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_invalid_mode() {
        let mut cfg = DaemonConfig::default();
        cfg.socket_mode = 0o1000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_timeouts() {
        let mut cfg = DaemonConfig::default();
        cfg.scan_interval = Duration::ZERO;
        assert!(cfg.validate().is_err());

        let mut cfg = DaemonConfig::default();
        cfg.usb_timeout = Duration::ZERO;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_tiny_packet_cap() {
        let mut cfg = DaemonConfig::default();
        cfg.max_packet_bytes = 512;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_defaults_ok() {
        assert!(DaemonConfig::default().validate().is_ok());
    }
}
