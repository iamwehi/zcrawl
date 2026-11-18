//! Integration tests against mock Zcash peers on localhost.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};

use zcrawl::codec::{self, AddrEntry, Message, NetAddr, VersionMessage};
use zcrawl::config::Config;
use zcrawl::crawler::{self, RoundStatus};
use zcrawl::db::{self, Db};
use zcrawl::listener::{self, ListenerConfig};
use zcrawl::params::{self, Network};
use zcrawl::peer::{self, ProbeConfig};
use zcrawl::status;

const MAGIC: [u8; 4] = [0x24, 0xe9, 0x27, 0x64];

/// What a mock peer does after accepting a connection.
#[derive(Clone)]
enum Behaviour {
    /// Full handshake, a ping, then `addrs` in reply to `getaddr`.
    Honest {
        user_agent: &'static str,
        addrs: Vec<SocketAddr>,
    },
    /// Sends a `version` whose checksum is wrong.
    BadChecksum,
    /// Sends a header claiming a 4 GiB `version`, then nothing.
    OversizedLength,
    /// Sends its `version`, then hangs up, as a node that rejects us would.
    VersionThenClose,
}

/// What the mock saw from zcrawl on its most recent connection.
#[derive(Default, Debug, Clone)]
struct Seen {
    their_version: Option<VersionMessage>,
    got_verack: bool,
    got_pong: bool,
    got_getaddr: bool,
    /// Entries of `addr` messages zcrawl sent us.
    announced: Vec<AddrEntry>,
    other_commands: Vec<String>,
}

struct Mock {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    seen: Arc<Mutex<Seen>>,
}

async fn spawn_mock(behaviour: Behaviour) -> Mock {
    start_mock(TcpListener::bind("127.0.0.1:0").await.unwrap(), behaviour)
}

fn start_mock(listener: TcpListener, behaviour: Behaviour) -> Mock {
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (c, s) = (connections.clone(), seen.clone());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            c.fetch_add(1, Ordering::SeqCst);
            let (b, s) = (behaviour.clone(), s.clone());
            tokio::spawn(async move {
                let _ = serve(stream, b, s).await;
            });
        }
    });
    Mock {
        addr,
        connections,
        seen,
    }
}

fn mock_version(user_agent: &str) -> VersionMessage {
    VersionMessage {
        version: 170_190,
        services: 1,
        timestamp: 1_763_460_000,
        addr_recv: NetAddr {
            services: 0,
            addr: "0.0.0.0:0".parse().unwrap(),
        },
        addr_from: NetAddr {
            services: 1,
            addr: "0.0.0.0:0".parse().unwrap(),
        },
        nonce: 0xdead_beef,
        user_agent: user_agent.as_bytes().to_vec(),
        start_height: 3_600_000,
        relay: true,
    }
}

