//! `zcrawl status`: a summary of the most recent complete round.

use std::cmp::Reverse;
use std::io::{self, Write};

use rusqlite::{Connection, OptionalExtension};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::user_agent::NodeVersion;

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
        // By implementation, then newest version first.
        versions.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                let va = a.1.parse::<NodeVersion>().ok();
                let vb = b.1.parse::<NodeVersion>().ok();
                Reverse(va).cmp(&Reverse(vb)).then_with(|| a.1.cmp(&b.1))
            })
        });

        let mut stmt = conn.prepare(
            "SELECT coalesce(failure_reason, 'none'), count(*)
             FROM observations WHERE round_id = ?1 AND reachable = 0
             GROUP BY 1 ORDER BY 2 DESC, 1",
        )?;
        failures = stmt
            .query_map([round.id], |r| Ok((r.get(0)?, crate::db::count(r.get(1)?))))?
            .collect::<Result<_, _>>()?;
    }
    Ok(Status {
        last_complete,
        newer_incomplete,
        versions,
        failures,
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

pub fn write(status: &Status, out: &mut impl Write) -> io::Result<()> {
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
    writeln!(
        out,
        "{:<14} {:<22} {:>6}",
        "implementation", "version", "nodes"
    )?;
    let mut current_impl: Option<&str> = None;
    let mut impl_total = 0u64;
    for (imp, ver, n) in &status.versions {
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
