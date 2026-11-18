//! Crawl rounds and the `run` loop.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, TryLockError};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, sleep_until, timeout};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::db::{self, AddressSighting, Db, Observation};
use crate::netaddr;
use crate::params;
use crate::peer::{self, ProbeConfig, ProbeResult, unix_now};
use crate::user_agent::parse_user_agent;

const DNS_TIMEOUT: Duration = Duration::from_secs(15);
const FLUSH_EVERY_ROWS: usize = 500;
const FLUSH_EVERY: Duration = Duration::from_secs(5);

/// How a round ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundStatus {
    Complete { hit_time_limit: bool },
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct RoundSummary {
    pub round_id: i64,
    pub status: RoundStatus,
    pub addresses_tried: u64,
    pub nodes_reached: u64,
    pub duration: Duration,
}

/// `zcrawl run`: crawl rounds on a timer until SIGINT or SIGTERM.
pub async fn run(cfg: Config, once: bool) -> anyhow::Result<()> {
    let _lock = lock_database(&cfg.database)?;
    let conn = db::open(&cfg.database)?;
    db::check_network(&conn, cfg.network)?;
    let stale = db::interrupt_stale_rounds(&conn)?;
    if stale > 0 {
        warn!(
            rounds = stale,
            "marked rounds left running by an earlier process as interrupted"
        );
    }
    let db = Db::new(conn);

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        info!("shutting down");
        let _ = stop_tx.send(true);
    });

    info!(
        network = cfg.network.as_str(),
        database = %cfg.database.display(),
        user_agent = params::user_agent(),
        protocol_version = cfg.protocol_version(),
        "zcrawl starting"
    );

    let mut ticker = tokio::time::interval(cfg.round_interval());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stop = stop_rx.clone();
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            break;
        }
        match run_round(&db, &cfg, stop_rx.clone()).await {
            Ok(s) => info!(
                round_id = s.round_id,
                status = ?s.status,
                addresses_tried = s.addresses_tried,
                nodes_reached = s.nodes_reached,
                duration_secs = s.duration.as_secs(),
                "round finished"
            ),
            // A failed round is already marked interrupted; try again next tick.
            Err(e) => error!(error = format!("{e:#}"), "round failed"),
        }
        if once || *stop.borrow() {
            break;
        }
    }
    Ok(())
}

