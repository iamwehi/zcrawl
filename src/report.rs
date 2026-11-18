//! The adoption curve for one release.
//!
//! For every complete round that started at or after the release, count the
//! reachable nodes in the denominator set and how many of them run the release
//! or a later version. Rounds still running, interrupted, or started before
//! the release are left out.

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::IpAddr;

use rusqlite::Connection;

use crate::netaddr::subnet_key;
use crate::user_agent::{Implementation, NodeVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denominator {
    /// Reachable nodes of the release's implementation.
    Implementation,
    /// All reachable nodes, whatever they run.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// Every reachable address counts once.
    Node,
    /// Every IPv4 /24 and IPv6 /48 counts once. A subnet is upgraded only
    /// when every node in it that is in the denominator set is upgraded.
    Subnet,
}

#[derive(Debug, Clone)]
pub struct ReportParams {
    pub implementation: Implementation,
    pub version: NodeVersion,
    /// Unix seconds.
    pub released_at: i64,
    pub denominator: Denominator,
    pub group_by: GroupBy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurvePoint {
    pub round_id: i64,
    /// Seconds from the release to the round's start.
    pub secs_since_release: i64,
    pub upgraded: u64,
    pub total: u64,
}

impl CurvePoint {
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| self.upgraded as f64 / self.total as f64)
    }

    /// Exact integer test of `upgraded / total >= percent / 100`.
    fn reaches(&self, percent: u64) -> bool {
        self.total > 0 && self.upgraded * 100 >= self.total * percent
    }
}

pub const THRESHOLDS: [u64; 3] = [50, 80, 95];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub points: Vec<CurvePoint>,
    /// For each of [`THRESHOLDS`], seconds from release to the first round at
    /// or above it, or `None` if no round has reached it.
    pub time_to: [(u64, Option<i64>); 3],
}

/// Per-round counting state.
#[derive(Default)]
struct Tally {
    total: u64,
    upgraded: u64,
    /// For subnet grouping: subnet -> every counted node so far is upgraded.
    subnets: HashMap<String, bool>,
}

pub fn compute(conn: &Connection, p: &ReportParams) -> anyhow::Result<Report> {
    let mut rounds_stmt = conn.prepare(
        "SELECT id, started_at FROM rounds
         WHERE status = 'complete' AND started_at >= ?1
         ORDER BY started_at, id",
    )?;
    let rounds: Vec<(i64, i64)> = rounds_stmt
        .query_map([p.released_at], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    let mut tallies: HashMap<i64, Tally> = HashMap::new();
    let mut obs_stmt = conn.prepare(
        "SELECT o.round_id, o.address, o.implementation, o.version
         FROM observations o JOIN rounds r ON r.id = o.round_id
         WHERE r.status = 'complete' AND r.started_at >= ?1 AND o.reachable = 1",
    )?;
    let mut rows = obs_stmt.query([p.released_at])?;
    let target_impl = p.implementation.as_str();
    while let Some(row) = rows.next()? {
        let round_id: i64 = row.get(0)?;
        let address: String = row.get(1)?;
        let implementation: Option<String> = row.get(2)?;
        let version: Option<String> = row.get(3)?;

        let same_impl = implementation.as_deref() == Some(target_impl);
        let counted = match p.denominator {
            Denominator::Implementation => same_impl,
            Denominator::All => true,
        };
        if !counted {
            continue;
        }
        let upgraded = same_impl
            && version
                .as_deref()
                .and_then(|v| v.parse::<NodeVersion>().ok())
                .is_some_and(|v| v >= p.version);

        let tally = tallies.entry(round_id).or_default();
        match p.group_by {
            GroupBy::Node => {
                tally.total += 1;
                tally.upgraded += u64::from(upgraded);
            }
            GroupBy::Subnet => {
                // Stored addresses are always valid IP literals.
                let Ok(ip) = address.parse::<IpAddr>() else {
                    continue;
                };
                let all_upgraded = tally.subnets.entry(subnet_key(ip)).or_insert(true);
                *all_upgraded &= upgraded;
            }
        }
    }

    let points: Vec<CurvePoint> = rounds
        .into_iter()
        .map(|(round_id, started_at)| {
            let t = tallies.remove(&round_id).unwrap_or_default();
            let (upgraded, total) = match p.group_by {
                GroupBy::Node => (t.upgraded, t.total),
                GroupBy::Subnet => (
                    t.subnets.values().filter(|&&up| up).count() as u64,
                    t.subnets.len() as u64,
                ),
            };
            CurvePoint {
                round_id,
                secs_since_release: started_at - p.released_at,
                upgraded,
                total,
            }
        })
        .collect();

    let time_to = THRESHOLDS.map(|pct| {
        let first = points.iter().find(|pt| pt.reaches(pct));
        (pct, first.map(|pt| pt.secs_since_release))
    });
    Ok(Report { points, time_to })
}

fn hours(secs: i64) -> String {
    format!("{:.2}", secs as f64 / 3600.0)
}

/// Write the curve as CSV, then the threshold times as `#` comment lines.
pub fn write(report: &Report, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "hours_since_release,nodes_on_version_or_newer,total_nodes,fraction"
    )?;
    for pt in &report.points {
        let fraction = pt.fraction().map(|f| format!("{f:.4}")).unwrap_or_default();
        writeln!(
            out,
            "{},{},{},{}",
            hours(pt.secs_since_release),
            pt.upgraded,
            pt.total,
            fraction
        )?;
    }
    writeln!(out)?;
    for (pct, t) in &report.time_to {
        match t {
            Some(secs) => writeln!(out, "# time to {pct}%: {} hours", hours(*secs))?,
            None => writeln!(out, "# time to {pct}%: not reached")?,
        }
    }
    Ok(())
}
