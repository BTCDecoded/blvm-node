//! Unified BIP70 Payment Protocol
//!
//! Provides transport-agnostic payment processing that works over both HTTP and P2P.
//! Supports module payments with 75/15/10 split (author/commons/node).
//! Supports CTV (CheckTemplateVerify) covenants for instant payment proofs.

pub mod processor;

#[cfg(feature = "bip70-http")]
pub mod http;

#[cfg(feature = "ctv")]
pub mod covenant;

#[cfg(feature = "ctv")]
pub mod vault;

#[cfg(feature = "ctv")]
pub mod pool;

#[cfg(feature = "ctv")]
pub mod congestion;

pub mod settlement;
pub mod state_machine;

pub use processor::PaymentProcessor;

#[cfg(feature = "bip70-http")]
pub use http::handle_payment_routes;

#[cfg(feature = "ctv")]
pub use covenant::{CovenantEngine, CovenantProof, SettlementStatus, TransactionTemplate};

#[cfg(feature = "ctv")]
pub use vault::{VaultConfig, VaultEngine, VaultLifecycle, VaultState};

#[cfg(feature = "ctv")]
pub use pool::{PoolConfig, PoolEngine, PoolParticipant, PoolState, PoolTransaction};

#[cfg(feature = "ctv")]
pub use congestion::{
    BatchConfig, CongestionManager, CongestionMetrics, PendingTransaction, TransactionBatch,
    TransactionPriority,
};

pub use settlement::SettlementMonitor;
pub use state_machine::{PaymentState, PaymentStateMachine};
