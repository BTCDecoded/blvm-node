//! Stratum V2 Protocol Implementation
//!
//! Provides Stratum V2 mining protocol support for reference-node, enabling
//! efficient binary protocol (50-66% bandwidth savings), encrypted communication,
//! and merge mining coordination via multiplexed channels.
//!
//! Works with both TCP and Iroh transports via the transport abstraction layer.
//!
//! This module is conditionally compiled using the `stratum-v2` feature flag.

#[cfg(feature = "stratum-v2")]
pub mod client;
#[cfg(feature = "stratum-v2")]
pub mod error;
#[cfg(feature = "stratum-v2")]
pub mod merge_mining;
#[cfg(feature = "stratum-v2")]
pub mod messages;
#[cfg(feature = "stratum-v2")]
pub mod miner;
#[cfg(feature = "stratum-v2")]
pub mod pool;
#[cfg(feature = "stratum-v2")]
pub mod protocol;
#[cfg(feature = "stratum-v2")]
pub mod server;

#[cfg(feature = "stratum-v2")]
pub use client::StratumV2Client;
#[cfg(feature = "stratum-v2")]
pub use error::StratumV2Error;
#[cfg(feature = "stratum-v2")]
pub use merge_mining::{
    ChainStatistics, MergeMiningCoordinator, RevenueDistribution, SecondaryChain,
};
#[cfg(feature = "stratum-v2")]
pub use messages::*;
#[cfg(feature = "stratum-v2")]
pub use miner::StratumV2Miner;
#[cfg(feature = "stratum-v2")]
pub use pool::StratumV2Pool;
#[cfg(feature = "stratum-v2")]
pub use protocol::{TlvDecoder, TlvEncoder};
#[cfg(feature = "stratum-v2")]
pub use server::StratumV2Server;
