//! TOML configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

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
    fn example_config_parses() {
        let text = include_str!("../contrib/zcrawl.toml");
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
    }
}
