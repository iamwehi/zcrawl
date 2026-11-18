//! SQLite storage.
//!
//! Times are stored as Unix seconds (UTC) in INTEGER columns.
//!
//! Consistency: a round row is inserted with `status = 'running'` and
//! `finished_at = NULL`. Observations are committed in batches while it runs.
//! Only the final transaction sets `status = 'complete'` and `finished_at`. If
//! the process dies, the row stays `running`; the next `zcrawl run` changes it to
//! `interrupted`. Reports and `status` read `complete` rounds only, so a crash
//! never leaves half a round in the results.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::params::Network;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rounds (
    id              INTEGER PRIMARY KEY,
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    status          TEXT NOT NULL DEFAULT 'running'
                    CHECK (status IN ('running', 'complete', 'interrupted')),
    hit_time_limit  INTEGER NOT NULL DEFAULT 0,
    addresses_tried INTEGER NOT NULL DEFAULT 0,
    nodes_reached   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS observations (
    id               INTEGER PRIMARY KEY,
    round_id         INTEGER NOT NULL REFERENCES rounds(id),
    address          TEXT NOT NULL,
    port             INTEGER NOT NULL,
    reachable        INTEGER NOT NULL,
    failure_reason   TEXT,
    -- BLOB affinity stores values unchanged: TEXT when the bytes were valid
    -- UTF-8, BLOB otherwise.
    user_agent_raw   BLOB,
    implementation   TEXT,
    version          TEXT,
    protocol_version INTEGER,
    start_height     INTEGER,
    services         INTEGER,
    observed_at      INTEGER NOT NULL,
    UNIQUE (round_id, address, port)
);

CREATE INDEX IF NOT EXISTS observations_round_reachable
    ON observations (round_id, reachable);

CREATE TABLE IF NOT EXISTS addresses (
    address        TEXT NOT NULL,
    port           INTEGER NOT NULL,
    first_seen     INTEGER NOT NULL,
    last_seen      INTEGER NOT NULL,
    last_reachable INTEGER,
    PRIMARY KEY (address, port)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS addresses_last_seen ON addresses (last_seen);
";

/// One connection attempt, as stored in `observations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub addr: SocketAddr,
    /// True once the version handshake completed.
    pub reachable: bool,
    pub failure_reason: Option<String>,
    /// Raw bytes as received. Stored as TEXT if valid UTF-8, otherwise BLOB.
    pub user_agent_raw: Option<Vec<u8>>,
    pub implementation: Option<String>,
    pub version: Option<String>,
    pub protocol_version: Option<u32>,
    pub start_height: Option<i32>,
    pub services: Option<u64>,
    pub observed_at: i64,
}

/// An address learned from DNS or `addr`, or reached by us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressSighting {
    pub addr: SocketAddr,
    pub seen_at: i64,
    pub reachable: bool,
}

/// Open (creating if needed) a database for writing.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    configure(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("database schema version {version} is newer than this zcrawl ({SCHEMA_VERSION})");
    }
    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(conn)
}

/// Open an existing database read-only, for `report` and `status`.
pub fn open_read_only(path: &Path) -> anyhow::Result<Connection> {
    if !path.exists() {
        bail!(
            "no database at {} (has `zcrawl run` been started?)",
            path.display()
        );
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening database {}", path.display()))?;
    configure(&conn)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        bail!("database schema version is {version}, expected {SCHEMA_VERSION}");
    }
    Ok(conn)
}

fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Record the network on first use and refuse to mix networks afterwards.
pub fn check_network(conn: &Connection, network: Network) -> anyhow::Result<()> {
    let stored: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'network'", [], |r| {
            r.get(0)
        })
        .optional()?;
    match stored {
        None => {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('network', ?1)",
                [network.as_str()],
            )?;
        }
        Some(s) if s == network.as_str() => {}
        Some(s) => bail!(
            "database holds {s} data but the config says {}; use a separate database file",
            network.as_str()
        ),
    }
    Ok(())
}

/// Mark rounds left `running` by a process that died as `interrupted`.
pub fn interrupt_stale_rounds(conn: &Connection) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE rounds SET status = 'interrupted' WHERE status = 'running'",
        [],
    )?)
}

