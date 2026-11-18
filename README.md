# zcrawl

`zcrawl` crawls the Zcash p2p network once an hour. For every node it can reach, it records the
software and version the node announces. From those snapshots it reports how quickly operators
adopt a release: the fraction of reachable nodes on that release or newer, plotted against hours
since the release was published, and the time to reach 50%, 80% and 95%.

zcrawl collects no historical data. It records from the moment it is started.

**Contact.** zcrawl announces itself as `/zcrawl:<version>/`. If it causes problems for your
node, open an issue at **<https://github.com/CHANGE-ME/zcrawl/issues>**.
<!-- TODO: replace with the real repository or contact URL before deploying. -->

## How a round works

1. **Seed.** The round starts from three sources: the DNS seeders, any `extra_peers`, and every
   address learned or reached in the last `address_retention_days` (default 7).
2. **Probe each address**, at most `max_connections` (200) at a time and at most once per round:
   - Open a TCP connection, with a 5 s timeout.
   - Exchange `version`/`verack`, with a 10 s timeout. The peer's `version` message is recorded.
   - Send `getaddr` and wait up to 10 s for an `addr`/`addrv2` reply with more than one entry.
   - Disconnect.
3. **Crawl what was learned.** Newly learned IPv4/IPv6 addresses are probed in the same round.
   The round ends when no addresses are left, or when `round_time_limit_minutes` (45) has passed.
   At the time limit, no new connections start and the ones in flight finish.
4. **Commit.** The round is marked `complete` in a single transaction. The next round starts
   `round_interval_minutes` (60) after this one started. If a round overruns, the next one starts
   as soon as it ends.

On the wire, zcrawl:
- answers `ping` with `pong` and ignores every other message;
- never sends `getdata`, `getblocks`, `getheaders`, `inv` or `tx`;
- advertises no services, with `relay = false` and `start_height = 0`.

Tor, I2P and CJDNS addresses in `addrv2` are dropped, and so are onion-cat IPv6 addresses.
Private, loopback, documentation and other non-global addresses are skipped.

## Install

```sh
cargo build --release
sudo install -m 0755 target/release/zcrawl /usr/local/bin/zcrawl
sudo useradd --system --no-create-home --shell /usr/sbin/nologin zcrawl
sudo install -d -m 0755 /etc/zcrawl
sudo install -m 0644 contrib/zcrawl.toml /etc/zcrawl/zcrawl.toml
sudo install -m 0644 contrib/systemd/zcrawl.service /etc/systemd/system/zcrawl.service
sudo systemctl daemon-reload
sudo systemctl enable --now zcrawl
journalctl -u zcrawl -f
```