async fn serve(
    stream: TcpStream,
    behaviour: Behaviour,
    seen: Arc<Mutex<Seen>>,
) -> anyhow::Result<()> {
    let mut conn = BufReader::new(stream);
    let first = codec::read_message(&mut conn, MAGIC).await?;
    let Message::Version(v) = first else {
        anyhow::bail!("expected version first")
    };
    *seen.lock().await = Seen {
        their_version: Some(v),
        ..Seen::default()
    };

    let (user_agent, addrs) = match behaviour {
        Behaviour::Honest { user_agent, addrs } => (user_agent, addrs),
        Behaviour::BadChecksum => {
            let mut bytes =
                codec::encode_message(MAGIC, &Message::Version(mock_version("/Zebra:2.6.0/")));
            bytes[20] ^= 0xff;
            conn.get_mut().write_all(&bytes).await?;
            tokio::time::sleep(Duration::from_secs(5)).await;
            return Ok(());
        }
        Behaviour::OversizedLength => {
            let mut h = [0u8; 24];
            h[..4].copy_from_slice(&MAGIC);
            h[4..11].copy_from_slice(b"version");
            h[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
            conn.get_mut().write_all(&h).await?;
            tokio::time::sleep(Duration::from_secs(5)).await;
            return Ok(());
        }
        Behaviour::VersionThenClose => {
            let bytes =
                codec::encode_message(MAGIC, &Message::Version(mock_version("/Zebra:1.0.0/")));
            conn.get_mut().write_all(&bytes).await?;
            return Ok(());
        }
    };

    let send = |m: Message| codec::encode_message(MAGIC, &m);
    conn.get_mut()
        .write_all(&send(Message::Version(mock_version(user_agent))))
        .await?;
    conn.get_mut().write_all(&send(Message::Verack)).await?;
    // Zebra pings new peers; zcrawl must answer.
    conn.get_mut()
        .write_all(&send(Message::Ping(Some(42))))
        .await?;
    // Unrelated traffic zcrawl must ignore.
    conn.get_mut()
        .write_all(&send(Message::Other("inv".into())))
        .await?;

    loop {
        let msg = codec::read_message(&mut conn, MAGIC).await?;
        let mut s = seen.lock().await;
        match msg {
            Message::Verack => s.got_verack = true,
            Message::Pong(42) => s.got_pong = true,
            Message::GetAddr => {
                s.got_getaddr = true;
                let entries = addrs
                    .iter()
                    .map(|&addr| AddrEntry {
                        time: 1_763_460_000,
                        services: 1,
                        addr,
                    })
                    .collect();
                drop(s);
                conn.get_mut()
                    .write_all(&send(Message::Addr(entries)))
                    .await?;
            }
            Message::Addr(entries) => s.announced.extend(entries),
            other => s.other_commands.push(other.command().to_owned()),
        }
    }
}

fn probe_config() -> ProbeConfig {
    ProbeConfig {
        magic: MAGIC,
        protocol_version: params::PROTOCOL_VERSION,
        user_agent: params::user_agent(),
        connect_timeout: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(2),
        getaddr_timeout: Duration::from_secs(2),
        max_addrs: 1000,
        announce: None,
    }
}

async fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
    // dropped here, so connections are refused
}

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcrawl-test-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("zcrawl.sqlite")
}

#[tokio::test]
async fn handshake_and_two_addresses() {
    let returned: Vec<SocketAddr> = vec![
        "203.0.113.7:8233".parse().unwrap(),
        "[2a01:4f8::7]:8233".parse().unwrap(),
    ];
    let mock = spawn_mock(Behaviour::Honest {
        user_agent: "/Zebra:2.6.0/",
        addrs: returned.clone(),
    })
    .await;

    let result = peer::probe(mock.addr, &probe_config()).await;

    assert!(result.handshake_complete, "{:?}", result.failure);
    assert_eq!(result.failure, None);
    assert_eq!(result.addrs, returned);
    let v = result.version.as_ref().unwrap();
    assert_eq!(v.user_agent, b"/Zebra:2.6.0/");
    assert_eq!(v.start_height, 3_600_000);

    let obs = crawler::observation(&result);
    assert!(obs.reachable);
    assert_eq!(obs.implementation.as_deref(), Some("zebra"));
    assert_eq!(obs.version.as_deref(), Some("2.6.0"));
    assert_eq!(obs.protocol_version, Some(170_190));

    // What zcrawl told the peer.
    let seen = mock.seen.lock().await.clone();
    let ours = seen.their_version.unwrap();
    assert_eq!(ours.user_agent, params::user_agent().as_bytes());
    assert_eq!(ours.version, 170_190);
    assert_eq!(ours.services, 0);
    assert!(!ours.relay);
    assert!(seen.got_verack && seen.got_pong && seen.got_getaddr);
    // Without a listener we announce nothing.
    assert!(seen.announced.is_empty());
    assert_eq!(ours.addr_from.services, 0);
    // Nothing else: no block, header or transaction requests.
    assert!(seen.other_commands.is_empty(), "{:?}", seen.other_commands);
}