pub fn start_round(conn: &Connection, started_at: i64) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT INTO rounds (started_at, status) VALUES (?1, 'running')",
        [started_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Store a batch of observations and address sightings in one transaction.
pub fn record_batch(
    conn: &mut Connection,
    round_id: i64,
    observations: &[Observation],
    sightings: &[AddressSighting],
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    insert_observations(&tx, round_id, observations)?;
    upsert_addresses(&tx, sightings)?;
    tx.commit()?;
    Ok(())
}

fn insert_observations(
    conn: &Connection,
    round_id: i64,
    observations: &[Observation],
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO observations (round_id, address, port, reachable, failure_reason,
             user_agent_raw, implementation, version, protocol_version, start_height,
             services, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for o in observations {
        let ua = match &o.user_agent_raw {
            None => Value::Null,
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => Value::Text(s.to_owned()),
                Err(_) => Value::Blob(bytes.clone()),
            },
        };
        stmt.execute(params![
            round_id,
            o.addr.ip().to_string(),
            o.addr.port(),
            o.reachable,
            o.failure_reason,
            ua,
            o.implementation,
            o.version,
            o.protocol_version,
            o.start_height,
            // SQLite integers are signed; the 64 service bits are stored as-is.
            o.services.map(|s| s as i64),
            o.observed_at,
        ])?;
    }
    Ok(())
}

fn upsert_addresses(conn: &Connection, sightings: &[AddressSighting]) -> anyhow::Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO addresses (address, port, first_seen, last_seen, last_reachable)
         VALUES (?1, ?2, ?3, ?3, ?4)
         ON CONFLICT (address, port) DO UPDATE SET
             last_seen = max(last_seen, excluded.last_seen),
             last_reachable = coalesce(max(last_reachable, excluded.last_reachable),
                                       last_reachable, excluded.last_reachable)",
    )?;
    for s in sightings {
        stmt.execute(params![
            s.addr.ip().to_string(),
            s.addr.port(),
            s.seen_at,
            s.reachable.then_some(s.seen_at),
        ])?;
    }
    Ok(())
}

/// Close a round. Counts come from the stored observations so they always
/// agree with them. Returns `(addresses_tried, nodes_reached)`.
pub fn finish_round(
    conn: &mut Connection,
    round_id: i64,
    finished_at: i64,
    hit_time_limit: bool,
) -> anyhow::Result<(u64, u64)> {
    let tx = conn.transaction()?;
    let (tried, reached): (i64, i64) = tx.query_row(
        "SELECT count(*), coalesce(sum(reachable), 0) FROM observations WHERE round_id = ?1",
        [round_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let updated = tx.execute(
        "UPDATE rounds SET status = 'complete', finished_at = ?2, hit_time_limit = ?3,
             addresses_tried = ?4, nodes_reached = ?5
         WHERE id = ?1 AND status = 'running'",
        params![round_id, finished_at, hit_time_limit, tried, reached],
    )?;
    if updated != 1 {
        bail!("round {round_id} was no longer running when it finished");
    }
    tx.commit()?;
    Ok((count(tried), count(reached)))
}

/// SQLite counts are signed; they are never negative.
pub fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

pub fn mark_interrupted(conn: &Connection, round_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE rounds SET status = 'interrupted' WHERE id = ?1 AND status = 'running'",
        [round_id],
    )?;
    Ok(())
}

/// Addresses seen (learned or reached) at or after `since`.
pub fn known_addresses(conn: &Connection, since: i64) -> anyhow::Result<Vec<SocketAddr>> {
    let mut stmt =
        conn.prepare("SELECT address, port FROM addresses WHERE last_seen >= ?1 ORDER BY address")?;
    let rows = stmt.query_map([since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, u16>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (ip, port) = row?;
        // Rows are written by zcrawl from parsed addresses; skip any that fail.
        if let Ok(ip) = ip.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, port));
        }
    }
    Ok(out)
}

