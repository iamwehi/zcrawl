//! Zcash network parameters.
//!
//! Every value here was read from the Zebra source at commit
//! `720141ff8b705bab4e57d88884f028276e4119d3` (2026-11-17), not written from
//! memory. The file and item each value came from are given next to it. The
//! Zakura source at `4546310f4d43604bffbe007bd5dd4d361ec1a23e` has the same
//! values for all of them.

use serde::Deserialize;

/// The protocol version zcrawl advertises in its `version` message.
///
/// `zebra-network/src/constants.rs`: `CURRENT_NETWORK_PROTOCOL_VERSION = 170_190`.
/// Zebra and Zakura advertise this one value on both Mainnet and Testnet.
pub const PROTOCOL_VERSION: u32 = 170_190;

/// Largest message payload Zebra accepts.
///
/// `zebra-chain/src/serialization/zcash_serialize.rs`: `MAX_PROTOCOL_MESSAGE_LEN`.
pub const MAX_PROTOCOL_MESSAGE_LEN: u32 = 2 * 1024 * 1024;

/// `zebra-network/src/protocol/external/message.rs`: `MAX_USER_AGENT_LENGTH`.
pub const MAX_USER_AGENT_LENGTH: usize = 256;

/// `zebra-network/src/constants.rs`: `MAX_ADDRS_IN_MESSAGE`.
pub const MAX_ADDRS_IN_MESSAGE: usize = 1000;

/// `zebra-network/src/protocol/external/addr/v2.rs`: `MAX_ADDR_V2_ADDR_SIZE`.
pub const MAX_ADDR_V2_ADDR_SIZE: usize = 512;

/// The user agent zcrawl advertises, `/zcrawl:<version>/`.
pub fn user_agent() -> String {
    format!("/zcrawl:{}/", env!("CARGO_PKG_VERSION"))
}

/// The Zcash network to crawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Mainnet,
    Testnet,
}

impl Network {
    /// Message header magic bytes, in wire order.
    ///
    /// `zebra-chain/src/parameters/constants.rs`: `magics::MAINNET`, `magics::TESTNET`.
    pub fn magic(self) -> [u8; 4] {
        match self {
            Network::Mainnet => [0x24, 0xe9, 0x27, 0x64],
            Network::Testnet => [0xfa, 0x1a, 0xf9, 0xbf],
        }
    }

    /// `zebra-chain/src/parameters/network.rs`: `Network::default_port`.
    pub fn default_port(self) -> u16 {
        match self {
            Network::Mainnet => 8233,
            Network::Testnet => 18233,
        }
    }

    /// `zebra-network/src/config.rs`: `DEFAULT_MAINNET_PEERS`, `DEFAULT_TESTNET_PEERS`.
    pub fn dns_seeders(self) -> &'static [&'static str] {
        match self {
            Network::Mainnet => &[
                "dnsseed.str4d.xyz:8233",
                "dnsseed.z.cash:8233",
                "mainnet.seeder.shieldedinfra.net:8233",
                "mainnet.seeder.zfnd.org:8233",
                "seeder.zec.rocks:8233",
            ],
            Network::Testnet => &[
                "dnsseed.testnet.z.cash:18233",
                "seeder.testnet.zec.rocks:18233",
                "testnet.seeder.zfnd.org:18233",
            ],
        }
    }

    /// The lowest protocol version a Zebra node near the chain tip accepts from a
    /// peer once NU7 is active. Shown for information; zcrawl itself accepts and
    /// records peers of any version.
    ///
    /// `zebra-network/src/protocol/external/types.rs`:
    /// `Version::min_specified_for_upgrade` for `Nu7`.
    pub fn nu7_min_peer_protocol_version(self) -> u32 {
        match self {
            Network::Mainnet => 170_190,
            Network::Testnet => 170_180,
        }
    }

    /// NU7 activation height.
    ///
    /// `zebra-chain/src/parameters/constants.rs`: `activation_heights::{mainnet,testnet}::NU7`.
    pub fn nu7_activation_height(self) -> u32 {
        match self {
            Network::Mainnet => 3_543_405,
            Network::Testnet => 4_465_026,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_is_bip14() {
        let ua = user_agent();
        assert!(ua.starts_with("/zcrawl:") && ua.ends_with('/'));
        assert!(ua.len() <= MAX_USER_AGENT_LENGTH);
    }

    #[test]
    fn advertised_version_meets_nu7_minimum_on_both_networks() {
        for n in [Network::Mainnet, Network::Testnet] {
            assert!(PROTOCOL_VERSION >= n.nu7_min_peer_protocol_version());
        }
    }
}