#[tokio::test]
async fn listening_crawler_announces_itself_as_a_full_node() {
    let returned: Vec<SocketAddr> = vec![
        "203.0.113.7:8233".parse().unwrap(),
        "203.0.113.8:8233".parse().unwrap(),
    ];
    let mock = spawn_mock(Behaviour::Honest {
        user_agent: "/Zebra:2.6.0/",
        addrs: returned.clone(),
    })
    .await;
    let ours: SocketAddr = "198.51.100.9:8233".parse().unwrap();
    let cfg = ProbeConfig {
        announce: Some(ours),
        ..probe_config()
    };

    let result = peer::probe(mock.addr, &cfg).await;
    assert!(result.handshake_complete, "{:?}", result.failure);
    // Our own announcement does not get in the way of the getaddr reply.
    assert_eq!(result.addrs, returned);

    let seen = mock.seen.lock().await.clone();
    let v = seen.their_version.unwrap();
    assert_eq!(v.services, params::NODE_NETWORK);
    assert_eq!(v.addr_from.services, params::NODE_NETWORK);
    assert_eq!(v.addr_from.addr, ours);
    assert_eq!(v.user_agent, params::user_agent().as_bytes());
    assert_eq!(seen.announced.len(), 1);
    assert_eq!(seen.announced[0].addr, ours);
    assert_eq!(seen.announced[0].services, params::NODE_NETWORK);
    assert!(seen.other_commands.is_empty(), "{:?}", seen.other_commands);
}

