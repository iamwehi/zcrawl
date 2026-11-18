//! Report tests on a fixture database with a known adoption curve.
//!
//! Release: Zebra 2.6.0, published at T = 2026-11-01T00:00:00Z.
//!
//! Every round sees the same reachable nodes:
//! - 20 Zebra nodes. Nodes 1-15 each sit in their own IPv4 /24
//!   (203.0.N.1). Nodes 16-20 share one /24 (198.51.100.16-20), as if run by
//!   one operator. All start on 2.5.0.
//! - 2 Zakura nodes in one IPv6 /48, 3 zcashd nodes in one /24, and 1 node
//!   with an unparseable user agent: 26 reachable nodes in all.
//! - 2 unreachable addresses, one of which sent `/Zebra:2.6.0/` before
//!   hanging up. Neither may be counted.
//!
//! Zebra nodes on 2.6.0 or newer, per round:
//!
//! | round | start  | status      | upgraded nodes                       | n/20 |
//! |-------|--------|-------------|--------------------------------------|------|
//! | 1     | T - 1h | complete    | none (before release: excluded)      |  -   |
//! | 2     | T + 6h | complete    | 16,17,18                             |  3   |
//! | 3     | T +12h | complete    | 1-3, 16-20                           |  8   |
//! | 4     | T +24h | complete    | 1-5, 16-20                           | 10   |
//! | 5     | T +30h | interrupted | all 20 (must be excluded)            |  -   |
//! | 6     | T +36h | running     | all 20 (must be excluded)            |  -   |
//! | 7     | T +48h | complete    | 1-11, 16-20                          | 16   |
//! | 8     | T +72h | complete    | 1-14 (14 on 2.7.0-rc.1), 16-20; node | 19   |
//! |       |        |             | 15 on 2.6.0-rc.2, which is older     |      |
//!
//! So 50% is reached at 24 h, 80% at 48 h and 95% at 72 h.

use rusqlite::{Connection, params};

use zcrawl::report::{self, CurvePoint, Denominator, GroupBy, ReportParams};
use zcrawl::user_agent::{Implementation, parse_user_agent};

const T: i64 = 1_761_955_200; // 2026-11-01T00:00:00Z
const H: i64 = 3600;

/// Upgraded Zebra node numbers and their version, per round.
fn zebra_version(round: i64, node: u32) -> &'static str {
    let upgraded = match round {
        1 => false,
        2 => matches!(node, 16..=18),
        3 => matches!(node, 1..=3 | 16..=20),
        4 => matches!(node, 1..=5 | 16..=20),
        5 | 6 => true,
        7 => matches!(node, 1..=11 | 16..=20),
        8 => {
            if node == 14 {
                return "2.7.0-rc.1";
            }
            if node == 15 {
                return "2.6.0-rc.2";
            }
            matches!(node, 1..=13 | 16..=20)
        }
        _ => unreachable!(),
    };
    if upgraded { "2.6.0" } else { "2.5.0" }
}

fn zebra_address(node: u32) -> String {
    if node <= 15 {
        format!("203.0.{node}.1")
    } else {
        format!("198.51.100.{node}")
    }
}

fn fixture() -> Connection {
    // Tests run in parallel and macOS clocks tick in microseconds, so the
    // time alone does not make the directory unique.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "zcrawl-report-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = zcrawl::db::open(&dir.join("fixture.sqlite")).unwrap();

    let rounds = [
        (1, T - H, "complete"),
        (2, T + 6 * H, "complete"),
        (3, T + 12 * H, "complete"),
        (4, T + 24 * H, "complete"),
        (5, T + 30 * H, "interrupted"),
        (6, T + 36 * H, "running"),
        (7, T + 48 * H, "complete"),
        (8, T + 72 * H, "complete"),
    ];
    for (id, start, status) in rounds {
        let finished = (status == "complete").then_some(start + 1800);
        conn.execute(
            "INSERT INTO rounds (id, started_at, finished_at, status) VALUES (?1, ?2, ?3, ?4)",
            params![id, start, finished, status],
        )
        .unwrap();

        let mut nodes: Vec<(String, bool, String)> = (1..=20)
            .map(|n| {
                (
                    zebra_address(n),
                    true,
                    format!("/Zebra:{}/", zebra_version(id, n)),
                )
            })
            .collect();
        nodes.push(("2001:db8:1::1".into(), true, "/Zakura:1.6.1/".into()));
        nodes.push(("2001:db8:1::2".into(), true, "/Zakura:1.6.0/".into()));
        for n in 1..=3 {
            nodes.push((format!("192.0.2.{n}"), true, "/MagicBean:6.0.0/".into()));
        }
        nodes.push(("100.64.0.1".into(), true, "not a user agent".into()));
        nodes.push(("100.64.0.2".into(), false, "/Zebra:2.6.0/".into()));
        nodes.push(("100.64.0.3".into(), false, String::new()));

        for (address, reachable, ua) in nodes {
            let parsed = (!ua.is_empty()).then(|| parse_user_agent(ua.as_bytes()));
            conn.execute(
                "INSERT INTO observations (round_id, address, port, reachable, failure_reason,
                     user_agent_raw, implementation, version, observed_at)
                 VALUES (?1, ?2, 8233, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    address,
                    reachable,
                    (!reachable).then_some("connection_closed"),
                    (!ua.is_empty()).then_some(&ua),
                    parsed.as_ref().map(|p| p.implementation.as_str()),
                    parsed
                        .as_ref()
                        .and_then(|p| p.version.as_ref())
                        .map(|v| v.to_string()),
                    start + 60,
                ],
            )
            .unwrap();
        }
    }
    conn
}

