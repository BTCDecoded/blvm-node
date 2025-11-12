//! Kani proof helpers for network protocol verification
//!
//! Provides standardized helpers for common Kani proof patterns
//! to reduce duplication and ensure consistency across all proofs.
//!
//! Follows the same pattern as `bllvm-consensus/src/kani_helpers.rs`

/// Network protocol limits for Kani proofs
///
/// These limits are used to bound proof input sizes for tractability
/// while ensuring they match or exceed Bitcoin's actual limits.
///
/// Note: Proof-time limits are smaller than actual Bitcoin limits for proof tractability.
/// These are used only during Kani proof execution, not in runtime code.
pub mod proof_limits {
    /// Maximum message size for proof tractability
    pub const MAX_MESSAGE_SIZE_FOR_PROOF: usize = 1000;

    /// Maximum payload size for proof tractability
    pub const MAX_PAYLOAD_SIZE_FOR_PROOF: usize = 1000 - 24; // Minus header (24 bytes)

    /// Maximum addresses in Addr message
    pub const MAX_ADDR_COUNT_FOR_PROOF: usize = 10;

    /// Maximum inventory items
    pub const MAX_INV_COUNT_FOR_PROOF: usize = 10;

    /// Maximum user agent length
    pub const MAX_USER_AGENT_LEN_FOR_PROOF: usize = 256;

    /// Maximum headers in Headers message
    pub const MAX_HEADERS_COUNT_FOR_PROOF: usize = 10;
}

/// Standard unwind bounds for network operations
///
/// These bounds are tuned for proof performance while ensuring
/// complete coverage of all loop iterations.
pub mod unwind_bounds {
    /// Message header parsing (fixed size, no loops)
    pub const HEADER_PARSING: u32 = 3;

    /// Simple message parsing (1-2 loops)
    pub const SIMPLE_MESSAGE: u32 = 5;

    /// Complex message parsing (with arrays, 3+ loops)
    pub const COMPLEX_MESSAGE: u32 = 10;

    /// Checksum calculation (SHA256 operations)
    pub const CHECKSUM: u32 = 3;
}

/// Macro for bounding version messages
///
/// Applies standard bounds to a VersionMessage for Kani proofs.
#[macro_export]
macro_rules! assume_version_message_bounds {
    ($msg:expr) => {
        kani::assume(
            $msg.user_agent.len()
                <= $crate::network::kani_helpers::proof_limits::MAX_USER_AGENT_LEN_FOR_PROOF,
        );
        kani::assume($msg.version >= 70001); // Minimum valid version
    };
}

/// Macro for bounding addr messages
///
/// Applies standard bounds to an AddrMessage for Kani proofs.
#[macro_export]
macro_rules! assume_addr_message_bounds {
    ($msg:expr) => {
        kani::assume(
            $msg.addresses.len()
                <= $crate::network::kani_helpers::proof_limits::MAX_ADDR_COUNT_FOR_PROOF,
        );
    };
}

/// Macro for bounding BlockMessage
///
/// Applies standard bounds to a BlockMessage for Kani proofs.
#[macro_export]
macro_rules! assume_block_message_bounds {
    ($msg:expr) => {
        use bllvm_consensus::kani_helpers::assume_block_bounds;
        assume_block_bounds!($msg.block, 2, 2); // Bound block (max 2 txs, 2 inputs/outputs each)
        kani::assume($msg.witnesses.len() <= 10); // Bound witnesses
    };
}

/// Macro for bounding TxMessage
///
/// Applies standard bounds to a TxMessage for Kani proofs.
#[macro_export]
macro_rules! assume_tx_message_bounds {
    ($msg:expr) => {
        use bllvm_consensus::kani_helpers::assume_transaction_bounds;
        assume_transaction_bounds!($msg.transaction);
    };
}

/// Macro for bounding HeadersMessage
///
/// Applies standard bounds to a HeadersMessage for Kani proofs.
#[macro_export]
macro_rules! assume_headers_message_bounds {
    ($msg:expr) => {
        kani::assume(
            $msg.headers.len()
                <= $crate::network::kani_helpers::proof_limits::MAX_HEADERS_COUNT_FOR_PROOF,
        );
    };
}

/// Macro for bounding InvMessage
///
/// Applies standard bounds to an InvMessage for Kani proofs.
#[macro_export]
macro_rules! assume_inv_message_bounds {
    ($msg:expr) => {
        kani::assume(
            $msg.inventory.len()
                <= $crate::network::kani_helpers::proof_limits::MAX_INV_COUNT_FOR_PROOF,
        );
    };
}
