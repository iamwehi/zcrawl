//! Talking to a single peer.
//!
//! Outbound ([`probe`]): connect, handshake, `getaddr`, disconnect.
//! Inbound ([`answer`]): handshake, disconnect.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;

use crate::codec::{self, AddrEntry, Message, NetAddr, ReadError, VersionMessage};
use crate::params::NODE_NETWORK;

/// Settings for one probe.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub magic: [u8; 4],
    pub protocol_version: u32,
    pub user_agent: String,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub getaddr_timeout: Duration,
    pub max_addrs: usize,
    /// Our public listener address. When set, our `version` advertises
    /// `NODE_NETWORK` and this address, and every crawled peer is sent an
    /// `addr` announcing it, so nodes behind NAT learn of us and dial in.
    pub announce: Option<SocketAddr>,
}

/// Messages accepted in one phase before the peer is considered abusive.
const MAX_MESSAGES_PER_PHASE: usize = 200;

/// What we learned from one peer.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub addr: SocketAddr,
    /// Unix seconds when the probe started.
    pub observed_at: i64,
    /// The peer's `version` message, if it sent one.
    pub version: Option<VersionMessage>,
    /// True if the version handshake completed.
    pub handshake_complete: bool,
    /// Why the probe failed before the handshake completed.
    pub failure: Option<&'static str>,
    /// Addresses the peer returned to `getaddr`. Always empty for inbound
    /// connections.
    pub addrs: Vec<SocketAddr>,
}

impl ProbeResult {
    fn new(addr: SocketAddr) -> Self {
        ProbeResult {
            addr,
            observed_at: unix_now(),
            version: None,
            handshake_complete: false,
            failure: None,
            addrs: Vec::new(),
        }
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The nonce sent in every `version` from this process. One value for both
/// directions lets the crawler and the listener each recognise a connection to
/// the other. Our user agent already identifies us, so a shared nonce reveals
/// nothing new.
pub fn process_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    *NONCE.get_or_init(random_nonce)
}

/// Random nonce for self-connection detection. `RandomState` is seeded from
/// the OS RNG, which is enough here; the nonce has no security role.
fn random_nonce() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

fn io_reason(e: &io::Error) -> &'static str {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => "connect_refused",
        io::ErrorKind::NetworkUnreachable | io::ErrorKind::HostUnreachable => "connect_unreachable",
        _ => "connect_error",
    }
}

fn read_reason(e: &ReadError) -> &'static str {
    match e {
        ReadError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof => "connection_closed",
        ReadError::Io(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ) =>
        {
            "connection_reset"
        }
        ReadError::Io(_) => "io_error",
        ReadError::Codec(c) => c.reason_code(),
    }
}

/// Probe `addr`. Never panics and never returns an error: every failure is
/// recorded in the result.
pub async fn probe(addr: SocketAddr, cfg: &ProbeConfig) -> ProbeResult {
    let mut result = ProbeResult::new(addr);

    let stream = match timeout(cfg.connect_timeout, TcpStream::connect(addr)).await {
        Err(_) => {
            result.failure = Some("connect_timeout");
            return result;
        }
        Ok(Err(e)) => {
            debug!(%addr, error = %e, "connect failed");
            result.failure = Some(io_reason(&e));
            return result;
        }
        Ok(Ok(s)) => s,
    };
    let _ = stream.set_nodelay(true);
    let mut conn = BufReader::new(stream);

    match timeout(
        cfg.handshake_timeout,
        handshake(&mut conn, addr, cfg, Direction::Outbound, &mut result),
    )
    .await
    {
        Err(_) => result.failure = Some("handshake_timeout"),
        Ok(Err(reason)) => result.failure = Some(reason),
        Ok(Ok(())) => {
            result.handshake_complete = true;
            if let Some(ours) = cfg.announce {
                // Sent before `getaddr`, so the peer's reply is still the
                // first multi-entry `addr` we read.
                let entry = AddrEntry {
                    time: u32::try_from(unix_now()).unwrap_or(u32::MAX),
                    services: NODE_NETWORK,
                    addr: ours,
                };
                if let Err(e) = send(&mut conn, cfg.magic, &Message::Addr(vec![entry])).await {
                    debug!(%addr, reason = e, "could not announce our address");
                }
            }
            // Errors after the handshake only end the address collection.
            if let Err(e) = timeout(
                cfg.getaddr_timeout,
                collect_addrs(&mut conn, cfg, &mut result),
            )
            .await
            .unwrap_or(Ok(()))
            {
                debug!(%addr, reason = e, "stopped reading addresses");
            }
        }
    }
    let _ = conn.get_mut().shutdown().await;
    result
}