fn params(denominator: Denominator, group_by: GroupBy) -> ReportParams {
    ReportParams {
        implementation: Implementation::Zebra,
        version: "2.6.0".parse().unwrap(),
        released_at: T,
        denominator,
        group_by,
        inbound_window_secs: None,
    }
}

fn curve(points: &[CurvePoint]) -> Vec<(i64, u64, u64)> {
    points
        .iter()
        .map(|p| (p.secs_since_release / H, p.upgraded, p.total))
        .collect()
}

#[test]
fn zebra_nodes() {
    let rep = report::compute(
        &fixture(),
        &params(Denominator::Implementation, GroupBy::Node),
    )
    .unwrap();
    assert_eq!(
        curve(&rep.points),
        vec![
            (6, 3, 20),
            (12, 8, 20),
            (24, 10, 20),
            (48, 16, 20),
            (72, 19, 20)
        ]
    );
    assert_eq!(
        rep.time_to,
        [(50, Some(24 * H)), (80, Some(48 * H)), (95, Some(72 * H))]
    );

    let mut out = Vec::new();
    report::write(&rep, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "hours_since_release,nodes_on_version_or_newer,total_nodes,fraction\n\
         6.00,3,20,0.1500\n\
         12.00,8,20,0.4000\n\
         24.00,10,20,0.5000\n\
         48.00,16,20,0.8000\n\
         72.00,19,20,0.9500\n\
         \n\
         # time to 50%: 24.00 hours\n\
         # time to 80%: 48.00 hours\n\
         # time to 95%: 72.00 hours\n"
    );
}

#[test]
fn all_nodes_denominator() {
    let rep = report::compute(&fixture(), &params(Denominator::All, GroupBy::Node)).unwrap();
    // 26 reachable nodes of every kind; the unknown one counts too.
    assert_eq!(
        curve(&rep.points),
        vec![
            (6, 3, 26),
            (12, 8, 26),
            (24, 10, 26),
            (48, 16, 26),
            (72, 19, 26)
        ]
    );
    // 16/26 = 61.5% at 48 h; 19/26 = 73.1% at most.
    assert_eq!(rep.time_to, [(50, Some(48 * H)), (80, None), (95, None)]);

    let mut out = Vec::new();
    report::write(&rep, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.ends_with("# time to 80%: not reached\n# time to 95%: not reached\n"),
        "{text}"
    );
}

#[test]
fn zebra_subnets() {
    let rep = report::compute(
        &fixture(),
        &params(Denominator::Implementation, GroupBy::Subnet),
    )
    .unwrap();
    // 16 subnets: 15 single-node /24s and the shared 198.51.100.0/24, which
    // only counts once all five of its nodes are upgraded (from 12 h).
    assert_eq!(
        curve(&rep.points),
        vec![
            (6, 0, 16),
            (12, 4, 16),
            (24, 6, 16),
            (48, 12, 16),
            (72, 15, 16)
        ]
    );
    // 12/16 = 75% at 48 h; 15/16 = 93.75% at 72 h.
    assert_eq!(
        rep.time_to,
        [(50, Some(48 * H)), (80, Some(72 * H)), (95, None)]
    );
}