/// A `Connection` usable from async code: each call runs on the blocking pool.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn new(conn: Connection) -> Self {
        Db {
            inner: Arc::new(Mutex::new(conn)),
        }
    }

    pub async fn call<R, F>(&self, f: F) -> anyhow::Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut conn = inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut conn)
        })
        .await
        .context("database task failed")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn obs(addr: &str, reachable: bool, ua: Option<&[u8]>) -> Observation {
        Observation {
            addr: addr.parse().unwrap(),
            reachable,
            failure_reason: (!reachable).then(|| "connect_timeout".into()),
            user_agent_raw: ua.map(|u| u.to_vec()),
            implementation: None,
            version: None,
            protocol_version: None,
            start_height: None,
            services: Some(u64::MAX),
            observed_at: 100,
        }
    }

    #[test]
    fn round_lifecycle() {
        let mut conn = mem();
        let id = start_round(&conn, 100).unwrap();
        record_batch(
            &mut conn,
            id,
            &[
                obs("1.2.3.4:8233", true, Some(b"/Zebra:2.5.0/")),
                obs("[2a01::1]:8233", false, None),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(finish_round(&mut conn, id, 200, false).unwrap(), (2, 1));
        let (status, fin): (String, i64) = conn
            .query_row(
                "SELECT status, finished_at FROM rounds WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((status.as_str(), fin), ("complete", 200));
        // Finishing twice is refused.
        assert!(finish_round(&mut conn, id, 300, false).is_err());
        // u64::MAX services round-trips through the signed column.
        let s: i64 = conn
            .query_row("SELECT services FROM observations LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(s as u64, u64::MAX);
    }

    #[test]
    fn stale_rounds_become_interrupted() {
        let conn = mem();
        let a = start_round(&conn, 100).unwrap();
        assert_eq!(interrupt_stale_rounds(&conn).unwrap(), 1);
        let status: String = conn
            .query_row("SELECT status FROM rounds WHERE id = ?1", [a], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "interrupted");
    }

    #[test]
    fn duplicate_address_in_round_is_rejected() {
        let mut conn = mem();
        let id = start_round(&conn, 100).unwrap();
        let o = obs("1.2.3.4:8233", true, None);
        assert!(record_batch(&mut conn, id, &[o.clone(), o], &[]).is_err());
        // The failed batch was rolled back as a whole.
        let n: i64 = conn
            .query_row("SELECT count(*) FROM observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn raw_user_agent_kept_exactly() {
        let mut conn = mem();
        let id = start_round(&conn, 100).unwrap();
        let bad = [b'/', 0xff, b'/'];
        record_batch(&mut conn, id, &[obs("1.2.3.4:1", true, Some(&bad))], &[]).unwrap();
        record_batch(
            &mut conn,
            id,
            &[obs("1.2.3.5:1", true, Some(b"/Zebra:1.0.0/"))],
            &[],
        )
        .unwrap();
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT user_agent_raw FROM observations WHERE address = '1.2.3.4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(blob, bad);
        let text: String = conn
            .query_row(
                "SELECT user_agent_raw FROM observations WHERE address = '1.2.3.5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "/Zebra:1.0.0/");
    }

    #[test]
    fn address_book_retention() {
        let mut conn = mem();
        let id = start_round(&conn, 0).unwrap();
        let s = |a: &str, t, r| AddressSighting {
            addr: a.parse().unwrap(),
            seen_at: t,
            reachable: r,
        };
        record_batch(
            &mut conn,
            id,
            &[],
            &[
                s("1.1.1.1:8233", 10, true),
                s("2.2.2.2:8233", 50, false),
                s("1.1.1.1:8233", 5, false),
            ],
        )
        .unwrap();
        let since_20 = known_addresses(&conn, 20).unwrap();
        assert_eq!(since_20, vec!["2.2.2.2:8233".parse().unwrap()]);
        let (last_seen, last_reachable): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_seen, last_reachable FROM addresses WHERE address = '1.1.1.1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((last_seen, last_reachable), (10, Some(10)));
    }

    #[test]
    fn network_mismatch_is_refused() {
        let conn = mem();
        check_network(&conn, Network::Testnet).unwrap();
        check_network(&conn, Network::Testnet).unwrap();
        assert!(check_network(&conn, Network::Mainnet).is_err());
    }
}
