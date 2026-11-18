//! `zcrawl status`: a summary of the most recent complete round, and of the
//! nodes that dialled the listener in the last day.

use std::cmp::Reverse;
use std::io::{self, Write};

use rusqlite::{Connection, OptionalExtension};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::peer::unix_now;
use crate::user_agent::NodeVersion;

/// `status` summarises inbound connections from this far back.
pub const INBOUND_WINDOW_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRow {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub hit_time_limit: bool,
    pub addresses_tried: u64,
    pub nodes_reached: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub last_complete: Option<RoundRow>,
    /// A round newer than `last_complete` that is running or was interrupted.
    pub newer_incomplete: Option<RoundRow>,
    /// (implementation, version, nodes), most nodes first within an implementation.
    pub versions: Vec<(String, String, u64)>,
    /// (failure reason, count) for unreachable addresses.
    pub failures: Vec<(String, u64)>,
    /// Inbound connections in the last [`INBOUND_WINDOW_SECS`], or `None` if
    /// the listener has never recorded one.
    pub inbound: Option<InboundSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundSummary {
    /// Connections accepted, including ones that never completed a handshake.
    pub connections: u64,
    /// Distinct IPs that completed a handshake.
    pub nodes: u64,
    /// Of `nodes`, how many IPs the last complete round did not reach on any
    /// port: nodes only the listener sees. `None` without a complete round.
    pub not_crawled: Option<u64>,
    /// (implementation, version, IPs), using each IP's latest handshake.
    pub versions: Vec<(String, String, u64)>,
}

fn round_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RoundRow> {
    Ok(RoundRow {
        id: r.get(0)?,
        started_at: r.get(1)?,
        finished_at: r.get(2)?,
        status: r.get(3)?,
        hit_time_limit: r.get(4)?,
        addresses_tried: crate::db::count(r.get(5)?),
        nodes_reached: crate::db::count(r.get(6)?),
    })
}

const ROUND_COLUMNS: &str =
    "id, started_at, finished_at, status, hit_time_limit, addresses_tried, nodes_reached";

pub fn compute(conn: &Connection) -> anyhow::Result<Status> {
    compute_at(conn, unix_now())
}

/// By implementation, then newest version first.
fn sort_versions(versions: &mut [(String, String, u64)]) {
    versions.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            let va = a.1.parse::<NodeVersion>().ok();
            let vb = b.1.parse::<NodeVersion>().ok();
            Reverse(va).cmp(&Reverse(vb)).then_with(|| a.1.cmp(&b.1))
        })
    });
}

fn inbound_summary(
    conn: &Connection,
    since: i64,
    last_round: Option<i64>,
) -> anyhow::Result<Option<InboundSummary>> {
    let ever: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM inbound)", [], |r| r.get(0))?;
    if !ever {
        return Ok(None);
    }
    let (connections, nodes): (i64, i64) = conn.query_row(
        "SELECT count(*), count(DISTINCT CASE WHEN handshake_complete = 1 THEN address END)
         FROM inbound WHERE observed_at >= ?1",
        [since],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let not_crawled = match last_round {
        None => None,
        Some(round_id) => Some(crate::db::count(conn.query_row(
            "SELECT count(DISTINCT address) FROM inbound
             WHERE handshake_complete = 1 AND observed_at >= ?1
               AND address NOT IN (SELECT address FROM observations
                                   WHERE round_id = ?2 AND reachable = 1)",
            [since, round_id],
            |r| r.get(0),
        )?)),
    };
    let mut stmt = conn.prepare(
        "WITH latest AS (
             SELECT implementation, version,
                    row_number() OVER (PARTITION BY address
                                       ORDER BY observed_at DESC, id DESC) AS rn
             FROM inbound WHERE handshake_complete = 1 AND observed_at >= ?1)
         SELECT coalesce(implementation, 'unknown'), coalesce(version, '-'), count(*)
         FROM latest WHERE rn = 1 GROUP BY 1, 2",
    )?;
    let mut versions = stmt
        .query_map([since], |r| {
            Ok((r.get(0)?, r.get(1)?, crate::db::count(r.get(2)?)))
        })?
        .collect::<Result<Vec<(String, String, u64)>, _>>()?;
    sort_versions(&mut versions);
    Ok(Some(InboundSummary {
        connections: crate::db::count(connections),
        nodes: crate::db::count(nodes),
        not_crawled,
        versions,
    }))
}

