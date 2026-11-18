//! The passive listener: record the `version` of every node that dials us.
//!
//! Most nodes sit behind NAT or a firewall and never accept connections, so
//! the crawler cannot reach them. They do dial out to addresses they learn
//! from gossip. The crawler announces our address to every peer it reaches;
//! nodes that later dial it are answered here, recorded, and hung up on.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::crawler::observation;
use crate::db::{self, AddressSighting, Db, InboundObservation};
use crate::netaddr;
use crate::peer::{self, ProbeConfig, ProbeResult};

const FLUSH_EVERY_ROWS: usize = 100;
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// Pause after `accept` fails (for example, out of file descriptors).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct ListenerConfig {
    pub probe: Arc<ProbeConfig>,
    pub max_connections: usize,
    /// Add non-global claimed addresses to the address book. For tests only.
    pub allow_non_routable: bool,
}

pub async fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding listener to {addr}"))
}

/// Accept connections until `stop` turns true, then write what is pending.
pub async fn serve(
    listener: TcpListener,
    cfg: ListenerConfig,
    db: Db,
    mut stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut tasks: JoinSet<ProbeResult> = JoinSet::new();
    let mut pending: Vec<ProbeResult> = Vec::new();
    let mut last_flush = Instant::now();
    let mut stop_open = true;
    loop {
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    if tasks.len() >= cfg.max_connections {
                        debug!(%peer, "inbound connection limit reached; dropping");
                        drop(stream);
                    } else {
                        let pc = Arc::clone(&cfg.probe);
                        let peer = netaddr::normalize(peer);
                        tasks.spawn(async move { peer::answer(stream, peer, &pc).await });
                    }
                }
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    sleep(ACCEPT_BACKOFF).await;
                }
            },
            joined = tasks.join_next(), if !tasks.is_empty() => match joined {
                Some(Ok(result)) => {
                    debug!(
                        peer = %result.addr,
                        handshake_complete = result.handshake_complete,
                        failure = result.failure,
                        "inbound"
                    );
                    pending.push(result);
                }
                Some(Err(e)) => error!(error = %e, "inbound task failed"),
                None => {}
            },
            _ = sleep(FLUSH_EVERY) => {}
            changed = stop.changed(), if stop_open => {
                stop_open = changed.is_ok();
            }
        }
        if pending.len() >= FLUSH_EVERY_ROWS
            || (!pending.is_empty() && last_flush.elapsed() >= FLUSH_EVERY)
        {
            flush(&db, &mut pending, cfg.allow_non_routable).await?;
            last_flush = Instant::now();
        }
    }
    // Handshakes in flight are cut off; they were never going to be counted.
    tasks.shutdown().await;
    flush(&db, &mut pending, cfg.allow_non_routable).await?;
    info!("listener stopped");
    Ok(())
}

async fn flush(
    db: &Db,
    pending: &mut Vec<ProbeResult>,
    allow_non_routable: bool,
) -> anyhow::Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let results = std::mem::take(pending);
    let mut rows = Vec::with_capacity(results.len());
    let mut sightings = Vec::new();
    for r in &results {
        let (row, sighting) = inbound_row(r, allow_non_routable);
        rows.push(row);
        sightings.extend(sighting);
    }
    db.call(move |c| db::record_inbound(c, &rows, &sightings))
        .await
}

/// The row for one inbound connection, and the listening address it suggests.
///
/// Like Zebra, we trust the connection's source IP but take the port from
/// the peer's `addr_from`. A peer that really listens there will be reached
/// by the next round. Most report `[::]:8233` or `0.0.0.0:0`.
pub fn inbound_row(
    r: &ProbeResult,
    allow_non_routable: bool,
) -> (InboundObservation, Option<AddressSighting>) {
    let claimed = r.version.as_ref().map(|v| v.addr_from.addr);
    let sighting = (r.handshake_complete)
        .then_some(claimed)
        .flatten()
        .filter(|c| c.port() != 0)
        .map(|c| SocketAddr::new(r.addr.ip(), c.port()))
        .filter(|a| allow_non_routable || netaddr::is_crawlable(*a))
        .map(|addr| AddressSighting {
            addr,
            seen_at: r.observed_at,
            reachable: false,
        });
    let row = InboundObservation {
        obs: observation(r),
        claimed_addr: claimed,
    };
    (row, sighting)
}