The unit runs `zcrawl run` as the unprivileged `zcrawl` user. It keeps state in
`/var/lib/zcrawl` (created by systemd's `StateDirectory=`) and is sandboxed to outbound
networking plus that directory. Because the directory is mode 0750, query the database as the
service user:

```sh
sudo -u zcrawl zcrawl --config /etc/zcrawl/zcrawl.toml status
```

Building needs Rust 1.89 or newer, because zcrawl uses `File::try_lock`. SQLite is compiled in;
no system library is needed.

## Configuration

zcrawl reads a TOML file. It uses `--config <path>` if given, otherwise `./zcrawl.toml`, then
`/etc/zcrawl/zcrawl.toml`, then built-in defaults. `--db <path>` overrides `database` for any
subcommand. Every key is optional, and unknown keys are an error.
[`contrib/zcrawl.toml`](contrib/zcrawl.toml) lists them all.

| key | default | meaning |
|---|---|---|
| `network` | `"mainnet"` | `"testnet"` switches magic bytes, default port and DNS seeders. |
| `database` | `"zcrawl.sqlite"` | SQLite file. zcrawl refuses to mix networks in one file. |
| `round_interval_minutes` | 60 | Time between the starts of two rounds. |
| `round_time_limit_minutes` | 45 | No new connections start after this; the round still counts as complete. |
| `address_retention_days` | 7 | Addresses seen within this window seed the next round. |
| `connect_timeout_secs` | 5 | TCP connect timeout. |
| `handshake_timeout_secs` | 10 | `version`/`verack` exchange timeout. |
| `getaddr_timeout_secs` | 10 | Wait for an `addr` reply. |
| `max_connections` | 200 | Simultaneous connections. |
| `max_addrs_per_peer` | 1000 | Addresses taken from one peer per round. |
| `seeders` | Zebra's list | DNS seeders as `host:port`. |
| `extra_peers` | `[]` | Extra `host:port` peers to probe every round. |
| `protocol_version` | 170190 | Protocol version sent in our `version` message. |
| `allow_non_routable` | `false` | Probe private and loopback addresses (tests only). |

`RUST_LOG` controls logging, for example `RUST_LOG=zcrawl=debug`. Logs go to stderr.

### Network constants

These values were taken from the Zebra source at commit `720141ff` (2026-11-17). The Zakura
source at `4546310f` has the same values. [`src/params.rs`](src/params.rs) names the file each
one came from.

| | mainnet | testnet |
|---|---|---|
| magic bytes | `24 e9 27 64` | `fa 1a f9 bf` |
| default port | 8233 | 18233 |
| protocol version Zebra advertises (`CURRENT_NETWORK_PROTOCOL_VERSION`) | 170190 | 170190 |
| lowest peer version Zebra accepts after NU7 (`min_specified_for_upgrade(Nu7)`) | 170190 | 170180 |
| NU7 activation height | 3,543,405 | 4,465,026 |

DNS seeders:
- mainnet: `dnsseed.str4d.xyz`, `dnsseed.z.cash`, `mainnet.seeder.shieldedinfra.net`,
  `mainnet.seeder.zfnd.org`, `seeder.zec.rocks`
- testnet: `dnsseed.testnet.z.cash`, `seeder.testnet.zec.rocks`, `testnet.seeder.zfnd.org`

Zakura upgrades a connection to its QUIC-based "P2P v2" transport only when the peer advertises
service bit 24 (`NODE_P2P_V2`). zcrawl advertises no services, so it always gets the legacy TCP
handshake.

## Commands

### `zcrawl run [--once]`

Runs rounds until SIGINT or SIGTERM. `--once` runs a single round and exits.

On shutdown, the round in progress is marked `interrupted`. If the process dies without warning,
its round stays `running`. The next `zcrawl run` marks it `interrupted` before starting. A lock
file (`<database>.lock`) stops two `run` processes from sharing a database.

### `zcrawl report`

```sh
zcrawl report --implementation zebra --version 2.6.0 --released-at 2026-11-20T15:00:00Z
```

Output is CSV on stdout, followed by the threshold times as `#` comment lines. A CSV reader that
honours comments, such as `pandas.read_csv(..., comment="#")`, can read it directly.

```text
hours_since_release,nodes_on_version_or_newer,total_nodes,fraction
6.00,3,20,0.1500
12.00,8,20,0.4000
24.00,10,20,0.5000
48.00,16,20,0.8000
72.00,19,20,0.9500

# time to 50%: 24.00 hours
# time to 80%: 48.00 hours
# time to 95%: 72.00 hours
```

What gets counted:
- **Rows.** There is one row per `complete` round that started at or after `--released-at`.
  `hours_since_release` is measured from the round's start time. Rounds that are `running` or
  `interrupted` are never included.
- **Numerator.** Reachable nodes of the given implementation whose parsed version is `>=`
  `--version`, using SemVer order. A pre-release such as `2.6.0-rc.1` is older than `2.6.0`.
  A later pre-release such as `2.7.0-rc.1` is newer. Nodes with an unparseable user agent are
  never in the numerator.
- **Denominator.** By default (`--denominator implementation`), reachable nodes of the same
  implementation. With `--denominator all`, every reachable node, including zcashd, `other` and
  `unknown`.
- **Time to X%.** The `hours_since_release` of the first round whose fraction is at least X%.
  Comparisons use integers, so 10/20 counts as exactly 50%. It does not interpolate between
  rounds, so the true time lies between the previous round and this one. If the fraction later
  drops below X%, the first crossing is still reported.

With `--group-by subnet`, each IPv4 /24 and IPv6 /48 counts once instead of each address:
- A subnet is in the denominator if it holds at least one counted node.
- A subnet is in the numerator only if **every** counted node in it is upgraded.

Compare this with the per-node curve to see how much a few large operators move the result.

### `zcrawl status`

Shows the most recent complete round:
- start time, finish time and duration;
- addresses tried and nodes reached;
- a count per implementation and version;
- failure reasons for unreachable addresses;
- any newer round that is still running or was interrupted.

## Data

All times are Unix seconds (UTC). Query them with `datetime(col, 'unixepoch')`.

`rounds`

| column | |
|---|---|
| `id`, `started_at`, `finished_at` | `finished_at` is NULL unless `status = 'complete'`. |
| `status` | `running`, `complete` or `interrupted`. Reports use `complete` only. |
| `hit_time_limit` | 1 if the round stopped at its time limit with addresses still untried. |
| `addresses_tried`, `nodes_reached` | Counted from `observations` when the round completes. |

`observations` has one row per address per round. `UNIQUE (round_id, address, port)` enforces
this.

| column | |
|---|---|
| `address`, `port` | The IP literal without brackets, and the port. IPv4-mapped IPv6 is stored as IPv4. |
| `reachable` | 1 if the `version`/`verack` handshake completed. |
| `failure_reason` | NULL if reachable. Otherwise one of: `connect_timeout`, `connect_refused`, `connect_unreachable`, `connect_error`, `handshake_timeout`, `connection_closed`, `connection_reset`, `io_error`, `bad_magic`, `bad_command`, `bad_checksum`, `oversized_message`, `malformed_message`, `too_many_messages`, `self_connection`. |
| `user_agent_raw` | Exactly what the peer sent: TEXT if valid UTF-8, BLOB otherwise. It is filled in whenever the peer sent a `version`, even if the handshake then failed. |
| `implementation` | `zebra`, `zakura`, `zcashd` (`MagicBean`), `other` (well-formed, unrecognised name) or `unknown` (unparseable). Never dropped. |
| `version` | Normalised SemVer such as `2.6.0` or `2.0.0-rc.0`, without build metadata. NULL if unknown. |
| `protocol_version`, `start_height`, `services` | From the peer's `version`. `services` keeps all 64 bits in a signed integer. |
| `observed_at` | When the connection attempt started. |

`addresses` is zcrawl's address book: `address`, `port`, `first_seen`, `last_seen` and
`last_reachable`. An address counts as seen when a DNS seeder or a peer gives it to us, or when we
complete a handshake with it.

### User-agent parsing

A user agent must follow BIP 14: `/` followed by `name:version` components, each optionally
followed by a `(comment)` and closed by `/`. The first component named exactly `Zebra`, `Zakura`
or `MagicBean` decides the implementation, and its version must be `x.y.z[-pre][+build]`.
Examples:
- `/Zebra:2.5.0(linux; x86_64)/` → `zebra 2.5.0`
- `/MagicBean:6.0.0(bitcore)/` → `zcashd 6.0.0`
- `/zcrawl:0.1.0/` → `other`
- `/Zebra:2.5/` → `unknown`
- `hello` → `unknown`

## Limits of the measurement

- **Reachable nodes only.** Only nodes that accept inbound TCP connections are counted. Nodes
  behind NAT or firewalls, nodes with `listen_addr` on localhost, and nodes that never appear in
  gossip or DNS seeds are invisible. Those may be the nodes that upgrade slowest, or fastest.
- **Sybillable.** User agents are self-reported, unauthenticated strings. Anyone can run many
  cheap listeners claiming any version, or gossip fake addresses. That can move the curve in
  either direction. `--group-by subnet` limits, but does not remove, the effect of one operator
  with many addresses.
- **One node per address.** A node is an `IP:port` pair:
  - A node reachable over both IPv4 and IPv6 is counted twice.
  - Several nodes behind one IP on different ports count separately.
  - Several nodes behind one IP on the *same* port (for example a load balancer) count once.
- **Round granularity.** Times are only as fine as the round interval, 1 hour by default. A round
  that hits its time limit is still counted. Check `hit_time_limit` if that matters.
- **Discovery bias.** The node set depends on what the DNS seeders and peers return. Zebra serves
  only a sample of its address book to `getaddr`.
- **Release time is an input.** "Published" means whatever `--released-at` says. Nodes that ran a
  release candidate and then the release are counted from when they announce `>= x.y.z`.
- **Zakura P2P v2.** A Zakura node that is reachable only over its QUIC transport, and not over
  legacy TCP, would not be seen. Current Zakura listens on legacy TCP.

## Testing

```sh
cargo test
```

- `src/user_agent.rs`: the user-agent parser on Zebra, Zakura, MagicBean, comment fields,
  multiple components, and a list of malformed strings.
- `src/codec/tests.rs`: the message codec on fixed byte vectors whose checksums were computed
  independently. Covers an oversized length field, a wrong checksum, wrong magic, a bad command,
  truncation, a user agent over 256 bytes, `addr` count limits, `addrv2` with a Tor address
  that must be dropped, and a small deterministic fuzz loop.
- `tests/mock_peer.rs`: a mock TCP peer that completes a handshake, pings, and returns two
  addresses. Also covers a full round in which two mock peers advertise each other and a closed
  port (each probed exactly once), a second round seeded only from the address book, peers that
  send a bad checksum, an oversized header, or `version` and then hang up, and a round stopped
  mid-way.
- `tests/report_fixture.rs`: a fixture database with a known adoption curve, checking 50/80/95%
  times exactly for every `--denominator` and `--group-by` combination. Its rounds include an
  interrupted one, a still-running one and a pre-release one, all of which must be excluded.

### Manual check: one round against testnet

Run this by hand before trusting a new build or a protocol-version change:

```sh
cat > /tmp/zcrawl-testnet.toml <<'TOML'
network = "testnet"
database = "/tmp/zcrawl-testnet.sqlite"
round_time_limit_minutes = 15
TOML
cargo run --release -- --config /tmp/zcrawl-testnet.toml run --once
cargo run --release -- --config /tmp/zcrawl-testnet.toml status
```

What to look for:
- A `complete` round.
- Dozens of reachable Zebra and Zakura nodes with `protocol_version` 170180 or higher.
- Few `connection_closed` failures on port 18233. A peer that sends its `version` and then
  hangs up has usually rejected our protocol version.

Output from 2026-11-18, zcrawl 0.1.0, run from a development machine:

```text
Last complete round: 1
  started:         2026-11-18T11:54:17Z
  finished:        2026-11-18T11:55:30Z
  duration:        1m 13s
  addresses tried: 1310
  nodes reached:   53

implementation version                 nodes
zakura         1.8.9                       6
zakura         1.8.8                       1
zakura         1.8.7                       4
zakura         1.8.7-modified              1
zakura         1.8.5                       1
zakura         1.8.4                       2
zakura         1.8.4-modified              1
zakura         1.7.0                       1
zakura         1.6.0                       2
               (total)                    19
zebra          7.1.0                      15
zebra          7.1.0-modified              1
zebra          7.1.0-alpha.0               3
zebra          7.0.0                      11
zebra          7.0.0-rc.0                  1
zebra          6.3.0                       1
zebra          6.0.0-rc.0                  2
               (total)                    34

Unreachable addresses by reason:
  handshake_timeout        1231
  connection_closed          14
  bad_magic                  12
```

The 53 reached nodes are real: each completed a handshake with the testnet magic. The failure
breakdown is not representative. The machine this ran on sits behind a transparent proxy that
accepts every outbound TCP connection, including to `8.8.8.8:18233`. Unreachable addresses
therefore showed up as `handshake_timeout` instead of `connect_refused` or `connect_timeout`, and
the `bad_magic` replies came from the proxy or from a non-Zcash service. Run the check again from
the crawl server to get a clean baseline.

Most of the 1310 addresses were ephemeral ports. Peers gossip many `IP:port` pairs with
high-numbered ports for the same few hosts: one host appeared with 62 ports, and only one of
them answered. This costs probe time, not accuracy, because only completed handshakes are
counted.

## Assumptions to confirm

1. **Reachable means a completed handshake** (`version` and `verack` both received). A peer that
   sends `version` and then hangs up is stored, user agent included, but with `reachable = 0`,
   and is not counted.
2. **A node is an `IP:port` pair.** A dual-stack node therefore counts twice. Ports other than
   8233/18233 are crawled when gossiped.
3. **Implementation labels.** `MagicBean` is stored as `zcashd`. A well-formed user agent with an
   unrecognised name is `other`; an unparseable one is `unknown`. Names match case-sensitively,
   and the first recognised component wins. A recognised name with a bad version, such as
   `/Zebra:2.5/`, is `unknown`.
4. **Version order is SemVer**, so `2.6.0-rc.1 < 2.6.0 < 2.7.0-rc.1`. The exception is
   `x.y.z-modified` (Zebra's label for a dirty build of a release tag), which counts as `x.y.z`.
5. **"Seen in the last 7 days"** means an address a DNS seeder or peer gave us, or one we
   completed a handshake with, within 7 days by our clock. Peer-claimed `addr` timestamps are
   ignored. This needs a third table, `addresses`.
6. **Schema additions beyond the spec.** `rounds.status` (`running`/`complete`/`interrupted`),
   `rounds.hit_time_limit`, and a `meta` table that pins the network.
7. **Round time limit defaults to 45 minutes.** The spec did not give one. A round that hits it
   still counts as complete.
8. **Curve timing.** Each round is placed at its start time. Only rounds starting at or after
   `--released-at` are used. Time to X% is the first round at or above X%, with no
   interpolation.
9. **`--denominator all`** includes zcashd, `other` and `unknown` nodes.
10. **`--group-by subnet`** counts a subnet as upgraded only when *every* counted node in it is
    upgraded. Alternatives would be "any node" or "majority".
11. **Handshake fields.** zcrawl advertises protocol 170190 on both networks (as Zebra and Zakura
    do), services 0, `relay = false` and `start_height = 0`. It does not send `sendaddrv2`.
12. **`getaddr` handling.** zcrawl waits up to 10 s for an `addr` with more than one entry, and
    takes at most 1000 addresses per peer per round.
13. **Report layout.** Threshold times are printed after the CSV as `#` comment lines on stdout.
14. **Placeholders.** The contact URL (`CHANGE-ME`) and the license (`MIT OR Apache-2.0`) are
    placeholders.

## Design notes

**Why not an existing crawler?** Two candidates were checked in November 2026:
- **Zcash Foundation's [dnsseeder](https://github.com/ZcashFoundation/dnsseeder).** A Go CoreDNS
  plugin, last changed June 2026. It still advertises protocol 170150 (NU6.2), so post-NU7 Zebra
  nodes would disconnect it. It does not record user agents at all, and it has no storage beyond
  an in-memory address book.
- **The [Ziggurat](https://github.com/equilibriumco/ziggurat-zcash) crawler.** Updated for NU7
  on 2026-11-15, and it classifies Zebra and Zakura user agents. But it keeps all state in
  memory and writes one aggregate summary when it exits. It has no per-round snapshots,
  persistence or adoption report, and its continuous-crawl design has no notion of a round.
  Adding those would be most of this project, on top of the `pea2pea` and `ziggurat-core`
  dependencies.

**Why a minimal codec instead of `zebra-network`?** The crawler needs seven messages and must
record peers that Zebra itself would refuse, such as old zcashd versions or obsolete protocol
versions. `zebra-network` enforces Zebra's own peer policy during the handshake, and it brings in
`zebra-chain` and its cryptography dependency tree. A codec of a few hundred lines with bounded
allocation is easier to audit, and it does not tie zcrawl's release cycle to Zebra's.

## License

MIT OR Apache-2.0.