/// Answer a connection a peer opened to our listener: complete the
/// handshake, record the peer's `version`, and hang up. We never hold the
/// connection, so a node that dialled us because we advertise
/// `NODE_NETWORK` loses its outbound slot for at most `handshake_timeout`.
pub async fn answer(stream: TcpStream, peer: SocketAddr, cfg: &ProbeConfig) -> ProbeResult {
    let mut result = ProbeResult::new(peer);
    let _ = stream.set_nodelay(true);
    let mut conn = BufReader::new(stream);
    match timeout(
        cfg.handshake_timeout,
        handshake(&mut conn, peer, cfg, Direction::Inbound, &mut result),
    )
    .await
    {
        Err(_) => result.failure = Some("handshake_timeout"),
        Ok(Err(reason)) => result.failure = Some(reason),
        Ok(Ok(())) => result.handshake_complete = true,
    }
    let _ = conn.get_mut().shutdown().await;
    result
}

/// Who opened the connection. The side that dialled sends `version` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Outbound,
    Inbound,
}

/// The `version` we send to `peer`.
fn our_version(cfg: &ProbeConfig, peer: SocketAddr) -> VersionMessage {
    // Zebra rounds its timestamp down to 5 minutes for privacy; so do we.
    let now = unix_now();
    let (services, addr_from) = match cfg.announce {
        Some(ours) => (NODE_NETWORK, ours),
        // No services: we serve no blocks and relay nothing.
        None => (0, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)),
    };
    VersionMessage {
        version: cfg.protocol_version,
        services,
        timestamp: now - now.rem_euclid(300),
        addr_recv: NetAddr {
            services: NODE_NETWORK,
            addr: peer,
        },
        // Zebra caches this address with our services and passes it on when
        // it asks this connection for peers.
        addr_from: NetAddr {
            services,
            addr: addr_from,
        },
        nonce: process_nonce(),
        user_agent: cfg.user_agent.as_bytes().to_vec(),
        // We hold no chain.
        start_height: 0,
        // Ask peers not to relay transactions to us.
        relay: false,
    }
}

async fn send(
    conn: &mut BufReader<TcpStream>,
    magic: [u8; 4],
    msg: &Message,
) -> Result<(), &'static str> {
    conn.get_mut()
        .write_all(&codec::encode_message(magic, msg))
        .await
        .map_err(|e| match e.kind() {
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe => "connection_reset",
            _ => "io_error",
        })
}

async fn handshake(
    conn: &mut BufReader<TcpStream>,
    addr: SocketAddr,
    cfg: &ProbeConfig,
    direction: Direction,
    result: &mut ProbeResult,
) -> Result<(), &'static str> {
    let ours = our_version(cfg, addr);
    let nonce = ours.nonce;
    if direction == Direction::Outbound {
        send(conn, cfg.magic, &Message::Version(ours.clone())).await?;
    }

    let mut got_verack = false;
    for _ in 0..MAX_MESSAGES_PER_PHASE {
        let msg = codec::read_message(conn, cfg.magic).await.map_err(|e| {
            debug!(%addr, error = %e, "read failed during handshake");
            read_reason(&e)
        })?;
        match msg {
            Message::Version(v) if result.version.is_none() => {
                if v.nonce == nonce {
                    return Err("self_connection");
                }
                result.version = Some(v);
                if direction == Direction::Inbound {
                    send(conn, cfg.magic, &Message::Version(ours.clone())).await?;
                }
                send(conn, cfg.magic, &Message::Verack).await?;
            }
            Message::Verack => got_verack = true,
            Message::Ping(Some(n)) => send(conn, cfg.magic, &Message::Pong(n)).await?,
            // Duplicate versions, addresses and everything else are ignored.
            _ => {}
        }
        if got_verack && result.version.is_some() {
            return Ok(());
        }
    }
    Err("too_many_messages")
}

async fn collect_addrs(
    conn: &mut BufReader<TcpStream>,
    cfg: &ProbeConfig,
    result: &mut ProbeResult,
) -> Result<(), &'static str> {
    send(conn, cfg.magic, &Message::GetAddr).await?;
    for _ in 0..MAX_MESSAGES_PER_PHASE {
        let msg = codec::read_message(conn, cfg.magic)
            .await
            .map_err(|e| read_reason(&e))?;
        let entries = match msg {
            Message::Addr(e) | Message::AddrV2(e) => e,
            Message::Ping(Some(n)) => {
                send(conn, cfg.magic, &Message::Pong(n)).await?;
                continue;
            }
            _ => continue,
        };
        // A single entry is usually the peer announcing itself unprompted;
        // keep it but wait for the real reply.
        let is_reply = entries.len() > 1;
        let room = cfg.max_addrs.saturating_sub(result.addrs.len());
        result
            .addrs
            .extend(entries.iter().take(room).map(|e| e.addr));
        if is_reply || result.addrs.len() >= cfg.max_addrs {
            return Ok(());
        }
    }
    Err("too_many_messages")
}