#[test]
fn all_subnets() {
    let rep = report::compute(&fixture(), &params(Denominator::All, GroupBy::Subnet)).unwrap();
    // 16 Zebra subnets + 1 Zakura /48 + 1 zcashd /24 + 100.64.0.0/24 = 19.
    assert_eq!(
        curve(&rep.points),
        vec![
            (6, 0, 19),
            (12, 4, 19),
            (24, 6, 19),
            (48, 12, 19),
            (72, 15, 19)
        ]
    );
    assert_eq!(rep.time_to, [(50, Some(48 * H)), (80, None), (95, None)]);
}

#[test]
fn zakura_release() {
    let p = ReportParams {
        implementation: Implementation::Zakura,
        version: "1.6.1".parse().unwrap(),
        ..params(Denominator::Implementation, GroupBy::Node)
    };
    let rep = report::compute(&fixture(), &p).unwrap();
    assert!(
        rep.points
            .iter()
            .all(|pt| (pt.upgraded, pt.total) == (1, 2))
    );
    assert_eq!(rep.time_to, [(50, Some(6 * H)), (80, None), (95, None)]);
}

#[test]
fn release_after_all_rounds_has_empty_curve() {
    let p = ReportParams {
        released_at: T + 100 * H,
        ..params(Denominator::Implementation, GroupBy::Node)
    };
    let rep = report::compute(&fixture(), &p).unwrap();
    assert!(rep.points.is_empty());
    assert_eq!(rep.time_to, [(50, None), (80, None), (95, None)]);
}

/// Inbound sightings on top of the fixture, counted with a 6 hour window.
///
/// - Hidden Zebra node 198.18.0.1 dials in at T+5h on 2.5.0 and at T+11h30m
///   on 2.6.0. Round 2 (window T..T+6h30m) counts it as not upgraded, round 3
///   (T+6h..T+12h30m) as upgraded, and later rounds not at all.
/// - Node 1 (203.0.1.1) also dials in, at T+6h10m announcing 2.6.0. Round 2
///   already reached that IP, so it is not counted twice.
/// - 198.18.0.2 never completes its handshake, and a hidden Zakura node
///   only counts in the `all` denominator.
fn with_inbound() -> Connection {
    let conn = fixture();
    type Row = (
        &'static str,
        bool,
        Option<&'static str>,
        Option<&'static str>,
        i64,
    );
    let rows: [Row; 5] = [
        ("198.18.0.1", true, Some("zebra"), Some("2.5.0"), T + 5 * H),
        (
            "198.18.0.1",
            true,
            Some("zebra"),
            Some("2.6.0"),
            T + 11 * H + 1800,
        ),
        (
            "203.0.1.1",
            true,
            Some("zebra"),
            Some("2.6.0"),
            T + 6 * H + 600,
        ),
        ("198.18.0.2", false, None, None, T + 6 * H),
        (
            "2001:db8:9::1",
            true,
            Some("zakura"),
            Some("1.8.9"),
            T + 6 * H,
        ),
    ];
    for (address, done, implementation, version, at) in rows {
        conn.execute(
            "INSERT INTO inbound (address, port, handshake_complete, implementation, version,
                 observed_at)
             VALUES (?1, 50000, ?2, ?3, ?4, ?5)",
            params![address, done, implementation, version, at],
        )
        .unwrap();
    }
    conn
}

#[test]
fn inbound_window_adds_hidden_nodes_once() {
    let conn = with_inbound();
    let mut p = params(Denominator::Implementation, GroupBy::Node);
    // Without a window, inbound rows change nothing.
    assert_eq!(
        curve(&report::compute(&conn, &p).unwrap().points),
        vec![
            (6, 3, 20),
            (12, 8, 20),
            (24, 10, 20),
            (48, 16, 20),
            (72, 19, 20)
        ]
    );
    p.inbound_window_secs = Some(6 * H);
    assert_eq!(
        curve(&report::compute(&conn, &p).unwrap().points),
        vec![
            (6, 3, 21),
            (12, 9, 21),
            (24, 10, 20),
            (48, 16, 20),
            (72, 19, 20)
        ]
    );

    let mut p = params(Denominator::All, GroupBy::Node);
    p.inbound_window_secs = Some(6 * H);
    let points = report::compute(&conn, &p).unwrap().points;
    // 26 reachable, plus the hidden Zebra and Zakura nodes.
    assert_eq!((points[0].upgraded, points[0].total), (3, 28));
}
