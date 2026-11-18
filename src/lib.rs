//! zcrawl: a Zcash p2p crawler that records which node software and version
//! every reachable node runs, so release adoption can be measured over time.

#![forbid(unsafe_code)]

pub mod codec;
pub mod config;
pub mod crawler;
pub mod db;
pub mod netaddr;
pub mod params;
pub mod peer;
pub mod report;
pub mod status;
pub mod user_agent;
