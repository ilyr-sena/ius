//! Relay backend — the zero-driver mode.
//!
//! Instead of owning USB devices directly, the daemon transparently splices
//! client connections to an upstream usbmuxd-compatible service (on Windows
//! that is Apple's free "Apple Mobile Device Service" at 127.0.0.1:27015,
//! shipped with iTunes / the "Apple Devices" Store app — both free).
//!
//! The relay is protocol-transparent: every usbmuxd message (ListDevices,
//! Listen, Connect, pair-record ops, raw data channels) passes through
//! byte-for-byte. We measure bytes and enforce the same client limits and
//! peer allowlists as the USB mode.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::config::DaemonConfig;
use crate::metrics::Metrics;
use crate::platform;
use super::transport::{Endpoint, TransportListener, TransportStream};

/// Run the daemon in relay mode. Same listen endpoint, same shutdown
/// semantics as the USB backend.
pub async fn run_relay(
    config: DaemonConfig,
    metrics: Arc<Metrics>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), Box<dyn std::error::Error>> {
    config.validate().map_err(std::io::Error::other)?;
    tokio::pin!(shutdown);

    // Stale unix socket cleanup + filesystem security; named pipes get their
    // DACL atomically inside TransportListener::bind.
    #[cfg(unix)]
    let _socket_guard = {
        if let Endpoint::Unix(ref path) = config.endpoint {
            if path.exists() {
                std::fs::remove_file(path)?;
                info!("removed existing socket: {}", path.display());
            }
            Some(path.clone())
        } else {
            None
        }
    };

    let mut listener = TransportListener::bind(&config.endpoint, &config.pipe_security).await?;

    #[cfg(unix)]
    let _socket_guard = _socket_guard.map(|path| {
        platform::apply_endpoint_security(&path, config.socket_mode, config.resolve_group_gid())
            .expect("failed to secure unix socket");
        crate::security::SocketCleanupGuard::new(path, true)
    });
    info!(
        "relay mode: listening on {} → upstream {}",
        config.endpoint.display_string(),
        config.upstream.display_string()
    );
    platform::notify_service_ready();

    let semaphore = Arc::new(Semaphore::new(config.max_clients));

    loop {
        let (stream, peer) = tokio::select! {
            res = listener.accept() => match res {
                Ok(pair) => pair,
                Err(e) => {
                    debug!("accept error (continuing): {e}");
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                metrics.clients_rejected.fetch_add(1, Ordering::Relaxed);
                drop(stream);
                continue;
            }
        };

        let upstream = config.upstream.clone();
        let metrics = metrics.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let _permit = permit;
            handle_relay_client(stream, peer, upstream, metrics, config).await;
        });
    }

    drop(listener);
    info!("relay daemon stopped");
    Ok(())
}

async fn handle_relay_client(
    mut client: TransportStream,
    peer: crate::platform::PeerIdentity,
    upstream: Endpoint,
    metrics: Arc<Metrics>,
    config: DaemonConfig,
) {
    metrics.clients_accepted.fetch_add(1, Ordering::Relaxed);
    metrics.clients_active.fetch_add(1, Ordering::Relaxed);

    if !crate::daemon::connection::peer_is_allowed(&peer, &config) {
        warn!("relay: rejecting client, not in allowlist");
        metrics.clients_rejected.fetch_add(1, Ordering::Relaxed);
        metrics.clients_active.fetch_sub(1, Ordering::Relaxed);
        return;
    }

    let mut up = match upstream.connect().await {
        Ok(s) => s,
        Err(e) => {
            warn!("relay: upstream {} unreachable: {e}", upstream.display_string());
            metrics.clients_active.fetch_sub(1, Ordering::Relaxed);
            return;
        }
    };

    metrics.connects_total.fetch_add(1, Ordering::Relaxed);
    debug!("relay: splicing client to {}", upstream.display_string());

    let result = splice(&mut client, &mut up, &metrics).await;

    metrics.clients_active.fetch_sub(1, Ordering::Relaxed);
    if let Err(e) = result {
        debug!("relay: connection ended: {e}");
    }
}

/// Bidirectional byte-splice with byte-level metrics accounting.
async fn splice(
    a: &mut TransportStream,
    b: &mut TransportStream,
    metrics: &Arc<Metrics>,
) -> std::io::Result<()> {
    let (a_to_b, b_to_a) = tokio::io::copy_bidirectional(a, b).await?;
    metrics.client_rx_bytes.fetch_add(a_to_b, Ordering::Relaxed);
    metrics.client_tx_bytes.fetch_add(b_to_a, Ordering::Relaxed);
    Ok(())
}
