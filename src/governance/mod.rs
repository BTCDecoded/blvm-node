//! Governance integration for bllvm-node
//!
//! Provides webhook integration with bllvm-commons for fee forwarding tracking

#[cfg(feature = "governance")]
pub mod webhook;

#[cfg(feature = "governance")]
pub use webhook::GovernanceWebhookClient;
