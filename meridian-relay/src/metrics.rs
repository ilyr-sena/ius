use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug)]
pub struct Metrics {
    pub start_time: Instant,
    pub clients_accepted: AtomicU64,
    pub clients_rejected: AtomicU64,
    pub clients_active: AtomicI64,
    pub commands_total: AtomicU64,
    pub list_devices_total: AtomicU64,
    pub listens_active: AtomicI64,
    pub connects_total: AtomicU64,
    pub connect_failures: AtomicU64,
    pub proxies_started: AtomicU64,
    pub proxies_ended: AtomicU64,
    pub usb_tx_bytes: AtomicU64,
    pub usb_rx_bytes: AtomicU64,
    pub client_tx_bytes: AtomicU64,
    pub client_rx_bytes: AtomicU64,
    pub devices_attached: AtomicI64,
    pub parse_errors: AtomicU64,
    pub pair_reads: AtomicU64,
    pub pair_writes: AtomicU64,
    pub pair_deletes: AtomicU64,
    pub rsts_received: AtomicU64,
    pub overflow_rejections: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            clients_accepted: AtomicU64::new(0),
            clients_rejected: AtomicU64::new(0),
            clients_active: AtomicI64::new(0),
            commands_total: AtomicU64::new(0),
            list_devices_total: AtomicU64::new(0),
            listens_active: AtomicI64::new(0),
            connects_total: AtomicU64::new(0),
            connect_failures: AtomicU64::new(0),
            proxies_started: AtomicU64::new(0),
            proxies_ended: AtomicU64::new(0),
            usb_tx_bytes: AtomicU64::new(0),
            usb_rx_bytes: AtomicU64::new(0),
            client_tx_bytes: AtomicU64::new(0),
            client_rx_bytes: AtomicU64::new(0),
            devices_attached: AtomicI64::new(0),
            parse_errors: AtomicU64::new(0),
            pair_reads: AtomicU64::new(0),
            pair_writes: AtomicU64::new(0),
            pair_deletes: AtomicU64::new(0),
            rsts_received: AtomicU64::new(0),
            overflow_rejections: AtomicU64::new(0),
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            uptime_secs: self.uptime_secs(),
            clients_accepted: self.clients_accepted.load(Ordering::Relaxed),
            clients_rejected: self.clients_rejected.load(Ordering::Relaxed),
            clients_active: self.clients_active.load(Ordering::Relaxed),
            commands_total: self.commands_total.load(Ordering::Relaxed),
            list_devices_total: self.list_devices_total.load(Ordering::Relaxed),
            listens_active: self.listens_active.load(Ordering::Relaxed),
            connects_total: self.connects_total.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            proxies_started: self.proxies_started.load(Ordering::Relaxed),
            proxies_ended: self.proxies_ended.load(Ordering::Relaxed),
            usb_tx_bytes: self.usb_tx_bytes.load(Ordering::Relaxed),
            usb_rx_bytes: self.usb_rx_bytes.load(Ordering::Relaxed),
            client_tx_bytes: self.client_tx_bytes.load(Ordering::Relaxed),
            client_rx_bytes: self.client_rx_bytes.load(Ordering::Relaxed),
            devices_attached: self.devices_attached.load(Ordering::Relaxed),
            parse_errors: self.parse_errors.load(Ordering::Relaxed),
            pair_reads: self.pair_reads.load(Ordering::Relaxed),
            pair_writes: self.pair_writes.load(Ordering::Relaxed),
            pair_deletes: self.pair_deletes.load(Ordering::Relaxed),
            rsts_received: self.rsts_received.load(Ordering::Relaxed),
            overflow_rejections: self.overflow_rejections.load(Ordering::Relaxed),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.snapshot()).unwrap_or_default()
    }
}

#[derive(Debug, serde::Serialize)]
pub struct StatsSnapshot {
    pub uptime_secs: u64,
    pub clients_accepted: u64,
    pub clients_rejected: u64,
    pub clients_active: i64,
    pub commands_total: u64,
    pub list_devices_total: u64,
    pub listens_active: i64,
    pub connects_total: u64,
    pub connect_failures: u64,
    pub proxies_started: u64,
    pub proxies_ended: u64,
    pub usb_tx_bytes: u64,
    pub usb_rx_bytes: u64,
    pub client_tx_bytes: u64,
    pub client_rx_bytes: u64,
    pub devices_attached: i64,
    pub parse_errors: u64,
    pub pair_reads: u64,
    pub pair_writes: u64,
    pub pair_deletes: u64,
    pub rsts_received: u64,
    pub overflow_rejections: u64,
}

#[derive(Debug, Clone)]
pub struct ListenGuard {
    metrics: std::sync::Arc<Metrics>,
}

impl ListenGuard {
    pub fn new(metrics: &std::sync::Arc<Metrics>) -> Self {
        metrics.listens_active.fetch_add(1, Ordering::Relaxed);
        Self { metrics: metrics.clone() }
    }
}

impl Drop for ListenGuard {
    fn drop(&mut self) {
        self.metrics.listens_active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_snapshot() {
        let m = Metrics::new();
        m.clients_accepted.fetch_add(10, Ordering::Relaxed);
        m.clients_active.fetch_add(3, Ordering::Relaxed);
        let snap = m.snapshot();
        assert_eq!(snap.clients_accepted, 10);
        assert_eq!(snap.clients_active, 3);
        assert_eq!(snap.uptime_secs, 0);
    }

    #[test]
    fn test_listen_guard() {
        let m = std::sync::Arc::new(Metrics::new());
        assert_eq!(m.listens_active.load(Ordering::Relaxed), 0);
        {
            let _g = ListenGuard::new(&m);
            assert_eq!(m.listens_active.load(Ordering::Relaxed), 1);
        }
        assert_eq!(m.listens_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_to_json() {
        let m = Metrics::new();
        m.devices_attached.fetch_add(2, Ordering::Relaxed);
        let json = m.to_json();
        assert!(json.contains("\"devices_attached\": 2"));
    }
}
