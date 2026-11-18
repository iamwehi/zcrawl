//! TOML configuration.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::netaddr;
use crate::params::{self, Network};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `mainnet` or `testnet`. Selects magic bytes, default port and seeders.
    pub network: Network,
    /// SQLite database file. Created if missing.
    pub database: PathBuf,
    /// Minutes between the starts of two rounds.
    pub round_interval_minutes: u64,
    /// A round stops starting new connections after this many minutes.
    pub round_time_limit_minutes: u64,
    /// Addresses seen within this many days seed the next round.
    pub address_retention_days: u64,
    pub connect_timeout_secs: u64,
    pub handshake_timeout_secs: u64,
    /// How long to wait for an `addr` reply after sending `getaddr`.
    pub getaddr_timeout_secs: u64,
    /// Maximum simultaneous connections.
    pub max_connections: usize,
    /// At most this many addresses are taken from one peer per round.
    pub max_addrs_per_peer: usize,
    /// DNS seeders as `host:port`. Defaults to Zebra's list for the network.
    pub seeders: Option<Vec<String>>,
    /// Extra peers to crawl every round, as `host:port` or `ip:port`.
    pub extra_peers: Vec<String>,
    /// Protocol version advertised in `version`. Defaults to Zebra's current one.
    pub protocol_version: Option<u32>,
    /// Crawl private, loopback and other non-global addresses. For tests only.
    pub allow_non_routable: bool,
    /// Accept inbound connections on this address, e.g. `[::]:8233`. Unset
    /// means zcrawl only crawls. Requires `external_addr`.
    pub listen_addr: Option<SocketAddr>,
    /// The public `ip:port` other nodes can reach the listener on. It is
    /// announced to every crawled peer.
    pub external_addr: Option<SocketAddr>,
    /// Maximum simultaneous inbound connections. Extra ones are dropped.
    pub max_inbound_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: Network::Mainnet,
            database: PathBuf::from("zcrawl.sqlite"),
            round_interval_minutes: 60,
            round_time_limit_minutes: 45,
            address_retention_days: 7,
            connect_timeout_secs: 5,
            handshake_timeout_secs: 10,
            getaddr_timeout_secs: 10,
            max_connections: 200,
            max_addrs_per_peer: 1000,
            seeders: None,
            extra_peers: Vec::new(),
            protocol_version: None,
            allow_non_routable: false,
            listen_addr: None,
            external_addr: None,
            max_inbound_connections: 64,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let positive = [
            ("round_interval_minutes", self.round_interval_minutes),
            ("round_time_limit_minutes", self.round_time_limit_minutes),
            ("address_retention_days", self.address_retention_days),
            ("connect_timeout_secs", self.connect_timeout_secs),
            ("handshake_timeout_secs", self.handshake_timeout_secs),
            ("getaddr_timeout_secs", self.getaddr_timeout_secs),
            ("max_connections", self.max_connections as u64),
            ("max_addrs_per_peer", self.max_addrs_per_peer as u64),
        ];
        for (name, value) in positive {
            if value == 0 {
                bail!("{name} must be greater than zero");
            }
        }
        if self.max_connections > 4096 {
            bail!("max_connections must be at most 4096");
        }
        if self.round_time_limit_minutes > 24 * 60 || self.round_interval_minutes > 7 * 24 * 60 {
            bail!("round_time_limit_minutes or round_interval_minutes is implausibly large");
        }
        if self.address_retention_days > 3650 {
            bail!("address_retention_days must be at most 3650");
        }
        match (self.listen_addr, self.external_addr) {
            (Some(_), None) => {
                bail!("listen_addr needs external_addr, the public ip:port other nodes can reach")
            }
            (None, Some(_)) => bail!("external_addr is only used together with listen_addr"),
            (Some(_), Some(ext)) => {
                if !self.allow_non_routable && !netaddr::is_crawlable(ext) {
                    bail!("external_addr {ext} is not a public address other nodes can reach");
                }
                if self.max_inbound_connections == 0 || self.max_inbound_connections > 4096 {
                    bail!("max_inbound_connections must be between 1 and 4096");
                }
            }
            (None, None) => {}
        }
        Ok(())
    }

    pub fn seeders(&self) -> Vec<String> {
        match &self.seeders {
            Some(s) => s.clone(),
            None => self
                .network
                .dns_seeders()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    pub fn protocol_version(&self) -> u32 {
        self.protocol_version.unwrap_or(params::PROTOCOL_VERSION)
    }

    /// The address announced to peers, if the listener is enabled.
    pub fn announce_addr(&self) -> Option<SocketAddr> {
        self.listen_addr
            .and(self.external_addr)
            .map(netaddr::normalize)
    }

    pub fn round_interval(&self) -> Duration {
        Duration::from_secs(self.round_interval_minutes * 60)
    }

    pub fn round_time_limit(&self) -> Duration {
        Duration::from_secs(self.round_time_limit_minutes * 60)
    }

    pub fn retention(&self) -> Duration {
        Duration::from_secs(self.address_retention_days * 24 * 60 * 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.network, Network::Mainnet);
        assert_eq!(c.round_interval_minutes, 60);
        assert_eq!(c.address_retention_days, 7);
        assert_eq!(c.connect_timeout_secs, 5);
        assert_eq!(c.handshake_timeout_secs, 10);
        assert_eq!(c.max_connections, 200);
        assert_eq!(c.protocol_version(), 170_190);
        assert_eq!(c.seeders().len(), 5);
        c.validate().unwrap();
    }

    #[test]
    fn testnet_switches_seeders() {
        let c: Config = toml::from_str("network = \"testnet\"").unwrap();
        assert_eq!(c.network, Network::Testnet);
        assert_eq!(c.network.default_port(), 18233);
        assert!(c.seeders().iter().all(|s| s.ends_with(":18233")));
    }

    #[test]
    fn rejects_unknown_keys_and_zero() {
        assert!(toml::from_str::<Config>("netwrok = \"testnet\"").is_err());
        let c: Config = toml::from_str("max_connections = 0").unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn listener_needs_a_public_external_addr() {
        let c: Config = toml::from_str("listen_addr = \"[::]:8233\"").unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str("external_addr = \"203.0.114.1:8233\"").unwrap();
        assert!(c.validate().is_err());
        let c: Config =
            toml::from_str("listen_addr = \"[::]:8233\"\nexternal_addr = \"192.168.1.2:8233\"")
                .unwrap();
        assert!(c.validate().is_err());
        let c: Config =
            toml::from_str("listen_addr = \"[::]:8233\"\nexternal_addr = \"8.8.8.8:8233\"")
                .unwrap();
        c.validate().unwrap();
        assert_eq!(c.announce_addr(), Some("8.8.8.8:8233".parse().unwrap()));
        assert_eq!(Config::default().announce_addr(), None);
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../contrib/zcrawl.toml");
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
    }
}