/// Hold an exclusive lock on `<database>.lock` so two `run` processes cannot
/// share a database (each would mark the other's round as interrupted).
fn lock_database(database: &std::path::Path) -> anyhow::Result<File> {
    let mut path = PathBuf::from(database.as_os_str());
    path.as_mut_os_string().push(".lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("another `zcrawl run` holds {}", path.display())
        }
        Err(TryLockError::Error(e)) => Err(e).context("locking database"),
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                warn!(error = %e, "cannot listen for SIGTERM");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

enum CrawlEnd {
    Done { hit_time_limit: bool },
    Stopped,
}

/// Run one round and record it. On error or shutdown the round is left
/// `interrupted`, so reports never see it.
pub async fn run_round(
    db: &Db,
    cfg: &Config,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<RoundSummary> {
    let started = Instant::now();
    let started_at = unix_now();
    let round_id = db.call(move |c| db::start_round(c, started_at)).await?;
    info!(round_id, "round started");

    let end = crawl(
        db,
        cfg,
        round_id,
        started_at,
        started + cfg.round_time_limit(),
        stop,
    )
    .await;
    match end {
        Ok(CrawlEnd::Done { hit_time_limit }) => {
            let finished_at = unix_now();
            let (tried, reached) = db
                .call(move |c| db::finish_round(c, round_id, finished_at, hit_time_limit))
                .await?;
            Ok(RoundSummary {
                round_id,
                status: RoundStatus::Complete { hit_time_limit },
                addresses_tried: tried,
                nodes_reached: reached,
                duration: started.elapsed(),
            })
        }
        Ok(CrawlEnd::Stopped) => {
            db.call(move |c| db::mark_interrupted(c, round_id)).await?;
            Ok(RoundSummary {
                round_id,
                status: RoundStatus::Interrupted,
                addresses_tried: 0,
                nodes_reached: 0,
                duration: started.elapsed(),
            })
        }
        Err(e) => {
            if let Err(e2) = db.call(move |c| db::mark_interrupted(c, round_id)).await {
                error!(
                    error = format!("{e2:#}"),
                    round_id, "could not mark round interrupted"
                );
            }
            Err(e)
        }
    }
}

/// Addresses waiting to be crawled and everything already queued this round.
struct Frontier {
    allow_non_routable: bool,
    seen: HashSet<SocketAddr>,
    queue: VecDeque<SocketAddr>,
}

impl Frontier {
    fn accepts(&self, addr: SocketAddr) -> bool {
        self.allow_non_routable || netaddr::is_crawlable(addr)
    }

    /// Queue `addr` unless it was already queued this round.
    fn push(&mut self, addr: SocketAddr) {
        let addr = netaddr::normalize(addr);
        if self.accepts(addr) && self.seen.insert(addr) {
            self.queue.push_back(addr);
        }
    }
}

/// Rows waiting to be written.
#[derive(Default)]
struct Pending {
    observations: Vec<Observation>,
    sightings: HashMap<SocketAddr, AddressSighting>,
}

impl Pending {
    fn sight(&mut self, addr: SocketAddr, seen_at: i64, reachable: bool) {
        let e = self.sightings.entry(addr).or_insert(AddressSighting {
            addr,
            seen_at,
            reachable,
        });
        e.seen_at = e.seen_at.max(seen_at);
        e.reachable |= reachable;
    }

    fn len(&self) -> usize {
        self.observations.len() + self.sightings.len()
    }

    async fn flush(&mut self, db: &Db, round_id: i64) -> anyhow::Result<()> {
        if self.len() == 0 {
            return Ok(());
        }
        let observations = std::mem::take(&mut self.observations);
        let sightings: Vec<_> = std::mem::take(&mut self.sightings).into_values().collect();
        db.call(move |c| db::record_batch(c, round_id, &observations, &sightings))
            .await
    }
}

async fn crawl(
    db: &Db,
    cfg: &Config,
    round_id: i64,
    started_at: i64,
    deadline: Instant,
    mut stop: watch::Receiver<bool>,
) -> anyhow::Result<CrawlEnd> {
    let probe_cfg = Arc::new(ProbeConfig {
        magic: cfg.network.magic(),
        protocol_version: cfg.protocol_version(),
        user_agent: params::user_agent(),
        connect_timeout: Duration::from_secs(cfg.connect_timeout_secs),
        handshake_timeout: Duration::from_secs(cfg.handshake_timeout_secs),
        getaddr_timeout: Duration::from_secs(cfg.getaddr_timeout_secs),
        max_addrs: cfg.max_addrs_per_peer,
    });
    let mut frontier = Frontier {
        allow_non_routable: cfg.allow_non_routable,
        seen: HashSet::new(),
        queue: VecDeque::new(),
    };
    let mut pending = Pending::default();

    // Seeds: DNS seeders, configured peers, and the recent address book.
    let mut names = cfg.seeders();
    names.extend(cfg.extra_peers.iter().cloned());
    for addr in resolve_all(names).await {
        let addr = netaddr::normalize(addr);
        if frontier.accepts(addr) {
            pending.sight(addr, started_at, false);
        }
        frontier.push(addr);
    }
    let since = started_at.saturating_sub(i64::try_from(cfg.retention().as_secs())?);
    let known = db.call(move |c| db::known_addresses(c, since)).await?;
    info!(
        round_id,
        dns_and_configured = frontier.queue.len(),
        address_book = known.len(),
        "seeding round"
    );
    for addr in known {
        frontier.push(addr);
    }

    let mut tasks: JoinSet<ProbeResult> = JoinSet::new();
    let mut last_flush = Instant::now();
    let mut stop_open = true;
    loop {
        if *stop.borrow() {
            tasks.shutdown().await;
            pending.flush(db, round_id).await?;
            return Ok(CrawlEnd::Stopped);
        }
        let past_deadline = Instant::now() >= deadline;
        if !past_deadline {
            while tasks.len() < cfg.max_connections {
                let Some(addr) = frontier.queue.pop_front() else {
                    break;
                };
                let pc = Arc::clone(&probe_cfg);
                tasks.spawn(async move { peer::probe(addr, &pc).await });
            }
        }
        if tasks.is_empty() && (frontier.queue.is_empty() || past_deadline) {
            break;
        }

        tokio::select! {
            joined = tasks.join_next(), if !tasks.is_empty() => match joined {
                Some(Ok(result)) => record(&result, &mut frontier, &mut pending),
                Some(Err(e)) => error!(error = %e, "probe task failed"),
                None => {}
            },
            _ = sleep_until(deadline.into()), if !past_deadline => {
                info!(round_id, untried = frontier.queue.len(), "round time limit reached");
            }
            changed = stop.changed(), if stop_open => {
                // A closed channel can never signal a stop; stop polling it.
                stop_open = changed.is_ok();
            }
        }

        if pending.len() >= FLUSH_EVERY_ROWS || last_flush.elapsed() >= FLUSH_EVERY {
            pending.flush(db, round_id).await?;
            last_flush = Instant::now();
        }
    }
    pending.flush(db, round_id).await?;
    let hit_time_limit = !frontier.queue.is_empty();
    if hit_time_limit {
        warn!(
            round_id,
            untried = frontier.queue.len(),
            "round ended at its time limit"
        );
    }
    Ok(CrawlEnd::Done { hit_time_limit })
}

/// Turn a probe into an observation, and queue the addresses it returned.
fn record(result: &ProbeResult, frontier: &mut Frontier, pending: &mut Pending) {
    pending.observations.push(observation(result));
    if result.handshake_complete {
        pending.sight(result.addr, result.observed_at, true);
    }
    let now = unix_now();
    for &addr in &result.addrs {
        let addr = netaddr::normalize(addr);
        if frontier.accepts(addr) {
            pending.sight(addr, now, false);
            frontier.push(addr);
        }
    }
    debug!(
        addr = %result.addr,
        reachable = result.handshake_complete,
        failure = result.failure,
        learned = result.addrs.len(),
        "probed"
    );
}

pub fn observation(r: &ProbeResult) -> Observation {
    let mut o = Observation {
        addr: r.addr,
        reachable: r.handshake_complete,
        failure_reason: r.failure.map(str::to_owned),
        user_agent_raw: None,
        implementation: None,
        version: None,
        protocol_version: None,
        start_height: None,
        services: None,
        observed_at: r.observed_at,
    };
    if let Some(v) = &r.version {
        let parsed = parse_user_agent(&v.user_agent);
        o.user_agent_raw = Some(v.user_agent.clone());
        o.implementation = Some(parsed.implementation.as_str().to_owned());
        o.version = parsed.version.map(|v| v.to_string());
        o.protocol_version = Some(v.version);
        o.start_height = Some(v.start_height);
        o.services = Some(v.services);
    }
    o
}

/// Resolve `host:port` names concurrently. Failures are logged and skipped.
async fn resolve_all(names: Vec<String>) -> Vec<SocketAddr> {
    let mut set = JoinSet::new();
    for name in names {
        set.spawn(async move {
            let res = timeout(DNS_TIMEOUT, tokio::net::lookup_host(name.clone())).await;
            (name, res)
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((name, Ok(Ok(addrs)))) => {
                let before = out.len();
                out.extend(addrs);
                debug!(%name, addresses = out.len() - before, "resolved");
            }
            Ok((name, Ok(Err(e)))) => warn!(%name, error = %e, "could not resolve"),
            Ok((name, Err(_))) => warn!(%name, "DNS lookup timed out"),
            Err(e) => warn!(error = %e, "DNS task failed"),
        }
    }
    out
}