/// [`compute`] with the inbound window ending at `now`.
pub fn compute_at(conn: &Connection, now: i64) -> anyhow::Result<Status> {
    let last_complete = conn
        .query_row(
            &format!(
                "SELECT {ROUND_COLUMNS} FROM rounds WHERE status = 'complete'
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            [],
            round_row,
        )
        .optional()?;
    let after = last_complete.as_ref().map_or(i64::MIN, |r| r.id);
    let newer_incomplete = conn
        .query_row(
            &format!(
                "SELECT {ROUND_COLUMNS} FROM rounds WHERE status != 'complete' AND id > ?1
                 ORDER BY id DESC LIMIT 1"
            ),
            [after],
            round_row,
        )
        .optional()?;

    let mut versions = Vec::new();
    let mut failures = Vec::new();
    if let Some(round) = &last_complete {
        let mut stmt = conn.prepare(
            "SELECT coalesce(implementation, 'unknown'), coalesce(version, '-'), count(*)
             FROM observations WHERE round_id = ?1 AND reachable = 1
             GROUP BY 1, 2",
        )?;
        versions = stmt
            .query_map([round.id], |r| {
                Ok((r.get(0)?, r.get(1)?, crate::db::count(r.get(2)?)))
            })?
            .collect::<Result<Vec<(String, String, u64)>, _>>()?;
        sort_versions(&mut versions);

        let mut stmt = conn.prepare(
            "SELECT coalesce(failure_reason, 'none'), count(*)
             FROM observations WHERE round_id = ?1 AND reachable = 0
             GROUP BY 1 ORDER BY 2 DESC, 1",
        )?;
        failures = stmt
            .query_map([round.id], |r| Ok((r.get(0)?, crate::db::count(r.get(1)?))))?
            .collect::<Result<_, _>>()?;
    }
    let inbound = inbound_summary(
        conn,
        now - INBOUND_WINDOW_SECS,
        last_complete.as_ref().map(|r| r.id),
    )?;
    Ok(Status {
        last_complete,
        newer_incomplete,
        versions,
        failures,
        inbound,
    })
}

pub fn format_time(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| unix.to_string())
}

fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{}m {:02}s", secs / 60, secs % 60)
}

fn write_versions(out: &mut impl Write, versions: &[(String, String, u64)]) -> io::Result<()> {
    writeln!(
        out,
        "{:<14} {:<22} {:>6}",
        "implementation", "version", "nodes"
    )?;
    let mut current_impl: Option<&str> = None;
    let mut impl_total = 0u64;
    for (imp, ver, n) in versions {
        if current_impl.is_some_and(|c| c != imp) {
            writeln!(out, "{:<14} {:<22} {:>6}", "", "(total)", impl_total)?;
            impl_total = 0;
        }
        current_impl = Some(imp);
        impl_total += n;
        writeln!(out, "{imp:<14} {ver:<22} {n:>6}")?;
    }
    if current_impl.is_some() {
        writeln!(out, "{:<14} {:<22} {:>6}", "", "(total)", impl_total)?;
    }
    Ok(())
}

fn write_inbound(out: &mut impl Write, inbound: &Option<InboundSummary>) -> io::Result<()> {
    let Some(i) = inbound else {
        return Ok(());
    };
    writeln!(out)?;
    writeln!(out, "Inbound, last 24 hours:")?;
    writeln!(out, "  connections:     {}", i.connections)?;
    writeln!(out, "  nodes (by IP):   {}", i.nodes)?;
    if let Some(n) = i.not_crawled {
        writeln!(
            out,
            "  not crawled:     {n} (not reached by the last round)"
        )?;
    }
    if !i.versions.is_empty() {
        writeln!(out)?;
        write_versions(out, &i.versions)?;
    }
    Ok(())
}

pub fn write(status: &Status, out: &mut impl Write) -> io::Result<()> {
    write_round(status, out)?;
    write_inbound(out, &status.inbound)
}

fn write_round(status: &Status, out: &mut impl Write) -> io::Result<()> {
    let Some(r) = &status.last_complete else {
        writeln!(out, "No complete round yet.")?;
        if let Some(inc) = &status.newer_incomplete {
            writeln!(
                out,
                "Round {} ({}) started at {}.",
                inc.id,
                inc.status,
                format_time(inc.started_at)
            )?;
        }
        return Ok(());
    };
    writeln!(out, "Last complete round: {}", r.id)?;
    writeln!(out, "  started:         {}", format_time(r.started_at))?;
    if let Some(f) = r.finished_at {
        writeln!(out, "  finished:        {}", format_time(f))?;
        writeln!(
            out,
            "  duration:        {}",
            format_duration(f - r.started_at)
        )?;
    }
    writeln!(out, "  addresses tried: {}", r.addresses_tried)?;
    writeln!(out, "  nodes reached:   {}", r.nodes_reached)?;
    if r.hit_time_limit {
        writeln!(
            out,
            "  note:            hit the round time limit; some addresses were not tried"
        )?;
    }

    writeln!(out)?;
    write_versions(out, &status.versions)?;

    if !status.failures.is_empty() {
        writeln!(out)?;
        writeln!(out, "Unreachable addresses by reason:")?;
        for (reason, n) in &status.failures {
            writeln!(out, "  {reason:<22} {n:>6}")?;
        }
    }

    if let Some(inc) = &status.newer_incomplete {
        writeln!(out)?;
        writeln!(
            out,
            "Round {} is {} (started {}).",
            inc.id,
            inc.status,
            format_time(inc.started_at)
        )?;
    }
    Ok(())
}