/// A listener on localhost writing to a fresh database.
struct TestListener {
    addr: SocketAddr,
    path: PathBuf,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn spawn_listener(name: &str) -> TestListener {
    let path = temp_db(name);
    let database = Db::new(db::open(&path).unwrap());
    let l = listener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    let cfg = ListenerConfig {
        probe: Arc::new(ProbeConfig {
            announce: Some("198.51.100.9:8233".parse().unwrap()),
            ..probe_config()
        }),
        max_connections: 8,
        allow_non_routable: true,
    };
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(listener::serve(l, cfg, database, stop_rx));
    TestListener {
        addr,
        path,
        stop,
        task,
    }
}

impl TestListener {
    /// Stop the listener; everything it saw is then in the database.
    async fn finish(self) -> rusqlite::Connection {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        db::open_read_only(&self.path).unwrap()
    }
}

/// Dial `addr` as a node would and send `version`.
async fn dial(addr: SocketAddr, mut version: VersionMessage) -> BufReader<TcpStream> {
    version.addr_from.addr = "0.0.0.0:18233".parse().unwrap();
    let mut conn = BufReader::new(TcpStream::connect(addr).await.unwrap());
    conn.get_mut()
        .write_all(&codec::encode_message(MAGIC, &Message::Version(version)))
        .await
        .unwrap();
    conn
}

async fn read(conn: &mut BufReader<TcpStream>) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(3), codec::read_message(conn, MAGIC))
        .await
        .unwrap()
        .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_records_a_dialling_node_and_hangs_up() {
    let l = spawn_listener("listen").await;
    let mut conn = dial(l.addr, mock_version("/Zebra:7.1.0/")).await;

    let Some(Message::Version(ours)) = read(&mut conn).await else {
        panic!("expected our version first");
    };
    assert_eq!(ours.services, params::NODE_NETWORK);
    assert_eq!(ours.addr_from.addr, "198.51.100.9:8233".parse().unwrap());
    assert_eq!(ours.nonce, peer::process_nonce());
    assert!(matches!(read(&mut conn).await, Some(Message::Verack)));
    conn.get_mut()
        .write_all(&codec::encode_message(MAGIC, &Message::Verack))
        .await
        .unwrap();
    // Then zcrawl hangs up without asking for anything.
    assert!(read(&mut conn).await.is_none());

    let conn = l.finish().await;
    let (done, implementation, version, claimed): (bool, String, String, String) = conn
        .query_row(
            "SELECT handshake_complete, implementation, version, claimed_addr FROM inbound",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert!(done);
    assert_eq!(
        (implementation.as_str(), version.as_str()),
        ("zebra", "7.1.0")
    );
    assert_eq!(claimed, "0.0.0.0:18233");
    // The claimed port on the connection's IP is queued for the crawler.
    assert_eq!(
        db::known_addresses(&conn, 0).unwrap(),
        vec!["127.0.0.1:18233".parse().unwrap()]
    );

    let st = status::compute(&conn, None).unwrap();
    let inbound = st.inbound.unwrap();
    assert_eq!((inbound.connections, inbound.nodes), (1, 1));
    assert_eq!(inbound.not_crawled, None);
    assert_eq!(
        inbound.versions,
        vec![("zebra".to_owned(), "7.1.0".to_owned(), 1)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_detects_our_own_crawler() {
    let l = spawn_listener("self").await;
    let mut v = mock_version(&params::user_agent());
    v.nonce = peer::process_nonce();
    let mut conn = dial(l.addr, v).await;
    // No version back: the connection is just closed.
    assert!(read(&mut conn).await.is_none());

    let conn = l.finish().await;
    let (done, reason): (bool, String) = conn
        .query_row(
            "SELECT handshake_complete, failure_reason FROM inbound",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(!done);
    assert_eq!(reason, "self_connection");
    assert!(db::known_addresses(&conn, 0).unwrap().is_empty());
}

#[tokio::test]
async fn bad_checksum_ends_connection_only() {
    let mock = spawn_mock(Behaviour::BadChecksum).await;
    let r = peer::probe(mock.addr, &probe_config()).await;
    assert!(!r.handshake_complete);
    assert_eq!(r.failure, Some("bad_checksum"));
}

#[tokio::test]
async fn oversized_length_ends_connection_only() {
    let mock = spawn_mock(Behaviour::OversizedLength).await;
    let r = peer::probe(mock.addr, &probe_config()).await;
    assert!(!r.handshake_complete);
    assert_eq!(r.failure, Some("oversized_message"));
}

#[tokio::test]
async fn version_then_close_keeps_user_agent_but_is_not_reachable() {
    let mock = spawn_mock(Behaviour::VersionThenClose).await;
    let r = peer::probe(mock.addr, &probe_config()).await;
    assert!(!r.handshake_complete);
    assert_eq!(r.failure, Some("connection_closed"));
    let obs = crawler::observation(&r);
    assert!(!obs.reachable);
    assert_eq!(obs.user_agent_raw.as_deref(), Some(&b"/Zebra:1.0.0/"[..]));
}

#[tokio::test]
async fn refused_connection() {
    let r = peer::probe(closed_port().await, &probe_config()).await;
    assert_eq!(r.failure, Some("connect_refused"));
    assert!(r.version.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn round_crawls_learned_addresses_once_each() {
    let closed = closed_port().await;
    // A returns B and the closed port; B returns A again and the closed port.
    let la = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lb = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (a_addr, b_addr) = (la.local_addr().unwrap(), lb.local_addr().unwrap());
    let mock_a = start_mock(
        la,
        Behaviour::Honest {
            user_agent: "/Zebra:2.6.0/",
            addrs: vec![b_addr, closed],
        },
    );
    let mock_b = start_mock(
        lb,
        Behaviour::Honest {
            user_agent: "/Zakura:1.6.1/",
            addrs: vec![a_addr, closed],
        },
    );

    let path = temp_db("round");
    let cfg = Config {
        network: Network::Mainnet,
        database: path.clone(),
        seeders: Some(vec![]),
        extra_peers: vec![mock_a.addr.to_string()],
        allow_non_routable: true,
        connect_timeout_secs: 2,
        handshake_timeout_secs: 2,
        getaddr_timeout_secs: 2,
        ..Config::default()
    };
    let conn = db::open(&path).unwrap();
    db::check_network(&conn, cfg.network).unwrap();
    let database = Db::new(conn);
    let (_stop_tx, stop_rx) = watch::channel(false);

    let summary = crawler::run_round(&database, &cfg, stop_rx.clone())
        .await
        .unwrap();
    assert_eq!(
        summary.status,
        RoundStatus::Complete {
            hit_time_limit: false
        }
    );
    // A, B and the closed port, each exactly once although each was
    // advertised twice.
    assert_eq!(summary.addresses_tried, 3);
    assert_eq!(summary.nodes_reached, 2);
    assert_eq!(mock_a.connections.load(Ordering::SeqCst), 1);
    assert_eq!(mock_b.connections.load(Ordering::SeqCst), 1);

    let conn = db::open_read_only(&path).unwrap();
    // (port, failure_reason, implementation, version) per observation.
    type Row = (u16, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = conn
        .prepare(
            "SELECT port, failure_reason, implementation, version
             FROM observations WHERE round_id = ?1",
        )
        .unwrap()
        .query_map([summary.round_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let find = |port: u16| rows.iter().find(|r| r.0 == port).unwrap().clone();
    assert_eq!(find(mock_a.addr.port()).2.as_deref(), Some("zebra"));
    assert_eq!(find(mock_b.addr.port()).2.as_deref(), Some("zakura"));
    assert_eq!(find(mock_b.addr.port()).3.as_deref(), Some("1.6.1"));
    assert_eq!(find(closed.port()).1.as_deref(), Some("connect_refused"));

    let st = status::compute(&conn, None).unwrap();
    assert_eq!(st.last_complete.as_ref().unwrap().nodes_reached, 2);
    assert_eq!(
        st.versions,
        vec![
            ("zakura".into(), "1.6.1".into(), 1),
            ("zebra".into(), "2.6.0".into(), 1)
        ]
    );
    let mut out = Vec::new();
    status::write(&st, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("nodes reached:   2"), "{text}");

    // Second round with no seeds at all: the address book alone finds A and B.
    let cfg2 = Config {
        extra_peers: vec![],
        ..cfg
    };
    let summary2 = crawler::run_round(&database, &cfg2, stop_rx).await.unwrap();
    assert_eq!(summary2.nodes_reached, 2);
    assert_eq!(mock_a.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_round_is_interrupted_and_excluded() {
    let mock = spawn_mock(Behaviour::Honest {
        user_agent: "/Zebra:2.6.0/",
        addrs: vec![],
    })
    .await;
    let path = temp_db("stop");
    let cfg = Config {
        database: path.clone(),
        seeders: Some(vec![]),
        extra_peers: vec![mock.addr.to_string()],
        allow_non_routable: true,
        // The mock replies to getaddr with zero addresses, so the probe waits
        // out this timeout, giving us time to stop the round.
        getaddr_timeout_secs: 30,
        ..Config::default()
    };
    let database = Db::new(db::open(&path).unwrap());
    let (stop_tx, stop_rx) = watch::channel(false);
    let round = tokio::spawn({
        let database = database.clone();
        let cfg = cfg.clone();
        async move { crawler::run_round(&database, &cfg, stop_rx).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop_tx.send(true).unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(5), round)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(summary.status, RoundStatus::Interrupted);

    let conn = db::open_read_only(&path).unwrap();
    let st = status::compute(&conn, None).unwrap();
    assert!(st.last_complete.is_none());
    assert_eq!(st.newer_incomplete.unwrap().status, "interrupted");
}

#[tokio::test]
async fn status_shows_an_idle_listener() {
    let path = temp_db("idle");
    drop(db::open(&path).unwrap());
    let conn = db::open_read_only(&path).unwrap();

    // Listener off and nothing recorded: no inbound section.
    let st = status::compute(&conn, None).unwrap();
    assert!(st.inbound.is_none());

    let ours: SocketAddr = "198.51.100.9:8233".parse().unwrap();
    let st = status::compute(&conn, Some(ours)).unwrap();
    let i = st.inbound.as_ref().unwrap();
    assert_eq!((i.connections, i.nodes), (0, 0));
    let mut out = Vec::new();
    status::write(&st, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("announced as:    198.51.100.9:8233"),
        "{text}"
    );
    assert!(text.contains("connections:     0"), "{text}");
}
