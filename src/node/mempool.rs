//! Mempool manager
//!
//! Handles transaction mempool management, validation, and relay.

use crate::config::{MempoolPolicyConfig, RbfConfig};
use crate::node::event_publisher::EventPublisher;
use crate::node::fee_estimator::{BlockObservation, FeeEstimator, TxAdmission};
use anyhow::Result;
use blvm_protocol::mempool::{Mempool, has_conflict_with_tx, replacement_checks_with_witness};
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, Hash, OutPoint, Transaction, TransactionOutput, UtxoSet};
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{debug, info, warn};

/// RBF tracking information for a transaction
#[derive(Debug, Clone)]
struct RbfTracking {
    /// Number of times this transaction has been replaced
    replacement_count: u32,
    /// Timestamp of last replacement (Unix timestamp)
    last_replacement_time: u64,
    /// Original transaction hash (before any replacements)
    original_tx_hash: Hash,
}

fn effective_ranked_fee(fee: u64, delta: i64) -> u64 {
    (fee as i64).saturating_add(delta).max(0) as u64
}

fn next_replacement_count(counts: impl IntoIterator<Item = u32>) -> u32 {
    counts.into_iter().max().unwrap_or(0).saturating_add(1)
}

/// True when `new_fee_scaled / existing_fee_scaled` is strictly above `multiplier`.
///
/// The multiplier is applied in integer millionths. A non-finite multiplier,
/// including Disabled, accepts nothing.
pub(crate) fn replacement_clears_fee_rate(
    new_fee_scaled: u128,
    existing_fee_scaled: u128,
    multiplier: f64,
) -> Result<bool> {
    if !multiplier.is_finite() {
        return Ok(false);
    }
    let millis = multiplier * 1_000_000.0;
    if !millis.is_finite() || millis < 0.0 {
        return Ok(false);
    }
    let millis = millis.round() as u128;
    let required = existing_fee_scaled
        .checked_mul(millis)
        .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?
        / 1_000_000;
    Ok(new_fee_scaled > required)
}

/// Core mempool state (transactions + spent outputs)
/// Wrapped in Mutex for add_transaction from Arc context (re-broadcast, sendrawtransaction)
struct MempoolPool {
    transactions: HashMap<Hash, Transaction>,
    /// Witness transaction id recorded at admission, keyed by txid.
    wtxids: HashMap<Hash, Hash>,
    /// SegWit witness stacks per txid (one stack per input), when known at accept time.
    tx_witnesses: HashMap<Hash, Vec<Witness>>,
    /// Sigop-adjusted virtual size recorded at accept time.
    adjusted_vsize: HashMap<Hash, u64>,
    spent_outputs: HashSet<OutPoint>,
    /// Spender txid for each outpoint the pool is spending.
    spenders: HashMap<OutPoint, Hash>,
}

/// Mempool manager
pub struct MempoolManager {
    /// Transaction mempool + spent outputs (interior mutability for Arc<MempoolManager>)
    pool: Mutex<MempoolPool>,
    /// Legacy mempool (HashSet of hashes) for compatibility
    #[allow(dead_code)]
    mempool: RwLock<Mempool>,
    /// Shared UTXO set for fee calculation (set via set_utxo_set_arc after construction).
    /// When None, RBF fee checks fall back to zero-fee comparisons and replacement is
    /// rejected. Callers should wire this to the node's live UTXO set.
    utxo_set_arc: RwLock<Option<Arc<tokio::sync::Mutex<UtxoSet>>>>,
    /// Sorted index by fee rate (descending) - Reverse<u64> for descending order
    /// Maps fee_rate -> Vec<Hash> (multiple transactions can have same fee rate)
    /// Uses RwLock for interior mutability to allow &self methods
    fee_index: RwLock<BTreeMap<Reverse<u64>, Vec<Hash>>>,
    /// Cache fee rates per transaction hash (sat/vB).
    /// Uses RwLock for interior mutability to allow &self methods
    fee_cache: RwLock<HashMap<Hash, u64>>,
    /// Absolute fee and vsize used for the BIP133 filter. The sat/vB cache drops the remainder.
    fee_basis: RwLock<HashMap<Hash, (u64, u64)>>,
    /// Mining priority fee deltas (satoshis), cumulative per BIP prioritisetransaction semantics.
    fee_deltas: RwLock<HashMap<Hash, i64>>,
    /// RBF configuration (optional)
    /// Uses RwLock for interior mutability to allow setting config after Arc sharing
    rbf_config: RwLock<Option<RbfConfig>>,
    /// Mempool policy configuration (optional)
    /// Uses RwLock for interior mutability to allow setting config after Arc sharing
    policy_config: RwLock<Option<MempoolPolicyConfig>>,
    /// RBF tracking: transaction hash -> RBF tracking info
    /// Uses RwLock for interior mutability
    rbf_tracking: RwLock<HashMap<Hash, RbfTracking>>,
    /// Transaction timestamps: when each transaction was added
    /// Uses RwLock for interior mutability
    tx_timestamps: RwLock<HashMap<Hash, u64>>,
    /// Chain tip height when each transaction was accepted.
    tx_heights: RwLock<HashMap<Hash, u64>>,
    /// Transaction dependency graph: child -> parent relationships
    /// Maps transaction hash to set of parent transaction hashes (transactions it depends on)
    /// Uses RwLock for interior mutability
    tx_dependencies: RwLock<HashMap<Hash, HashSet<Hash>>>,
    /// Reverse dependency graph: parent -> children relationships
    /// Maps transaction hash to set of child transaction hashes (transactions that depend on it)
    /// Uses RwLock for interior mutability
    tx_descendants: RwLock<HashMap<Hash, HashSet<Hash>>>,
    /// UTXO set hash for change detection (optimization: only recalculate when UTXO set changes)
    /// Uses RwLock for interior mutability
    utxo_set_hash: RwLock<Option<u64>>,
    /// Event publisher for mempool events (optional)
    /// Uses Arc for shared ownership and interior mutability
    event_publisher: RwLock<Option<Arc<EventPublisher>>>,
    /// Live chain, so admission can read the next block's height and median time.
    storage: RwLock<Option<Arc<crate::storage::Storage>>>,
    /// Point-read for a prevout the in-memory set does not hold.
    ///
    /// The steady-state set stays empty. Coins live in the IBD snapshot, or in
    /// the chainstate table when that snapshot has no rows. Inserting a hit
    /// into the shared set would make the next block call `store_utxo_set`
    /// on a fragment.
    snapshot_prevout: RwLock<Option<SnapshotPrevoutLookup>>,
    /// Test tip: next-block height and median time-past. Ignored when `storage` is set.
    chain_tip: RwLock<Option<(u64, u64)>>,
    /// Transactions in the package being admitted whose descendant fee covers the relay floor.
    relay_floor_waivers: Mutex<HashSet<Hash>>,
    /// Zero-fee parents whose single dust output is spent by a later transaction in the package.
    ephemeral_dust_waivers: Mutex<HashSet<Hash>>,
    /// Earlier transactions in a package dry-run, with adjusted virtual size and parent ids.
    package_preview: Mutex<PackagePreview>,
    /// Built-in fee estimator. Hooks run only after this manager has dropped its locks.
    fee_estimator: RwLock<Option<Arc<FeeEstimator>>>,
    /// Set for the whole of [`Self::load_from_disk`]. Restored transactions stay out of history.
    loading_from_disk: AtomicBool,
    /// Set while [`Self::add_package_with_witnesses`] is admitting its transactions.
    admitting_package: AtomicBool,
}

/// Sets `flag` for the guard's lifetime, including a panic unwind.
struct FlagHold<'a>(&'a AtomicBool);

impl<'a> FlagHold<'a> {
    fn enter(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}

impl Drop for FlagHold<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

type SnapshotPrevoutLookup = Arc<dyn Fn(&OutPoint) -> Option<blvm_protocol::UTXO> + Send + Sync>;

/// Transactions already accepted inside the package being previewed.
#[derive(Clone, Default)]
struct PackagePreview {
    /// Transaction id to adjusted virtual size and the parent ids this preview can see.
    txs: HashMap<Hash, (u64, Vec<Hash>)>,
    /// Direct children recorded while previewing the package.
    descendants: HashMap<Hash, HashSet<Hash>>,
}

fn ibd_snapshot_prevout(
    storage: &crate::storage::Storage,
    prevout: &OutPoint,
) -> Option<blvm_protocol::UTXO> {
    storage.read_chain_prevout(prevout).ok().flatten()
}

/// Next block height and the median time of the headers before that block.
pub(crate) fn next_block_finality(storage: &crate::storage::Storage) -> Option<(u64, u64)> {
    let tip = storage.chain().get_height().ok().flatten()?;
    let next_height = tip.saturating_add(1);
    let median_time_past = storage
        .blocks()
        .headers_before_height_for_mtp(next_height)
        .ok()
        .filter(|headers| !headers.is_empty())
        .map(|headers| blvm_protocol::bip113::get_median_time_past(&headers))
        .unwrap_or(0);
    Some((next_height, median_time_past))
}

fn is_p2sh_script(script: &[u8]) -> bool {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160, PUSH_20_BYTES};
    script.len() == 23
        && script[0] == OP_HASH160
        && script[1] == PUSH_20_BYTES
        && script[22] == OP_EQUAL
}

/// Last push in a push-only script, which is the redeem script of a P2SH spend.
///
/// `OP_0` and the small numeric opcodes have an empty payload. A non-push opcode
/// rejects the script. The returned slice borrows `script`.
fn last_script_push(script: &[u8]) -> Option<&[u8]> {
    use blvm_protocol::opcodes::{
        OP_0, OP_1, OP_1NEGATE, OP_16, OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4,
    };
    let mut i = 0;
    let mut last = None;
    while i < script.len() {
        let opcode = script[i];
        let (advance, data): (usize, &[u8]) = if opcode == OP_0 {
            (1, &[])
        } else if opcode > OP_0 && opcode < OP_PUSHDATA1 {
            let len = opcode as usize;
            if i + 1 + len > script.len() {
                return None;
            }
            (1 + len, &script[i + 1..i + 1 + len])
        } else if opcode == OP_PUSHDATA1 {
            if i + 1 >= script.len() {
                return None;
            }
            let len = script[i + 1] as usize;
            if i + 2 + len > script.len() {
                return None;
            }
            (2 + len, &script[i + 2..i + 2 + len])
        } else if opcode == OP_PUSHDATA2 {
            if i + 2 >= script.len() {
                return None;
            }
            let len = u16::from_le_bytes([script[i + 1], script[i + 2]]) as usize;
            if i + 3 + len > script.len() {
                return None;
            }
            (3 + len, &script[i + 3..i + 3 + len])
        } else if opcode == OP_PUSHDATA4 {
            if i + 4 >= script.len() {
                return None;
            }
            let len =
                u32::from_le_bytes([script[i + 1], script[i + 2], script[i + 3], script[i + 4]])
                    as usize;
            if i + 5 + len > script.len() {
                return None;
            }
            (5 + len, &script[i + 5..i + 5 + len])
        } else if opcode == OP_1NEGATE || (OP_1..=OP_16).contains(&opcode) {
            (1, &[])
        } else {
            return None;
        };
        last = Some(data);
        i += advance;
    }
    last
}

fn script_needs_witness_verification(script: &[u8]) -> bool {
    blvm_protocol::witness::is_any_witness_program(script)
}

fn p2sh_redeem_is_witness_program(script_pubkey: &[u8], script_sig: &[u8]) -> bool {
    if !is_p2sh_script(script_pubkey) {
        return false;
    }
    let Some(redeem) = last_script_push(script_sig) else {
        return false;
    };
    script_needs_witness_verification(redeem)
}

/// BIP141 standard transaction weight. A relay transaction above this is non-standard.
const MAX_STANDARD_TX_WEIGHT: u64 = 400_000;

/// A relay transaction smaller than this, witness excluded, is not admitted.
/// A 64-byte transaction can collide with a merkle node.
const MIN_STANDARD_TX_NONWITNESS_SIZE: usize = 65;

/// A standard input's scriptSig is at most this many bytes.
const MAX_STANDARD_SCRIPTSIG_SIZE: usize = 1_650;

/// A standard `OP_RETURN` output is at most this many bytes.
/// The budget is the standard transaction weight divided by four.
const MAX_STANDARD_OP_RETURN_SIZE: usize = (MAX_STANDARD_TX_WEIGHT / 4) as usize;

/// True for a standard output script: P2PKH, P2SH, P2PK, a witness program, or an `OP_RETURN`.
///
/// Version 0 is standard only at 20 or 32 bytes. Any other witness version, `OP_1`
/// through `OP_16`, is standard for a direct push of 2–40 bytes. An `OP_RETURN`
/// is standard up to [`MAX_STANDARD_OP_RETURN_SIZE`] bytes when every following
/// byte is a push.
fn output_script_is_standard(script: &[u8]) -> bool {
    if script.is_empty() {
        return false;
    }
    if script[0] == 0x6a {
        return script.len() <= MAX_STANDARD_OP_RETURN_SIZE
            && op_return_payload_is_push_only(script);
    }
    if blvm_protocol::witness::is_any_witness_program(script)
        && !blvm_protocol::witness::is_invalid_v0_witness_program(script)
    {
        return true;
    }
    if script.len() == 25
        && script[0] == 0x76
        && script[1] == 0xa9
        && script[2] == 0x14
        && script[23] == 0x88
        && script[24] == 0xac
    {
        return true;
    }
    if script.len() == 23 && script[0] == 0xa9 && script[1] == 0x14 && script[22] == 0x87 {
        return true;
    }
    if script.len() == 35
        && script[0] == 0x21
        && (script[1] == 0x02 || script[1] == 0x03)
        && script[34] == 0xac
    {
        return true;
    }
    script.len() == 67 && script[0] == 0x41 && script[1] == 0x04 && script[66] == 0xac
        || bare_multisig_is_standard(script)
}

/// A spent script Core's solver recognizes. A larger bare multisig is still that type.
fn spent_script_is_standard(script: &[u8]) -> bool {
    output_script_is_standard(script) || bare_multisig_within(script, 16)
}

/// Bare `OP_m <keys> OP_n OP_CHECKMULTISIG` with `1 <= m <= n <= 3`.
///
/// Each key is a 33-byte compressed push or a 65-byte uncompressed push.
fn bare_multisig_is_standard(script: &[u8]) -> bool {
    bare_multisig_within(script, 3)
}

fn bare_multisig_within(script: &[u8], max_keys: u8) -> bool {
    use blvm_protocol::opcodes::{OP_1, OP_16, OP_CHECKMULTISIG, PUSH_33_BYTES, PUSH_65_BYTES};

    if script.len() < 4 || script[script.len() - 1] != OP_CHECKMULTISIG {
        return false;
    }
    let n_op = script[script.len() - 2];
    if n_op < OP_1 || n_op > OP_1 + max_keys.saturating_sub(1) {
        return false;
    }
    let n = (n_op - OP_1 + 1) as usize;
    let m_op = script[0];
    if !(OP_1..=OP_16).contains(&m_op) {
        return false;
    }
    let m = (m_op - OP_1 + 1) as usize;
    if m == 0 || m > n {
        return false;
    }
    let mut i = 1usize;
    for _ in 0..n {
        if i >= script.len() - 2 {
            return false;
        }
        let (header, pk_len) = if script[i] == PUSH_33_BYTES {
            (script.get(i + 1).copied(), 33)
        } else if script[i] == PUSH_65_BYTES {
            (script.get(i + 1).copied(), 65)
        } else {
            return false;
        };
        let Some(header) = header else {
            return false;
        };
        let compressed = header == 0x02 || header == 0x03;
        let uncompressed = header == 0x04;
        if pk_len == 33 && !compressed || pk_len == 65 && !uncompressed {
            return false;
        }
        let next = i + 1 + pk_len;
        if next > script.len() - 2 {
            return false;
        }
        i = next;
    }
    i == script.len() - 2
}

/// Bytes after `OP_RETURN` are a data carrier only when each one is a push.
///
/// An opcode above `OP_16` is not a push. A truncated push does not count.
fn op_return_payload_is_push_only(script: &[u8]) -> bool {
    let mut i = 1usize;
    while i < script.len() {
        let opcode = script[i];
        if opcode > 0x60 {
            return false;
        }
        if opcode <= 0x4b {
            let len = opcode as usize;
            if i + 1 + len > script.len() {
                return false;
            }
            i += 1 + len;
            continue;
        }
        let (header, len) = if opcode == 0x4c {
            (2usize, script.get(i + 1).copied().map(|n| n as usize))
        } else if opcode == 0x4d {
            (
                3usize,
                script
                    .get(i + 1..i + 3)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize),
            )
        } else if opcode == 0x4e {
            (
                5usize,
                script
                    .get(i + 1..i + 5)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize),
            )
        } else {
            i += 1;
            continue;
        };
        let Some(len) = len else {
            return false;
        };
        if i + header + len > script.len() {
            return false;
        }
        i += header + len;
    }
    true
}

/// Spendable outputs below `n_size * 3` satoshis are dust.
///
/// `n_size` is the output's wire size plus the bytes a spend of that output adds.
/// An `OP_RETURN` output is unspendable and is not dust.
fn output_is_dust(output: &blvm_protocol::TransactionOutput) -> bool {
    if output.script_pubkey.first().copied() == Some(0x6a) {
        return false;
    }
    let script_len = output.script_pubkey.len() as u64;
    let compact = if script_len < 253 {
        1
    } else if script_len <= 0xffff {
        3
    } else {
        5
    };
    let spend = if blvm_protocol::witness::is_any_witness_program(output.script_pubkey.as_ref()) {
        32 + 4 + 1 + (107 / 4) + 4
    } else {
        32 + 4 + 1 + 107 + 4
    };
    let n_size = 8 + compact + script_len + spend;
    let threshold = n_size.saturating_mul(3000) / 1000;
    output.value < threshold as i64
}

/// A standard pay-to-script-hash redeem script has at most this many signature checks.
const MAX_STANDARD_P2SH_SIGOPS: u32 = 15;

/// Last data push in a push-only scriptSig. That push is the redeem script.
fn last_push_payload(script: &[u8]) -> Option<Vec<u8>> {
    if script.is_empty() {
        return None;
    }
    let mut i = 0usize;
    let mut last = None;
    while i < script.len() {
        let opcode = script[i];
        if opcode > 0x60 {
            return None;
        }
        if opcode <= 0x4b {
            let len = opcode as usize;
            if i + 1 + len > script.len() {
                return None;
            }
            last = Some(script[i + 1..i + 1 + len].to_vec());
            i += 1 + len;
            continue;
        }
        let (header, len) = if opcode == 0x4c {
            (2usize, script.get(i + 1).copied().map(|n| n as usize))
        } else if opcode == 0x4d {
            (
                3usize,
                script
                    .get(i + 1..i + 3)
                    .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize),
            )
        } else if opcode == 0x4e {
            (
                5usize,
                script.get(i + 1..i + 5).map(|bytes| {
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
                }),
            )
        } else {
            last = Some(Vec::new());
            i += 1;
            continue;
        };
        let len = len?;
        if i + header + len > script.len() {
            return None;
        }
        last = Some(script[i + header..i + header + len].to_vec());
        i += header + len;
    }
    last
}

fn p2sh_redeem_sigops_exceed(tx: &Transaction, prevouts: &[Option<InputPrevout<'_>>]) -> bool {
    tx.inputs.iter().enumerate().any(|(i, input)| {
        let Some(prevout) = prevouts.get(i).and_then(|prevout| prevout.as_ref()) else {
            return false;
        };
        if !is_p2sh_script(prevout.script.as_ref()) {
            return false;
        }
        let Some(redeem) = last_push_payload(&input.script_sig) else {
            return false;
        };
        blvm_protocol::sigop::count_sigops_in_script(&redeem, true) > MAX_STANDARD_P2SH_SIGOPS
    })
}

/// A standard pay-to-witness-script-hash data item is at most this many bytes.
/// The last stack item is the witness script and is not under this limit.
const MAX_STANDARD_P2WSH_STACK_ITEM_SIZE: usize = 80;

/// A standard pay-to-witness-script-hash witness script is at most this many bytes.
const MAX_STANDARD_P2WSH_SCRIPT_SIZE: usize = 3_600;

/// A standard pay-to-witness-script-hash witness has at most this many items before the script.
const MAX_STANDARD_P2WSH_STACK_ITEMS: usize = 100;

fn is_p2wsh_script(script: &[u8]) -> bool {
    script.len() == 34 && script[0] == 0x00 && script[1] == 0x20
}

fn pay_to_anchor_witness_present(
    prevouts: &[Option<InputPrevout<'_>>],
    witnesses: Option<&[Witness]>,
) -> bool {
    let Some(witnesses) = witnesses else {
        return false;
    };
    prevouts.iter().enumerate().any(|(i, prevout)| {
        let Some(prevout) = prevout.as_ref() else {
            return false;
        };
        if !blvm_protocol::witness::is_pay_to_anchor(prevout.script.as_ref()) {
            return false;
        }
        witnesses.get(i).is_some_and(|stack| !stack.is_empty())
    })
}

fn p2wsh_data_item_exceeds(
    tx: &Transaction,
    prevouts: &[Option<InputPrevout<'_>>],
    witnesses: Option<&[Witness]>,
) -> bool {
    let Some(witnesses) = witnesses else {
        return false;
    };
    prevouts.iter().enumerate().any(|(i, prevout)| {
        let Some(prevout) = prevout.as_ref() else {
            return false;
        };
        let script_sig = tx
            .inputs
            .get(i)
            .map(|input| input.script_sig.as_slice())
            .unwrap_or(&[]);
        let native = is_p2wsh_script(prevout.script.as_ref());
        let wrapped = is_p2sh_script(prevout.script.as_ref())
            && last_script_push(script_sig).is_some_and(is_p2wsh_script);
        if !native && !wrapped {
            return false;
        }
        let Some(stack) = witnesses.get(i) else {
            return false;
        };
        let data_items = stack.len().saturating_sub(1);
        stack
            .iter()
            .take(data_items)
            .any(|item| item.len() > MAX_STANDARD_P2WSH_STACK_ITEM_SIZE)
    })
}

fn p2wsh_script_exceeds(
    tx: &Transaction,
    prevouts: &[Option<InputPrevout<'_>>],
    witnesses: Option<&[Witness]>,
) -> bool {
    let Some(witnesses) = witnesses else {
        return false;
    };
    prevouts.iter().enumerate().any(|(i, prevout)| {
        let Some(prevout) = prevout.as_ref() else {
            return false;
        };
        let script_sig = tx
            .inputs
            .get(i)
            .map(|input| input.script_sig.as_slice())
            .unwrap_or(&[]);
        let native = is_p2wsh_script(prevout.script.as_ref());
        let wrapped = is_p2sh_script(prevout.script.as_ref())
            && last_script_push(script_sig).is_some_and(is_p2wsh_script);
        if !native && !wrapped {
            return false;
        }
        witnesses
            .get(i)
            .and_then(|stack| stack.last())
            .is_some_and(|script| script.len() > MAX_STANDARD_P2WSH_SCRIPT_SIZE)
    })
}

fn p2wsh_stack_count_exceeds(
    tx: &Transaction,
    prevouts: &[Option<InputPrevout<'_>>],
    witnesses: Option<&[Witness]>,
) -> bool {
    let Some(witnesses) = witnesses else {
        return false;
    };
    prevouts.iter().enumerate().any(|(i, prevout)| {
        let Some(prevout) = prevout.as_ref() else {
            return false;
        };
        let script_sig = tx
            .inputs
            .get(i)
            .map(|input| input.script_sig.as_slice())
            .unwrap_or(&[]);
        let native = is_p2wsh_script(prevout.script.as_ref());
        let wrapped = is_p2sh_script(prevout.script.as_ref())
            && last_script_push(script_sig).is_some_and(is_p2wsh_script);
        if !native && !wrapped {
            return false;
        }
        witnesses
            .get(i)
            .is_some_and(|stack| stack.len().saturating_sub(1) > MAX_STANDARD_P2WSH_STACK_ITEMS)
    })
}

/// First byte of a Taproot annex. No meaning is defined for it, so it is not relayed.
const TAPROOT_ANNEX_TAG: u8 = 0x50;

/// A standard tapscript data item is at most this many bytes.
/// The witness script and the control block are not under this limit.
const MAX_STANDARD_TAPSCRIPT_STACK_ITEM_SIZE: usize = 80;

/// Low bit of a control-block header is the output-key parity. The rest is the leaf version.
const TAPROOT_LEAF_MASK: u8 = 0xfe;
const TAPROOT_LEAF_TAPSCRIPT: u8 = 0xc0;

fn is_taproot_output(script: &[u8]) -> bool {
    script.len() == 34 && script[0] == 0x51 && script[1] == 0x20
}

fn taproot_annex_present(
    prevouts: &[Option<InputPrevout<'_>>],
    witnesses: Option<&[Witness]>,
) -> bool {
    let Some(witnesses) = witnesses else {
        return false;
    };
    prevouts.iter().enumerate().any(|(i, prevout)| {
        let Some(prevout) = prevout.as_ref() else {
            return false;
        };
        if !is_taproot_output(prevout.script.as_ref()) {
            return false;
        }
        witnesses.get(i).is_some_and(|stack| {
            stack.len() >= 2
                && stack
                    .last()
                    .is_some_and(|annex| annex.first().copied() == Some(TAPROOT_ANNEX_TAG))
        })
    })
}

fn tapscript_data_item_exceeds(
    prevouts: &[Option<InputPrevout<'_>>],
    witnesses: Option<&[Witness]>,
) -> bool {
    let Some(witnesses) = witnesses else {
        return false;
    };
    prevouts.iter().enumerate().any(|(i, prevout)| {
        let Some(prevout) = prevout.as_ref() else {
            return false;
        };
        if !is_taproot_output(prevout.script.as_ref()) {
            return false;
        }
        let Some(stack) = witnesses.get(i) else {
            return false;
        };
        if stack.len() < 2 {
            return false;
        }
        let Some(control) = stack.last() else {
            return false;
        };
        if control
            .first()
            .is_none_or(|header| header & TAPROOT_LEAF_MASK != TAPROOT_LEAF_TAPSCRIPT)
        {
            return false;
        }
        let data_items = stack.len() - 2;
        stack
            .iter()
            .take(data_items)
            .any(|item| item.len() > MAX_STANDARD_TAPSCRIPT_STACK_ITEM_SIZE)
    })
}

/// Value and script for one spent output.
///
/// A UTXO script is borrowed. A mempool parent script is owned because the pool
/// lock ends before script checks run.
struct InputPrevout<'a> {
    value: i64,
    script: Cow<'a, [u8]>,
}

/// Structure, coinbase, witness-count, and script checks.
///
/// `prevouts` is one entry per input. A missing entry was not in the snapshot and
/// was not created by a transaction already in the pool.
fn consensus_checks_reject(
    tx: &Transaction,
    witnesses: Option<&[Witness]>,
    prevouts: &[Option<InputPrevout<'_>>],
    require_standard: bool,
) -> bool {
    use blvm_protocol::ValidationResult;
    use blvm_protocol::block::calculate_script_flags_for_block_network;
    use blvm_protocol::script::{SigVersion, verify_script_with_context_full};
    use blvm_protocol::transaction::{check_transaction, is_coinbase};
    use blvm_protocol::types::Network;

    const MEMPOOL_POLICY_HEIGHT: u64 = 1_000_000;

    if is_coinbase(tx) {
        return true;
    }
    if !matches!(check_transaction(tx), Ok(ValidationResult::Valid)) {
        return true;
    }
    if let Some(wits) = witnesses
        && wits.len() != tx.inputs.len()
    {
        return true;
    }

    // A witness program of any version, native or inside a P2SH redeem script,
    // is still a witness spend when no stack was supplied.
    // Flag selection otherwise looks only at supplied stacks and at the tx outputs.
    let needs_witness_flags = tx.inputs.iter().enumerate().any(|(i, input)| {
        prevouts
            .get(i)
            .and_then(|prevout| prevout.as_ref())
            .is_some_and(|prevout| {
                let script = prevout.script.as_ref();
                script_needs_witness_verification(script)
                    || p2sh_redeem_is_witness_program(script, &input.script_sig)
            })
    });
    let has_witness = needs_witness_flags || witnesses.is_some_and(|stacks| !stacks.is_empty());
    // Policy only. Block connect keeps the consensus flag mask.
    const SCRIPT_VERIFY_MINIMALDATA: u32 = 1 << 6;
    let mut flags = calculate_script_flags_for_block_network(
        tx,
        has_witness,
        MEMPOOL_POLICY_HEIGHT,
        Network::Mainnet,
    ) | SCRIPT_VERIFY_MINIMALDATA;
    if require_standard {
        flags |= blvm_protocol::script::flags::SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_NOPS
            | blvm_protocol::script::flags::SCRIPT_VERIFY_MINIMALIF
            | blvm_protocol::script::flags::SCRIPT_VERIFY_CLEANSTACK
            | blvm_protocol::script::flags::SCRIPT_VERIFY_NULLFAIL
            | blvm_protocol::script::flags::SCRIPT_VERIFY_LOW_S
            | blvm_protocol::script::flags::SCRIPT_VERIFY_WITNESS_PUBKEYTYPE
            | blvm_protocol::script::flags::SCRIPT_VERIFY_DISCOURAGE_OP_SUCCESS
            | blvm_protocol::script::flags::SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_TAPROOT_VERSION
            | blvm_protocol::script::flags::SCRIPT_VERIFY_CONST_SCRIPTCODE
            | blvm_protocol::script::flags::SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM
            | blvm_protocol::script::flags::SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_PUBKEYTYPE
            | blvm_protocol::script::flags::SCRIPT_VERIFY_STRICTENC;
    }
    let mut prevout_values = Vec::with_capacity(prevouts.len());
    let mut prevout_scripts: Vec<&[u8]> = Vec::with_capacity(prevouts.len());
    for prevout in prevouts {
        match prevout {
            Some(prevout) => {
                prevout_values.push(prevout.value);
                prevout_scripts.push(prevout.script.as_ref());
            }
            None => {
                prevout_values.push(0);
                prevout_scripts.push(&[]);
            }
        }
    }

    for (i, input) in tx.inputs.iter().enumerate() {
        let Some(prevout) = prevouts.get(i).and_then(|prevout| prevout.as_ref()) else {
            continue;
        };
        let witness = witnesses.and_then(|stacks| stacks.get(i));
        let Ok(true) = verify_script_with_context_full(
            &input.script_sig,
            prevout.script.as_ref(),
            witness,
            flags,
            tx,
            i,
            &prevout_values,
            &prevout_scripts,
            Some(MEMPOOL_POLICY_HEIGHT),
            None,
            Network::Mainnet,
            SigVersion::Base,
            #[cfg(feature = "production")]
            None,
            None,
            #[cfg(feature = "production")]
            None,
            #[cfg(feature = "production")]
            None,
            #[cfg(feature = "production")]
            None,
            None,
        ) else {
            return true;
        };
    }
    false
}

pub(crate) fn money_sats(value: i64) -> Option<u64> {
    if (0..=blvm_protocol::constants::MAX_MONEY).contains(&value) {
        Some(value as u64)
    } else {
        None
    }
}

/// Running sum of money. `None` once the total passes `MAX_MONEY`.
pub(crate) fn add_money(total: u64, sats: u64) -> Option<u64> {
    let max = blvm_protocol::constants::MAX_MONEY as u64;
    total.checked_add(sats).filter(|sum| *sum <= max)
}

/// Sum of output values in range. `None` when any output is outside the money range.
pub(crate) fn output_sum_sats(tx: &Transaction) -> Option<u64> {
    let mut total = 0u64;
    for output in &tx.outputs {
        let sats = money_sats(output.value)?;
        total = add_money(total, sats)?;
    }
    Some(total)
}

/// Transitive children of `root` in the direct-child map. `root` itself is not included.
fn descendant_closure(map: &HashMap<Hash, HashSet<Hash>>, root: &Hash) -> HashSet<Hash> {
    let mut out = HashSet::new();
    let mut stack = vec![*root];
    while let Some(current) = stack.pop() {
        if let Some(children) = map.get(&current) {
            for child in children {
                if out.insert(*child) {
                    stack.push(*child);
                }
            }
        }
    }
    out
}

/// Base serialization plus every witness stack item. The cap uses this size.
fn accounted_tx_bytes(tx: &Transaction, witnesses: Option<&[Witness]>) -> u64 {
    use blvm_protocol::serialization::transaction::serialize_transaction;
    let mut n = serialize_transaction(tx).len() as u64;
    if let Some(stacks) = witnesses {
        for stack in stacks {
            for item in stack {
                n = n.saturating_add(item.len() as u64);
            }
        }
    }
    n
}

impl MempoolManager {
    /// Create a new mempool manager
    pub fn new() -> Self {
        let manager = Self {
            pool: Mutex::new(MempoolPool {
                transactions: HashMap::new(),
                wtxids: HashMap::new(),
                tx_witnesses: HashMap::new(),
                adjusted_vsize: HashMap::new(),
                spent_outputs: HashSet::new(),
                spenders: HashMap::new(),
            }),
            mempool: RwLock::new(Mempool::new()),
            utxo_set_arc: RwLock::new(None),
            fee_index: RwLock::new(BTreeMap::new()),
            fee_cache: RwLock::new(HashMap::new()),
            fee_basis: RwLock::new(HashMap::new()),
            fee_deltas: RwLock::new(HashMap::new()),
            rbf_config: RwLock::new(None),
            policy_config: RwLock::new(None),
            rbf_tracking: RwLock::new(HashMap::new()),
            tx_timestamps: RwLock::new(HashMap::new()),
            tx_heights: RwLock::new(HashMap::new()),
            tx_dependencies: RwLock::new(HashMap::new()),
            tx_descendants: RwLock::new(HashMap::new()),
            utxo_set_hash: RwLock::new(None),
            event_publisher: RwLock::new(None),
            storage: RwLock::new(None),
            snapshot_prevout: RwLock::new(None),
            chain_tip: RwLock::new(None),
            relay_floor_waivers: Mutex::new(HashSet::new()),
            ephemeral_dust_waivers: Mutex::new(HashSet::new()),
            package_preview: Mutex::new(PackagePreview::default()),
            fee_estimator: RwLock::new(None),
            loading_from_disk: AtomicBool::new(false),
            admitting_package: AtomicBool::new(false),
        };
        #[cfg(test)]
        {
            let mut policy = crate::config::MempoolPolicyConfig::default();
            policy.require_standard = false;
            *manager
                .policy_config
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(policy);
        }
        manager
    }

    /// Create a new mempool manager with RBF configuration
    pub fn with_rbf_config(rbf_config: Option<RbfConfig>) -> Self {
        Self {
            pool: Mutex::new(MempoolPool {
                transactions: HashMap::new(),
                wtxids: HashMap::new(),
                tx_witnesses: HashMap::new(),
                adjusted_vsize: HashMap::new(),
                spent_outputs: HashSet::new(),
                spenders: HashMap::new(),
            }),
            mempool: RwLock::new(Mempool::new()),
            utxo_set_arc: RwLock::new(None),
            fee_index: RwLock::new(BTreeMap::new()),
            fee_cache: RwLock::new(HashMap::new()),
            fee_basis: RwLock::new(HashMap::new()),
            fee_deltas: RwLock::new(HashMap::new()),
            rbf_config: RwLock::new(rbf_config),
            policy_config: RwLock::new(None),
            rbf_tracking: RwLock::new(HashMap::new()),
            tx_timestamps: RwLock::new(HashMap::new()),
            tx_heights: RwLock::new(HashMap::new()),
            tx_dependencies: RwLock::new(HashMap::new()),
            tx_descendants: RwLock::new(HashMap::new()),
            utxo_set_hash: RwLock::new(None),
            event_publisher: RwLock::new(None),
            storage: RwLock::new(None),
            snapshot_prevout: RwLock::new(None),
            chain_tip: RwLock::new(None),
            relay_floor_waivers: Mutex::new(HashSet::new()),
            ephemeral_dust_waivers: Mutex::new(HashSet::new()),
            package_preview: Mutex::new(PackagePreview::default()),
            fee_estimator: RwLock::new(None),
            loading_from_disk: AtomicBool::new(false),
            admitting_package: AtomicBool::new(false),
        }
    }

    /// Wire the live UTXO set into the mempool so RBF fee checks use real values.
    /// Call this once after constructing MempoolManager and before the first transaction.
    pub fn set_utxo_set_arc(&self, utxo_set: Arc<tokio::sync::Mutex<UtxoSet>>) {
        *self.utxo_set_arc.write().unwrap_or_else(|e| e.into_inner()) = Some(utxo_set);
    }

    /// Wire chain storage so locktime checks use the current tip.
    pub fn set_storage(&self, storage: Arc<crate::storage::Storage>) {
        let lookup_storage = Arc::clone(&storage);
        *self.storage.write().unwrap_or_else(|e| e.into_inner()) = Some(storage);
        self.set_snapshot_prevout_lookup(Arc::new(move |prevout| {
            ibd_snapshot_prevout(&lookup_storage, prevout)
        }));
    }

    /// Prevout source used when the in-memory set misses. Tests pass a map.
    /// `set_storage` installs a point-read of the IBD snapshot, then the
    /// chainstate table when that snapshot has no rows.
    pub(crate) fn set_snapshot_prevout_lookup(&self, lookup: SnapshotPrevoutLookup) {
        *self
            .snapshot_prevout
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(lookup);
    }

    /// Next-block height and median time-past, for tests that have no chain storage.
    pub fn set_chain_tip(&self, next_height: u64, median_time_past: u64) {
        *self.chain_tip.write().unwrap_or_else(|e| e.into_inner()) =
            Some((next_height, median_time_past));
    }

    /// Height of a coin that is not yet in a block. Relative locks treat a missing
    /// prevout as created at this height.
    pub fn unconfirmed_coin_height(&self) -> u64 {
        self.finality_context()
            .map(|(height, _)| height)
            .unwrap_or(0)
    }

    fn finality_context(&self) -> Option<(u64, u64)> {
        if let Some(storage) = self
            .storage
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Some(next_block_finality(storage).unwrap_or((0, 0)));
        }
        *self.chain_tip.read().unwrap_or_else(|e| e.into_inner())
    }

    fn chain_network(&self) -> anyhow::Result<blvm_protocol::types::Network> {
        match self
            .storage
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(storage) => storage.chain().consensus_network(),
            None => Ok(blvm_protocol::types::Network::Mainnet),
        }
    }

    /// Median time of the block before `coin_height`.
    fn prior_median_time(&self, coin_height: u64) -> Option<i64> {
        let storage = self
            .storage
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let headers = storage
            .blocks()
            .headers_before_height_for_mtp(coin_height)
            .ok()?;
        if headers.is_empty() {
            return None;
        }
        i64::try_from(blvm_protocol::bip113::get_median_time_past(&headers)).ok()
    }

    /// Script and value for each input. The snapshot wins. Otherwise use the
    /// output a transaction already in the pool created.
    fn script_prevouts<'a>(
        &self,
        tx: &Transaction,
        utxo_set: &'a UtxoSet,
    ) -> Vec<Option<InputPrevout<'a>>> {
        let pool = self.pool_lock();
        tx.inputs
            .iter()
            .map(|input| {
                if let Some(utxo) = utxo_set.get(&input.prevout) {
                    Some(InputPrevout {
                        value: utxo.value,
                        script: Cow::Borrowed(utxo.script_pubkey.as_ref()),
                    })
                } else {
                    pool.transactions
                        .get(&input.prevout.hash)
                        .and_then(|parent| parent.outputs.get(input.prevout.index as usize))
                        .map(|output| InputPrevout {
                            value: output.value,
                            script: Cow::Owned(output.script_pubkey.clone()),
                        })
                }
            })
            .collect()
    }

    fn mempool_creates(&self, prevout: &OutPoint) -> bool {
        let pool = self.pool_lock();
        pool.transactions
            .get(&prevout.hash)
            .and_then(|parent| parent.outputs.get(prevout.index as usize))
            .is_some()
    }

    fn relative_lock_rejects(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        use blvm_protocol::locktime::{
            extract_sequence_locktime_value, extract_sequence_type_flag, is_sequence_disabled,
        };

        let Some((block_height, block_mtp)) = self.finality_context() else {
            // A relative lock of 0 is already final. A non-zero lock needs the tip.
            return blvm_consensus::sequence_locks::version_enforces_relative_lock(tx.version)
                && tx.inputs.iter().any(|input| {
                    let seq = input.sequence as u32;
                    !is_sequence_disabled(seq)
                        && blvm_protocol::locktime::extract_sequence_locktime_value(seq) != 0
                });
        };
        let block_mtp_i = i64::try_from(block_mtp).unwrap_or(-1);
        let mut prev_mtps = Vec::with_capacity(tx.inputs.len());
        for input in &tx.inputs {
            let seq = input.sequence as u32;
            if is_sequence_disabled(seq) || !extract_sequence_type_flag(seq) {
                prev_mtps.push(-1);
                continue;
            }
            if let Some(utxo) = utxo_set.get(&input.prevout) {
                if let Some(mtp) = self.prior_median_time(utxo.height) {
                    prev_mtps.push(mtp);
                } else if extract_sequence_locktime_value(seq) == 0 && block_mtp_i >= 0 {
                    // Value 0 is one second before the coin's median time, which
                    // is already earlier than this tip. The prior header is not required.
                    prev_mtps.push(block_mtp_i);
                } else {
                    return true;
                }
            } else if self.mempool_creates(&input.prevout) && block_mtp_i >= 0 {
                prev_mtps.push(block_mtp_i);
            } else {
                return true;
            }
        }
        let Ok(network) = self.chain_network() else {
            return true;
        };
        blvm_protocol::mempool::relative_lock_unsatisfied(
            tx,
            utxo_set,
            block_height,
            block_mtp,
            Some(&prev_mtps),
            network,
        )
        .unwrap_or(true)
    }

    fn prevout_value_out_of_range(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        tx.inputs.iter().any(|input| {
            utxo_set
                .get(&input.prevout)
                .is_some_and(|utxo| money_sats(utxo.value).is_none())
        })
    }

    /// Each prevout may sit on `MAX_MONEY`. The running sum may not.
    fn prevout_sum_exceeds_money(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        let mut total = 0u64;
        for input in &tx.inputs {
            let sats = if let Some(utxo) = utxo_set.get(&input.prevout) {
                let Some(sats) = money_sats(utxo.value) else {
                    return true;
                };
                sats
            } else if let Some(sats) = self.mempool_input_value(&input.prevout) {
                sats
            } else {
                continue;
            };
            let Some(sum) = add_money(total, sats) else {
                return true;
            };
            total = sum;
        }
        false
    }

    /// Outputs may consume the inputs. They may not create new coins.
    fn outputs_exceed_inputs(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        let Some(output_total) = output_sum_sats(tx) else {
            return false;
        };
        let mut input_total = 0u64;
        for input in &tx.inputs {
            let sats = if let Some(utxo) = utxo_set.get(&input.prevout) {
                money_sats(utxo.value)
            } else {
                self.mempool_input_value(&input.prevout)
            };
            let Some(sats) = sats else {
                return false;
            };
            let Some(sum) = add_money(input_total, sats) else {
                return false;
            };
            input_total = sum;
        }
        output_total > input_total
    }

    /// Copy snapshot hits into this admission's set only. The shared set is unchanged.
    fn fill_snapshot_misses(&self, tx: &Transaction, utxo_set: &mut UtxoSet) {
        for input in tx.inputs.iter() {
            if utxo_set.contains_key(&input.prevout) || self.mempool_creates(&input.prevout) {
                continue;
            }
            if let Some(utxo) = self.snapshot_prevout(&input.prevout) {
                utxo_set.insert(input.prevout, Arc::new(utxo));
            }
        }
    }

    fn snapshot_prevout(&self, prevout: &OutPoint) -> Option<blvm_protocol::UTXO> {
        let lookup = self
            .snapshot_prevout
            .read()
            .unwrap_or_else(|e| e.into_inner());
        lookup.as_ref().and_then(|lookup| lookup(prevout))
    }

    fn unknown_prevout(&self, tx: &Transaction, utxo_set: &UtxoSet, utxo_wired: bool) -> bool {
        if !utxo_wired {
            return false;
        }
        tx.inputs.iter().any(|input| {
            utxo_set.get(&input.prevout).is_none() && !self.mempool_creates(&input.prevout)
        })
    }

    fn immature_coinbase_spend(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        // No tip yet: the next block is height 0, where a coinbase is still immature.
        let height = self
            .finality_context()
            .map(|(height, _)| height)
            .unwrap_or(0);
        tx.inputs.iter().any(|input| {
            utxo_set.get(&input.prevout).is_some_and(|utxo| {
                !blvm_protocol::transaction::check_coinbase_maturity(
                    height,
                    utxo.height,
                    utxo.is_coinbase,
                )
            })
        })
    }

    /// Set event publisher for mempool events
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_event_publisher(&self, event_publisher: Option<Arc<EventPublisher>>) {
        *self
            .event_publisher
            .write()
            .unwrap_or_else(|e| e.into_inner()) = event_publisher.clone();
        if let Some(estimator) = self.fee_estimator() {
            estimator.set_event_publisher(event_publisher);
        }
    }

    /// Set RBF configuration
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_rbf_config(&self, rbf_config: Option<RbfConfig>) {
        *self.rbf_config.write().unwrap_or_else(|e| e.into_inner()) = rbf_config;
    }

    /// Set mempool policy configuration
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_policy_config(&self, policy_config: Option<MempoolPolicyConfig>) {
        *self
            .policy_config
            .write()
            .unwrap_or_else(|e| e.into_inner()) = policy_config;
        let floor = self.min_relay_fee_sat_per_kvb();
        if let Some(estimator) = self.fee_estimator() {
            estimator.set_relay_floor(floor);
        }
    }

    /// Attach the fee estimator. `None` detaches it. The estimator holds no pointer back here.
    pub fn set_fee_estimator(&self, estimator: Option<Arc<FeeEstimator>>) {
        if let Some(estimator) = estimator.as_ref() {
            let publisher = self
                .event_publisher
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            estimator.set_event_publisher(publisher);
        }
        *self
            .fee_estimator
            .write()
            .unwrap_or_else(|e| e.into_inner()) = estimator;
    }

    pub fn fee_estimator(&self) -> Option<Arc<FeeEstimator>> {
        self.fee_estimator
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Lock pool for access (transactions + spent_outputs)
    fn pool_lock(&self) -> std::sync::MutexGuard<'_, MempoolPool> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get current timestamp (Unix seconds)
    fn current_timestamp() -> u64 {
        crate::utils::time::current_timestamp()
    }

    /// Start mempool maintenance hooks.
    ///
    /// P2P and RPC intake call [`add_transaction`](Self::add_transaction) directly via
    /// `NetworkManager` and `RpcManager`; there is no separate pending-tx queue to drain here.
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting mempool manager");
        self.initialize_mempool().await?;
        Ok(())
    }

    /// Run periodic mempool maintenance once (expiry cleanup; for tests and manual ticks).
    pub async fn process_once(&self) -> Result<()> {
        self.cleanup_old_transactions().await?;
        Ok(())
    }

    /// Backdate or refresh one entry. Expiry tests use this.
    pub(crate) fn set_entry_time(&self, hash: &Hash, timestamp: u64) {
        self.tx_timestamps
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(*hash, timestamp);
    }

    /// Initialize mempool
    async fn initialize_mempool(&mut self) -> Result<()> {
        debug!("Initializing mempool");
        Ok(())
    }

    /// Clean up old transactions
    async fn cleanup_old_transactions(&self) -> Result<()> {
        let policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        if let Some(expiry_time) = policy.mempool_expiry_hours.checked_mul(3600) {
            let current_time = Self::current_timestamp();
            // Optimization: Pre-allocate with estimated capacity
            let estimated_removals = self.pool_lock().transactions.len() / 100; // Estimate ~1% will expire
            let mut to_remove = Vec::with_capacity(estimated_removals);

            {
                let timestamps = self.tx_timestamps.read().unwrap_or_else(|e| e.into_inner());
                for (hash, timestamp) in timestamps.iter() {
                    if current_time.saturating_sub(*timestamp) > expiry_time {
                        to_remove.push(*hash);
                    }
                }
            }

            // The expired transaction's in-mempool descendants spend it. Remove
            // that package with it. Ancestors are not in the package.
            for hash in to_remove {
                debug!("Removing expired transaction {}", hex::encode(hash));
                self.evict_with_descendants(&hash);
            }
        }

        // Check mempool size limits and evict if necessary
        self.enforce_mempool_limits().await?;

        Ok(())
    }

    /// Enforce mempool size limits by evicting transactions if necessary
    async fn enforce_mempool_limits(&self) -> Result<()> {
        let policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        // Calculate current mempool size
        let current_bytes = self.mempool_size_bytes();
        let current_tx_count = self.pool_lock().transactions.len();
        let cap_bytes = policy.max_mempool_mb.saturating_mul(1_048_576);

        // Check if we need to evict
        let needs_eviction = current_bytes > cap_bytes || current_tx_count > policy.max_mempool_txs;

        if !needs_eviction {
            return Ok(());
        }

        // Publish MempoolThresholdExceeded for module event subscribers
        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let threshold = policy.max_mempool_txs;
            let current = current_tx_count;
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_threshold_exceeded(current, threshold)
                    .await;
            });
        }

        debug!(
            "Mempool size limit exceeded: {} bytes / {} bytes, {} txs / {} txs. Evicting transactions...",
            current_bytes, cap_bytes, current_tx_count, policy.max_mempool_txs
        );

        // Capture min fee rate before eviction (for FeeRateChanged event)
        let old_min_fee_rate = self.get_min_fee_rate_sat_per_vb();

        // Evict transactions based on strategy
        let target_bytes = cap_bytes;
        let target_tx_count = policy.max_mempool_txs;
        let policy = &policy;

        match &policy.eviction_strategy {
            crate::config::EvictionStrategy::LowestFeeRate => {
                self.evict_lowest_fee_rate(target_bytes, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::OldestFirst => {
                self.evict_oldest_first(target_bytes, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::LargestFirst => {
                self.evict_largest_first(target_bytes, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::NoDescendantsFirst => {
                self.evict_no_descendants_first(target_bytes, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::Hybrid => {
                self.evict_hybrid(target_bytes, target_tx_count).await?;
            }
            crate::config::EvictionStrategy::SpamFirst => {
                self.evict_spam_first(target_bytes, target_tx_count).await?;
            }
        }

        // Publish FeeRateChanged when min fee rate increased due to eviction
        let new_min_fee_rate = self.get_min_fee_rate_sat_per_vb();
        if new_min_fee_rate != old_min_fee_rate
            && let Some(ref event_pub) = *self
                .event_publisher
                .read()
                .unwrap_or_else(|e| e.into_inner())
        {
            let old_f64 = old_min_fee_rate as f64;
            let new_f64 = new_min_fee_rate as f64;
            let mempool_size = self.pool_lock().transactions.len();
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_fee_rate_changed(old_f64, new_f64, mempool_size)
                    .await;
            });
        }

        Ok(())
    }

    /// Get the minimum fee rate (sat/vB) in the mempool, or 0 if empty.
    fn get_min_fee_rate_sat_per_vb(&self) -> u64 {
        let fee_index = self.fee_index.read().unwrap_or_else(|e| e.into_inner());
        fee_index
            .iter()
            .last()
            .map(|(Reverse(r), _)| *r)
            .unwrap_or(0)
    }

    /// Serialized size of every pooled transaction, in bytes, including witness stacks.
    fn mempool_size_bytes(&self) -> u64 {
        let pool = self.pool_lock();
        pool.transactions
            .iter()
            .map(|(hash, tx)| {
                accounted_tx_bytes(tx, pool.tx_witnesses.get(hash).map(|w| w.as_slice()))
            })
            .sum()
    }

    /// Remove `hash` and its in-mempool descendants. Returns bytes and count removed.
    fn evict_with_descendants(&self, hash: &Hash) -> (u64, usize) {
        if !self.pool_lock().transactions.contains_key(hash) {
            return (0, 0);
        }
        let package = self.replacement_package(hash);
        let mut bytes = 0u64;
        let mut count = 0usize;
        for id in package {
            let (tx, wits) = {
                let pool = self.pool_lock();
                (
                    pool.transactions.get(&id).cloned(),
                    pool.tx_witnesses.get(&id).cloned(),
                )
            };
            let Some(tx) = tx else {
                continue;
            };
            bytes += accounted_tx_bytes(&tx, wits.as_deref());
            if self.remove_transaction(&id) {
                count += 1;
            }
        }
        (bytes, count)
    }

    /// Evict `order` until both the byte cap and the count cap hold.
    fn evict_ordered(&self, order: Vec<Hash>, target_bytes: u64, target_tx_count: usize) {
        let mut current_bytes = self.mempool_size_bytes();
        let mut current_tx_count = self.pool_lock().transactions.len();
        for hash in order {
            if current_bytes <= target_bytes && current_tx_count <= target_tx_count {
                break;
            }
            let (bytes, count) = self.evict_with_descendants(&hash);
            current_bytes = current_bytes.saturating_sub(bytes);
            current_tx_count = current_tx_count.saturating_sub(count);
        }
    }

    fn hashes_by_ascending_fee(&self) -> Vec<Hash> {
        let hashes: Vec<Hash> = self.pool_lock().transactions.keys().copied().collect();
        let mut rows: Vec<(Hash, u64)> = hashes
            .into_iter()
            .map(|hash| {
                let fee_rate = self
                    .fee_cache
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&hash)
                    .copied()
                    .unwrap_or(0);
                (hash, fee_rate)
            })
            .collect();
        rows.sort_by_key(|(_, fee_rate)| *fee_rate);
        rows.into_iter().map(|(hash, _)| hash).collect()
    }

    /// Evict transactions with lowest fee rate
    async fn evict_lowest_fee_rate(&self, target_bytes: u64, target_tx_count: usize) -> Result<()> {
        self.evict_ordered(
            self.hashes_by_ascending_fee(),
            target_bytes,
            target_tx_count,
        );
        Ok(())
    }

    /// Evict oldest transactions first (FIFO)
    async fn evict_oldest_first(&self, target_bytes: u64, target_tx_count: usize) -> Result<()> {
        let mut tx_ages: Vec<(Hash, u64)> = {
            let timestamps = self.tx_timestamps.read().unwrap_or_else(|e| e.into_inner());
            self.pool_lock()
                .transactions
                .keys()
                .filter_map(|hash| timestamps.get(hash).map(|&timestamp| (*hash, timestamp)))
                .collect()
        };
        tx_ages.sort_by_key(|(_, timestamp)| *timestamp);
        let order = tx_ages.into_iter().map(|(hash, _)| hash).collect();
        self.evict_ordered(order, target_bytes, target_tx_count);
        Ok(())
    }

    /// Evict largest transactions first
    async fn evict_largest_first(&self, target_bytes: u64, target_tx_count: usize) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        let mut tx_sizes: Vec<(Hash, usize)> = self
            .pool_lock()
            .transactions
            .iter()
            .map(|(hash, tx)| (*hash, serialize_transaction(tx).len()))
            .collect();
        tx_sizes.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
        let order = tx_sizes.into_iter().map(|(hash, _)| hash).collect();
        self.evict_ordered(order, target_bytes, target_tx_count);
        Ok(())
    }

    /// Evict transactions with no descendants first, then whole packages if still over the cap.
    async fn evict_no_descendants_first(
        &self,
        target_bytes: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        let mut childless: Vec<(Hash, u64)> = {
            let descendants = self
                .tx_descendants
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let fee_cache = self.fee_cache.read().unwrap_or_else(|e| e.into_inner());
            self.pool_lock()
                .transactions
                .keys()
                .filter_map(|hash| {
                    let has_descendants = descendants
                        .get(hash)
                        .map(|d| !d.is_empty())
                        .unwrap_or(false);
                    if has_descendants {
                        None
                    } else {
                        Some((*hash, fee_cache.get(hash).copied().unwrap_or(0)))
                    }
                })
                .collect()
        };
        childless.sort_by_key(|(_, fee_rate)| *fee_rate);
        self.evict_ordered(
            childless.into_iter().map(|(hash, _)| hash).collect(),
            target_bytes,
            target_tx_count,
        );
        if self.mempool_size_bytes() > target_bytes
            || self.pool_lock().transactions.len() > target_tx_count
        {
            self.evict_ordered(
                self.hashes_by_ascending_fee(),
                target_bytes,
                target_tx_count,
            );
        }
        Ok(())
    }

    /// Hybrid eviction: combine fee rate and age
    async fn evict_hybrid(&self, target_bytes: u64, target_tx_count: usize) -> Result<()> {
        let current_time = Self::current_timestamp();
        let mut tx_scores: Vec<(Hash, u64)> = {
            let timestamps = self.tx_timestamps.read().unwrap_or_else(|e| e.into_inner());
            let fee_cache = self.fee_cache.read().unwrap_or_else(|e| e.into_inner());
            self.pool_lock()
                .transactions
                .keys()
                .map(|hash| {
                    let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);
                    let age = timestamps
                        .get(hash)
                        .map(|&t| current_time.saturating_sub(t))
                        .unwrap_or(0);
                    let fee_score = if fee_rate > 0 {
                        1_000_000 / (fee_rate + 1)
                    } else {
                        1_000_000
                    };
                    let age_score = age / 3600;
                    (*hash, fee_score + age_score)
                })
                .collect()
        };
        tx_scores.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        let order = tx_scores.into_iter().map(|(hash, _)| hash).collect();
        self.evict_ordered(order, target_bytes, target_tx_count);
        Ok(())
    }

    /// Evict spam transactions first (when mempool is full)
    async fn evict_spam_first(&self, target_bytes: u64, target_tx_count: usize) -> Result<()> {
        use blvm_protocol::spam_filter::SpamFilter;

        let spam_filter = SpamFilter::new();
        let mut spam_txs: Vec<(Hash, u64)> = Vec::new();
        let mut non_spam_txs: Vec<(Hash, u64)> = Vec::new();
        let entries: Vec<(Hash, Transaction, Option<Vec<Witness>>)> = {
            let pool = self.pool_lock();
            pool.transactions
                .iter()
                .map(|(h, t)| (*h, t.clone(), pool.tx_witnesses.get(h).cloned()))
                .collect()
        };
        let fee_cache = self.fee_cache.read().unwrap_or_else(|e| e.into_inner());
        for (hash, tx, wits) in &entries {
            let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);
            if spam_filter
                .is_spam_with_witness(tx, wits.as_deref(), None)
                .is_spam
            {
                spam_txs.push((*hash, fee_rate));
            } else {
                non_spam_txs.push((*hash, fee_rate));
            }
        }
        drop(fee_cache);
        spam_txs.sort_by_key(|(_, fee_rate)| *fee_rate);
        non_spam_txs.sort_by_key(|(_, fee_rate)| *fee_rate);
        let mut order: Vec<Hash> = spam_txs.into_iter().map(|(hash, _)| hash).collect();
        order.extend(non_spam_txs.into_iter().map(|(hash, _)| hash));
        self.evict_ordered(order, target_bytes, target_tx_count);
        Ok(())
    }

    /// Transactions admission deletes before it counts ancestors and descendants.
    /// Empty when `tx` conflicts with nothing still in the pool.
    fn txs_this_replacement_removes(&self, tx: &Transaction) -> HashSet<Hash> {
        let mut direct = Vec::new();
        for input in &tx.inputs {
            if let Some(existing_hash) = self.pool_lock().spenders.get(&input.prevout).copied()
                && !direct.contains(&existing_hash)
            {
                direct.push(existing_hash);
            }
        }
        let mut removed = HashSet::new();
        for hash in &direct {
            removed.extend(self.replacement_package(hash));
        }
        removed
    }

    /// Distinct connected components among `direct` conflicts.
    ///
    /// A parent and its child are one cluster. Independent chains are not.
    fn conflicting_cluster_count(&self, direct: &[Hash]) -> usize {
        let descendants_map = self
            .tx_descendants
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let pool = self.pool_lock();
        let mut seen = HashSet::new();
        let mut clusters = 0usize;
        for start in direct {
            if !pool.transactions.contains_key(start) || seen.contains(start) {
                continue;
            }
            clusters += 1;
            let mut stack = vec![*start];
            while let Some(current) = stack.pop() {
                if !seen.insert(current) {
                    continue;
                }
                if let Some(children) = descendants_map.get(&current) {
                    for child in children {
                        if pool.transactions.contains_key(child) {
                            stack.push(*child);
                        }
                    }
                }
                if let Some(tx) = pool.transactions.get(&current) {
                    for input in &tx.inputs {
                        if pool.transactions.contains_key(&input.prevout.hash) {
                            stack.push(input.prevout.hash);
                        }
                    }
                }
            }
        }
        clusters
    }

    /// In-mempool replacement package: `root` plus all transitive descendants.
    fn replacement_package(&self, root: &Hash) -> HashSet<Hash> {
        let descendants_map = self
            .tx_descendants
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let pool = self.pool_lock();
        let mut package = HashSet::new();
        let mut stack = vec![*root];
        while let Some(current) = stack.pop() {
            if !package.insert(current) {
                continue;
            }
            if let Some(children) = descendants_map.get(&current) {
                for child in children {
                    if pool.transactions.contains_key(child) {
                        stack.push(*child);
                    }
                }
            }
        }
        package
    }

    /// Sum fees and virtual size for a set of in-mempool transactions.
    fn package_fee_and_vsize(&self, package: &HashSet<Hash>, utxo_set: &UtxoSet) -> (i64, u64) {
        let mut total_fee = 0i64;
        let mut total_vsize = 0u64;
        for hash in package {
            let tx = {
                let pool = self.pool_lock();
                pool.transactions.get(hash).cloned()
            };
            if let Some(tx) = tx {
                total_fee += self.calculate_transaction_fee(&tx, utxo_set) as i64;
                total_vsize += self.pooled_vsize(hash, &tx, utxo_set);
            }
        }
        (total_fee, total_vsize)
    }

    /// Value of a prevout created by an in-mempool parent transaction.
    fn mempool_input_value(&self, prevout: &OutPoint) -> Option<u64> {
        let pool = self.pool_lock();
        let parent = pool.transactions.get(&prevout.hash)?;
        parent
            .outputs
            .get(prevout.index as usize)
            .and_then(|output| money_sats(output.value))
    }

    /// Prevouts `replacement_checks` needs. Confirmed coins are copied from
    /// `utxo_set`. An output created by a mempool transaction that this
    /// replacement does not remove is copied too. An output of a transaction
    /// in the displacement set stays absent, so spending it fails closed.
    fn replacement_input_view(
        &self,
        new_tx: &Transaction,
        existing_tx: &Transaction,
        utxo_set: &UtxoSet,
    ) -> UtxoSet {
        let existing_id = blvm_protocol::block::calculate_tx_id(existing_tx);
        let displaced = self.replacement_package(&existing_id);
        let mut view = UtxoSet::default();
        let mut missing = Vec::new();
        for input in new_tx.inputs.iter().chain(existing_tx.inputs.iter()) {
            if let Some(utxo) = utxo_set.get(&input.prevout) {
                view.insert(input.prevout, Arc::clone(utxo));
            } else if !displaced.contains(&input.prevout.hash) {
                missing.push(input.prevout);
            }
        }
        if missing.is_empty() {
            return view;
        }
        let pool = self.pool_lock();
        for prevout in missing {
            let Some(output) = pool
                .transactions
                .get(&prevout.hash)
                .and_then(|parent| parent.outputs.get(prevout.index as usize))
            else {
                continue;
            };
            view.insert(
                prevout,
                Arc::new(blvm_protocol::UTXO {
                    value: output.value,
                    script_pubkey: output.script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
        }
        view
    }

    /// Check if a transaction can replace an existing one (RBF)
    ///
    /// This wraps the consensus layer replacement_checks with RBF mode-specific logic
    ///
    /// `storage` is optional - if provided, can be used for conservative mode confirmation checks
    pub fn check_rbf_replacement(
        &self,
        new_tx: &Transaction,
        existing_tx: &Transaction,
        utxo_set: &UtxoSet,
        storage: Option<&crate::storage::Storage>,
    ) -> Result<bool> {
        self.check_rbf_replacement_with_witness(new_tx, existing_tx, utxo_set, storage, None)
    }

    fn check_rbf_replacement_with_witness(
        &self,
        new_tx: &Transaction,
        existing_tx: &Transaction,
        utxo_set: &UtxoSet,
        storage: Option<&crate::storage::Storage>,
        new_witnesses: Option<&[Witness]>,
    ) -> Result<bool> {
        use blvm_protocol::block::calculate_tx_id;

        let rbf_config = self
            .rbf_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        // Check if RBF is disabled
        if matches!(rbf_config.mode, crate::config::RbfMode::Disabled) {
            return Ok(false);
        }
        // Conservative mode is strict BIP125. Standard matches Core's default and
        // replaces an original that does not signal.
        if matches!(rbf_config.mode, crate::config::RbfMode::Conservative)
            && !blvm_protocol::mempool::signals_rbf(existing_tx)
        {
            return Ok(false);
        }

        // Use the cloned config for the rest of the function
        let rbf_config = &rbf_config;

        let existing_tx_hash = calculate_tx_id(existing_tx);

        // Check replacement count limit
        if let Some(tracking) = self
            .rbf_tracking
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&existing_tx_hash)
        {
            if tracking.replacement_count >= rbf_config.max_replacements_per_tx {
                warn!(
                    "RBF replacement rejected: max replacements ({}) exceeded for tx {}",
                    rbf_config.max_replacements_per_tx,
                    hex::encode(existing_tx_hash)
                );
                return Ok(false);
            }

            // Check cooldown period
            let current_time = Self::current_timestamp();
            let time_since_last = current_time.saturating_sub(tracking.last_replacement_time);
            if time_since_last < rbf_config.cooldown_seconds {
                warn!(
                    "RBF replacement rejected: cooldown period not met ({}s remaining) for tx {}",
                    rbf_config.cooldown_seconds - time_since_last,
                    hex::encode(existing_tx_hash)
                );
                return Ok(false);
            }
        }

        // Price every transaction this replacement removes. That is the conflicting
        // transaction plus its in-mempool descendants. A fee that beats only the
        // parent still deletes the child.
        let stored = self
            .pool_lock()
            .transactions
            .contains_key(&existing_tx_hash);
        let (existing_fee, existing_tx_size) = if stored {
            let package = self.replacement_package(&existing_tx_hash);
            if !rbf_config.allow_package_replacements && package.len() > 1 {
                return Ok(false);
            }
            if !rbf_config.allow_package_replacements {
                (
                    self.calculate_transaction_fee(existing_tx, utxo_set) as i64,
                    self.pooled_vsize(&existing_tx_hash, existing_tx, utxo_set),
                )
            } else {
                let totals = self.package_fee_and_vsize(&package, utxo_set);
                if package.len() > 1 {
                    debug!(
                        "RBF replacement: comparing against {} txs ({} sat total fee)",
                        package.len(),
                        totals.0
                    );
                }
                totals
            }
        } else {
            (
                self.calculate_transaction_fee(existing_tx, utxo_set) as i64,
                self.pooled_vsize(&existing_tx_hash, existing_tx, utxo_set),
            )
        };

        let new_fee = self.calculate_transaction_fee(new_tx, utxo_set) as i64;
        let new_tx_size = self.admit_vsize(new_tx, new_witnesses, utxo_set).0;

        if new_tx_size == 0 || existing_tx_size == 0 {
            return Ok(false);
        }

        // Check fee rate multiplier (mode-specific)
        let new_fee_scaled = (new_fee as u128)
            .checked_mul(existing_tx_size as u128)
            .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?;
        let existing_fee_scaled = (existing_fee as u128)
            .checked_mul(new_tx_size as u128)
            .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?;

        if !replacement_clears_fee_rate(
            new_fee_scaled,
            existing_fee_scaled,
            rbf_config.min_fee_rate_multiplier,
        )? {
            warn!(
                "RBF replacement rejected: fee rate increase insufficient (required: {:.2}x, got: {:.2}x) for tx {}",
                rbf_config.min_fee_rate_multiplier,
                (new_fee_scaled as f64) / (existing_fee_scaled as f64),
                hex::encode(existing_tx_hash)
            );
            return Ok(false);
        }

        // The replacement pays for its own relay. The incremental relay fee is
        // satoshis per kilovirtualbyte, rounded up across the replacement's virtual size.
        let rate_sat_per_kvb = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.incremental_relay_fee)
            .unwrap_or(100) as u128;
        let bandwidth = rate_sat_per_kvb
            .saturating_mul(new_tx_size as u128)
            .div_ceil(1000);
        // Standard mode is Core: the incremental relay fee is the whole bump.
        // The flat `min_fee_bump_satoshis` floor is the conservative and aggressive knob.
        let required = match rbf_config.mode {
            crate::config::RbfMode::Standard => bandwidth as i128,
            _ => (bandwidth as i128).max(rbf_config.min_fee_bump_satoshis as i128),
        };
        if (new_fee as i128) < existing_fee as i128 + required {
            warn!(
                "RBF replacement rejected: absolute fee bump insufficient (required: {} sat, got: {} sat) for tx {}",
                required,
                new_fee - existing_fee,
                hex::encode(existing_tx_hash)
            );
            return Ok(false);
        }

        // Conservative mode: Check minimum confirmations
        // Note: Transactions in mempool have 0 confirmations. This check ensures that
        // if a transaction has been confirmed (which shouldn't be in mempool), we require
        // it to have minimum confirmations before allowing replacement.
        // In practice, mempool transactions will always have 0 confirmations, so this
        // check mainly serves as a safety mechanism.
        if matches!(rbf_config.mode, crate::config::RbfMode::Conservative)
            && rbf_config.min_confirmations > 0
            && let Some(storage) = storage
        {
            // Check if transaction is in blockchain and has enough confirmations
            if let Ok(Some(metadata)) = storage.transactions().get_metadata(&existing_tx_hash) {
                // Transaction is in a block - check confirmations
                let block_hash = metadata.block_hash;
                if let Ok(Some(block_height)) = storage.blocks().get_height_by_hash(&block_hash)
                    && let Ok(Some(tip_height)) = storage.chain().get_height()
                {
                    let confirmations = tip_height.saturating_sub(block_height) + 1;
                    if confirmations < rbf_config.min_confirmations as u64 {
                        warn!(
                            "RBF replacement rejected: conservative mode requires {} confirmations, tx {} has {}",
                            rbf_config.min_confirmations,
                            hex::encode(existing_tx_hash),
                            confirmations
                        );
                        return Ok(false);
                    }
                }
            }
            // If transaction is not in blockchain (mempool only), confirmations = 0
            // For conservative mode, we might want to reject replacements of unconfirmed transactions
            // if min_confirmations > 0, but that would prevent all mempool RBF replacements.
            // So we allow it - the transaction is still in mempool and can be replaced.
        }

        // Check conflict (must spend at least one input from existing tx)
        if !has_conflict_with_tx(new_tx, existing_tx) {
            return Ok(false);
        }

        // The consensus helper only prices coins in the set it is given. An unrelated
        // mempool parent is spendable; an output this replacement removes is not.
        let replacement_view = self.replacement_input_view(new_tx, existing_tx, utxo_set);
        let bip125_result = replacement_checks_with_witness(
            new_tx,
            existing_tx,
            &replacement_view,
            &self.mempool.read().unwrap_or_else(|e| e.into_inner()),
            new_witnesses,
            None,
        )?;
        if !bip125_result {
            // BIP125 check failed (likely new dependencies issue)
            return Ok(false);
        }

        // All checks passed
        Ok(true)
    }

    /// Check ancestor/descendant limits for a transaction
    fn check_ancestor_descendant_limits(
        &self,
        tx: &Transaction,
        tx_hash: &Hash,
        policy: &MempoolPolicyConfig,
        candidate_vsize: u64,
    ) -> Result<bool> {
        // Admission removes this set before the same count. A dry-run has not.
        let removed = self.txs_this_replacement_removes(tx);
        // The candidate is not in the pool yet. Seed the walk from its inputs.
        // A package dry-run also sees earlier transactions in that package.
        let preview = self
            .package_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut ancestors = HashSet::with_capacity(10);
        let mut to_process = Vec::with_capacity(10);
        {
            let pool = self.pool_lock();
            for input in &tx.inputs {
                let parent = input.prevout.hash;
                if removed.contains(&parent) {
                    continue;
                }
                if pool.transactions.contains_key(&parent) || preview.txs.contains_key(&parent) {
                    ancestors.insert(parent);
                    to_process.push(parent);
                }
            }
        }
        let mut processed = HashSet::with_capacity(10);

        while let Some(current_hash) = to_process.pop() {
            if processed.contains(&current_hash) {
                continue;
            }
            processed.insert(current_hash);

            // Single pool critical section — nested pool_lock() would deadlock (non-reentrant Mutex).
            let parents = {
                let pool = self.pool_lock();
                if let Some(current_tx) = pool.transactions.get(&current_hash) {
                    current_tx
                        .inputs
                        .iter()
                        .map(|input| input.prevout.hash)
                        .filter(|parent| {
                            !removed.contains(parent)
                                && (pool.transactions.contains_key(parent)
                                    || preview.txs.contains_key(parent))
                        })
                        .collect::<Vec<_>>()
                } else if let Some((_, parents)) = preview.txs.get(&current_hash) {
                    parents.clone()
                } else {
                    Vec::new()
                }
            };
            for parent_hash in parents {
                if removed.contains(&parent_hash) {
                    continue;
                }
                if ancestors.insert(parent_hash) {
                    to_process.push(parent_hash);
                }
            }
        }

        // Calculate ancestor count and size
        let ancestor_count = ancestors.len() as u32;
        let ancestor_size: u64 = {
            let pool = self.pool_lock();
            ancestors
                .iter()
                .map(|hash| {
                    pool.adjusted_vsize
                        .get(hash)
                        .copied()
                        .or_else(|| preview.txs.get(hash).map(|(vsize, _)| *vsize))
                        .unwrap_or(0)
                })
                .sum()
        };

        // Check ancestor limits
        if ancestor_count + 1 > policy.max_ancestor_count {
            warn!(
                "Transaction {} exceeds max ancestor count: {} > {}",
                hex::encode(tx_hash),
                ancestor_count + 1,
                policy.max_ancestor_count
            );
            return Ok(false);
        }

        if ancestor_size + candidate_vsize > policy.max_ancestor_size {
            warn!(
                "Transaction {} exceeds max ancestor size: {} > {}",
                hex::encode(tx_hash),
                ancestor_size + candidate_vsize,
                policy.max_ancestor_size
            );
            return Ok(false);
        }

        // The candidate is not in the pool yet. Count it on each in-pool ancestor.
        let descendants_map = self
            .tx_descendants
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for ancestor in &ancestors {
            let mut existing = descendant_closure(&descendants_map, ancestor);
            existing.extend(descendant_closure(&preview.descendants, ancestor));
            existing.retain(|hash| !removed.contains(hash));
            // The ancestor is part of its own descendant package.
            let descendant_count = existing.len() as u32 + 1;
            if descendant_count + 1 > policy.max_descendant_count {
                warn!(
                    "Transaction {} exceeds max descendant count: {} > {}",
                    hex::encode(tx_hash),
                    descendant_count + 1,
                    policy.max_descendant_count
                );
                return Ok(false);
            }
            let (ancestor_vsize, descendant_size): (u64, u64) = {
                let pool = self.pool_lock();
                let vsize_of = |hash: &Hash| {
                    pool.adjusted_vsize
                        .get(hash)
                        .copied()
                        .or_else(|| preview.txs.get(hash).map(|(vsize, _)| *vsize))
                        .unwrap_or(0)
                };
                let ancestor_vsize = vsize_of(ancestor);
                let descendant_size = existing.iter().map(vsize_of).sum();
                (ancestor_vsize, descendant_size)
            };
            if ancestor_vsize + descendant_size + candidate_vsize > policy.max_descendant_size {
                warn!(
                    "Transaction {} exceeds max descendant size: {} > {}",
                    hex::encode(tx_hash),
                    ancestor_vsize + descendant_size + candidate_vsize,
                    policy.max_descendant_size
                );
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Update dependency graph when a transaction is added
    fn update_dependency_graph(&self, tx: &Transaction, tx_hash: &Hash) {
        let mut dependencies = self
            .tx_dependencies
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let mut descendants = self
            .tx_descendants
            .write()
            .unwrap_or_else(|e| e.into_inner());

        // Initialize empty sets for this transaction
        dependencies.entry(*tx_hash).or_default();
        descendants.entry(*tx_hash).or_default();

        // The parent is the transaction id stored on the input.
        for input in &tx.inputs {
            let parent_hash = input.prevout.hash;
            if self.pool_lock().transactions.contains_key(&parent_hash) {
                dependencies
                    .entry(*tx_hash)
                    .or_default()
                    .insert(parent_hash);
                descendants.entry(parent_hash).or_default().insert(*tx_hash);
            }
        }
    }

    /// Dry-run admission against `utxo_set`. Does not insert the transaction.
    ///
    /// `Ok(fee_satoshis)` means the prevouts are present, the outputs are not dust
    /// unless this is the package's one dust output, the locktime and sequence locks are final, no input spends an immature
    /// coinbase, the outputs do not exceed the inputs, the input sum is within
    /// the money limit, the fee meets the relay floor, including the
    /// signature-operation size of `witnesses`, and the absolute minimum, a
    /// replacement pays for every transaction it removes plus its own relay,
    /// the non-witness size is
    /// at least 65 bytes, the ancestor and descendant packages, including this
    /// transaction and not the transactions this replacement removes, stay within
    /// the configured limits, and, when standardness is required, the version is
    /// 1 or 2, the weight,
    /// including `witnesses`, is at most 400,000, every scriptSig is push-only
    /// and at most 1,650 bytes, and every output
    /// script and spent script is standard, a pay-to-script-hash redeem has at
    /// most 15 signature operations, and the transaction has at most 2,500
    /// legacy signature operations. `Err` is the reject reason.
    pub fn preview_accept(
        &self,
        tx: &Transaction,
        witnesses: Option<&[Witness]>,
        utxo_set: &UtxoSet,
    ) -> Result<u64, &'static str> {
        if tx.inputs.is_empty() || tx.outputs.is_empty() {
            return Err("bad-txns-vin-empty");
        }
        if tx.outputs.iter().any(output_is_dust)
            && !self.ephemeral_dust_allowed(tx, blvm_protocol::block::calculate_tx_id(tx))
        {
            return Err("dust");
        }
        let require_standard = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.require_standard)
            .unwrap_or(true);
        if require_standard && !(1..=2).contains(&tx.version) {
            return Err("version");
        }
        if require_standard && self.transaction_weight(tx, witnesses) > MAX_STANDARD_TX_WEIGHT {
            return Err("tx-size");
        }
        if require_standard
            && tx
                .outputs
                .iter()
                .any(|output| !output_script_is_standard(output.script_pubkey.as_ref()))
        {
            return Err("scriptpubkey");
        }
        if require_standard
            && tx
                .inputs
                .iter()
                .any(|input| input.script_sig.len() > MAX_STANDARD_SCRIPTSIG_SIZE)
        {
            return Err("scriptsig-size");
        }
        if require_standard
            && tx
                .inputs
                .iter()
                .any(|input| !blvm_protocol::script::p2sh_push_only_check(&input.script_sig))
        {
            return Err("scriptsig-not-pushonly");
        }
        let nonwitness_size = blvm_protocol::wire::serialize_tx(tx)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if require_standard && nonwitness_size < MIN_STANDARD_TX_NONWITNESS_SIZE {
            return Err("tx-size-small");
        }
        // Same next-block height and median admission uses. No tip means height 0.
        let (height, median_time_past) = self.finality_context().unwrap_or((0, 0));
        if !blvm_protocol::mempool::is_final_tx(tx, height, median_time_past) {
            return Err("non-final");
        }
        if self.relative_lock_rejects(tx, utxo_set) {
            return Err("non-BIP68-final");
        }
        if self.immature_coinbase_spend(tx, utxo_set) {
            return Err("bad-txns-premature-spend-of-coinbase");
        }
        let missing = tx.inputs.iter().any(|input| {
            utxo_set.get(&input.prevout).is_none() && !self.mempool_creates(&input.prevout)
        });
        if missing {
            return Err("missing-inputs");
        }
        if require_standard
            && self.script_prevouts(tx, utxo_set).iter().any(|prevout| {
                prevout
                    .as_ref()
                    .is_some_and(|prevout| !spent_script_is_standard(prevout.script.as_ref()))
            })
        {
            return Err("bad-txns-nonstandard-inputs");
        }
        if require_standard && p2sh_redeem_sigops_exceed(tx, &self.script_prevouts(tx, utxo_set)) {
            return Err("bad-txns-nonstandard-inputs");
        }
        if self.prevout_sum_exceeds_money(tx, utxo_set) {
            return Err("bad-txns-in-belowout");
        }
        if self.outputs_exceed_inputs(tx, utxo_set) {
            return Err("bad-txns-in-belowout");
        }
        let fee = self.calculate_transaction_fee(tx, utxo_set);
        let rate = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.min_relay_fee_rate)
            .unwrap_or(100);
        let vsize = self.admit_vsize(tx, witnesses, utxo_set).0;
        let required = (rate as u128).saturating_mul(vsize as u128).div_ceil(1000);
        let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
        let waived = self
            .relay_floor_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&tx_hash);
        if !waived && (fee as u128) < required {
            return Err("min relay fee not met");
        }
        let policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();
        if fee < policy.min_tx_fee {
            return Err("min relay fee not met");
        }
        if !self
            .check_ancestor_descendant_limits(tx, &tx_hash, &policy, vsize)
            .unwrap_or(false)
        {
            return Err("too-long-mempool-chain");
        }
        if self.replacement_rejects(tx, witnesses, utxo_set, vsize) {
            return Err("insufficient fee");
        }
        Ok(fee)
    }

    /// True when `tx` spends an output a pooled transaction already spends and
    /// admission would refuse the replacement. Does not remove anything.
    fn replacement_rejects(
        &self,
        tx: &Transaction,
        witnesses: Option<&[Witness]>,
        utxo_set: &UtxoSet,
        candidate_vsize: u64,
    ) -> bool {
        let mut direct = Vec::new();
        for input in &tx.inputs {
            if let Some(existing_hash) = self.pool_lock().spenders.get(&input.prevout).copied()
                && !direct.contains(&existing_hash)
            {
                direct.push(existing_hash);
            }
        }
        if direct.is_empty() {
            return false;
        }
        let mut displaced = HashSet::new();
        for hash in &direct {
            displaced.extend(self.replacement_package(hash));
        }
        // One chain is one cluster however many transactions it holds.
        if self.conflicting_cluster_count(&direct) > 100 {
            return true;
        }
        let incremental = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.incremental_relay_fee)
            .unwrap_or(100);
        let (displaced_fee, _) = self.package_fee_and_vsize(&displaced, utxo_set);
        let new_fee = self.calculate_transaction_fee(tx, utxo_set) as i64;
        let bandwidth = (incremental as u128)
            .saturating_mul(candidate_vsize as u128)
            .div_ceil(1000);
        if (new_fee as i128) < displaced_fee as i128 + bandwidth as i128 {
            return true;
        }
        for existing_hash in &direct {
            let existing_tx = {
                let pool = self.pool_lock();
                pool.transactions.get(existing_hash).cloned()
            };
            let Some(existing_tx) = existing_tx else {
                continue;
            };
            match self.check_rbf_replacement_with_witness(
                tx,
                &existing_tx,
                utxo_set,
                None,
                witnesses,
            ) {
                Ok(true) => {}
                _ => return true,
            }
        }
        false
    }

    /// Script and witness checks admission runs after [`Self::preview_accept`].
    ///
    /// When standardness is required, a pay-to-witness-script-hash data item
    /// longer than 80 bytes, a witness script longer than 3,600 bytes, more
    /// than 100 items before that script, a pay-to-anchor witness that is not
    /// empty, a Taproot annex, or a tapscript data item longer than 80 bytes,
    /// is rejected here. A signature-operation cost above 16,000 is rejected
    /// here as well. The relay floor in [`Self::preview_accept`] uses these
    /// stacks for the signature-operation size.
    pub fn script_rejects(
        &self,
        tx: &Transaction,
        witnesses: Option<&[Witness]>,
        utxo_set: &UtxoSet,
    ) -> bool {
        let require_standard = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|policy| policy.require_standard);
        if require_standard
            && p2wsh_data_item_exceeds(tx, &self.script_prevouts(tx, utxo_set), witnesses)
        {
            return true;
        }
        if require_standard
            && p2wsh_script_exceeds(tx, &self.script_prevouts(tx, utxo_set), witnesses)
        {
            return true;
        }
        if require_standard
            && p2wsh_stack_count_exceeds(tx, &self.script_prevouts(tx, utxo_set), witnesses)
        {
            return true;
        }
        if require_standard
            && pay_to_anchor_witness_present(&self.script_prevouts(tx, utxo_set), witnesses)
        {
            return true;
        }
        if require_standard && taproot_annex_present(&self.script_prevouts(tx, utxo_set), witnesses)
        {
            return true;
        }
        if require_standard
            && tapscript_data_item_exceeds(&self.script_prevouts(tx, utxo_set), witnesses)
        {
            return true;
        }
        if consensus_checks_reject(
            tx,
            witnesses,
            &self.script_prevouts(tx, utxo_set),
            require_standard,
        ) {
            return true;
        }
        self.admit_vsize(tx, witnesses, utxo_set).1
            > blvm_protocol::mempool::MAX_STANDARD_TX_SIGOPS_COST
    }

    /// Add transaction to mempool
    /// Uses interior mutability so it can be called with Arc<MempoolManager> (re-broadcast, sendrawtransaction)
    pub fn add_transaction(&self, tx: Transaction) -> Result<bool> {
        self.add_transaction_with_witness(tx, None)
    }

    /// Record the package fee and dust waivers [`Self::add_package_with_witnesses`] uses.
    ///
    /// `chain_utxo` holds confirmed coins. Outputs of `txs` are visible to the
    /// waiver calculation, including a child that spends an earlier transaction
    /// in this package. [`Self::clear_package_preview`] drops the waivers.
    pub fn arm_package_preview(
        &self,
        txs: &[(Transaction, Option<Vec<Witness>>)],
        chain_utxo: &UtxoSet,
    ) {
        let mut package_utxo = chain_utxo.clone();
        for (tx, _) in txs {
            let txid = blvm_protocol::block::calculate_tx_id(tx);
            for (index, output) in tx.outputs.iter().enumerate() {
                package_utxo.insert(
                    OutPoint {
                        hash: txid,
                        index: index as u32,
                    },
                    std::sync::Arc::new(blvm_protocol::UTXO {
                        value: output.value,
                        script_pubkey: output.script_pubkey.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    }),
                );
            }
        }
        let rate = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.min_relay_fee_rate)
            .unwrap_or(100);
        *self
            .relay_floor_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            self.package_relay_waivers(txs, &package_utxo, rate);
        *self
            .ephemeral_dust_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.ephemeral_dust_parents(txs, &package_utxo);
    }

    /// Drop the waivers and ancestor record from [`Self::arm_package_preview`].
    pub fn clear_package_preview(&self) {
        self.relay_floor_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.ephemeral_dust_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self
            .package_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = PackagePreview::default();
    }

    /// Remember an allowed package transaction so a later one in the same dry-run counts it.
    pub fn note_package_preview(
        &self,
        tx: &Transaction,
        witnesses: Option<&[Witness]>,
        utxo_set: &UtxoSet,
    ) {
        let txid = blvm_protocol::block::calculate_tx_id(tx);
        let vsize = self.admit_vsize(tx, witnesses, utxo_set).0;
        let mut preview = self
            .package_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let parents = {
            let pool = self.pool_lock();
            tx.inputs
                .iter()
                .map(|input| input.prevout.hash)
                .filter(|parent| {
                    pool.transactions.contains_key(parent) || preview.txs.contains_key(parent)
                })
                .collect::<Vec<_>>()
        };
        for parent in &parents {
            preview.descendants.entry(*parent).or_default().insert(txid);
        }
        preview.txs.insert(txid, (vsize, parents));
    }

    /// Admit an ordered package. A transaction below the relay floor is kept when a
    /// later transaction in this package spends it and their combined fee covers both.
    pub fn add_package(&self, txs: Vec<Transaction>) -> Result<bool> {
        let flags =
            self.add_package_with_witnesses(txs.into_iter().map(|tx| (tx, None)).collect())?;
        Ok(!flags.is_empty() && flags.into_iter().all(|ok| ok))
    }

    /// Same as [`Self::add_package`], keeping each transaction's witness stacks.
    pub fn add_package_with_witnesses(
        &self,
        txs: Vec<(Transaction, Option<Vec<Witness>>)>,
    ) -> Result<Vec<bool>> {
        if txs.is_empty() {
            return Ok(Vec::new());
        }
        let wired = self
            .utxo_set_arc
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if wired {
            let mut package_utxo = {
                let slot = self.utxo_set_arc.read().unwrap_or_else(|e| e.into_inner());
                match slot.as_ref().and_then(|arc| arc.try_lock().ok()) {
                    Some(guard) => {
                        let mut snapshot = UtxoSet::default();
                        for (tx, _) in &txs {
                            for input in &tx.inputs {
                                if let Some(utxo) = guard.get(&input.prevout) {
                                    snapshot.insert(input.prevout, utxo.clone());
                                }
                            }
                        }
                        snapshot
                    }
                    None => {
                        warn!("Package rejected: UTXO set is locked");
                        return Ok(vec![false; txs.len()]);
                    }
                }
            };
            for (tx, _) in &txs {
                let txid = blvm_protocol::block::calculate_tx_id(tx);
                for (index, output) in tx.outputs.iter().enumerate() {
                    package_utxo.insert(
                        OutPoint {
                            hash: txid,
                            index: index as u32,
                        },
                        std::sync::Arc::new(blvm_protocol::UTXO {
                            value: output.value,
                            script_pubkey: output.script_pubkey.clone().into(),
                            height: 0,
                            is_coinbase: false,
                        }),
                    );
                }
            }
            let rate = self
                .policy_config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|policy| policy.min_relay_fee_rate)
                .unwrap_or(100);
            *self
                .relay_floor_waivers
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                self.package_relay_waivers(&txs, &package_utxo, rate);
            *self
                .ephemeral_dust_waivers
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                self.ephemeral_dust_parents(&txs, &package_utxo);
        }
        let ids: Vec<Hash> = txs
            .iter()
            .map(|(tx, _)| blvm_protocol::block::calculate_tx_id(tx))
            .collect();
        let closures: Vec<HashSet<Hash>> = (0..txs.len())
            .map(|start| Self::package_descendants(&txs, start))
            .collect();
        let package_ids: HashSet<Hash> = ids.iter().copied().collect();
        let spends_package: Vec<Vec<Hash>> = txs
            .iter()
            .map(|(tx, _)| {
                tx.inputs
                    .iter()
                    .map(|input| input.prevout.hash)
                    .filter(|hash| package_ids.contains(hash))
                    .collect()
            })
            .collect();
        let waived = {
            let relay = self
                .relay_floor_waivers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let dust = self
                .ephemeral_dust_waivers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut waived = relay;
            waived.extend(dust);
            waived
        };
        let mut flags = Vec::with_capacity(txs.len());
        let mut outcome = Ok(());
        let _package = FlagHold::enter(&self.admitting_package);
        for (tx, witnesses) in txs {
            match self.add_transaction_with_witness(tx, witnesses) {
                Ok(admitted) => flags.push(admitted),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        drop(_package);
        self.relay_floor_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.ephemeral_dust_waivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let admitted: HashSet<Hash> = ids
            .iter()
            .zip(flags.iter())
            .filter(|(_, ok)| **ok)
            .map(|(id, _)| *id)
            .collect();
        for (index, id) in ids.iter().enumerate() {
            if !waived.contains(id) || !admitted.contains(id) {
                continue;
            }
            if closures[index]
                .iter()
                .any(|member| !admitted.contains(member))
            {
                for member in &closures[index] {
                    if !admitted.contains(member) {
                        continue;
                    }
                    self.remove_transaction(member);
                    if let Some(flag_index) = ids.iter().position(|candidate| candidate == member)
                        && flag_index < flags.len()
                    {
                        flags[flag_index] = false;
                    }
                }
            }
        }
        loop {
            let mut removed_any = false;
            for (index, parents) in spends_package.iter().enumerate() {
                if index >= flags.len() || !flags[index] {
                    continue;
                }
                let parent_missing = parents.iter().any(|parent| {
                    ids.iter()
                        .position(|candidate| candidate == parent)
                        .is_some_and(|parent_index| {
                            parent_index >= flags.len() || !flags[parent_index]
                        })
                });
                if parent_missing {
                    self.remove_transaction(&ids[index]);
                    flags[index] = false;
                    removed_any = true;
                }
            }
            if !removed_any {
                break;
            }
        }
        outcome?;
        Ok(flags)
    }

    /// Parents and the last transaction whose own fee is below `rate` satoshis per
    /// 1,000 virtual bytes, when those parents and that child together meet it.
    /// The child is counted once.
    fn package_relay_waivers(
        &self,
        txs: &[(Transaction, Option<Vec<Witness>>)],
        utxo: &UtxoSet,
        rate: u64,
    ) -> HashSet<Hash> {
        let ids: Vec<Hash> = txs
            .iter()
            .map(|(tx, _)| blvm_protocol::block::calculate_tx_id(tx))
            .collect();
        let spent_by_last: HashSet<Hash> = txs
            .last()
            .map(|(tx, _)| tx.inputs.iter().map(|input| input.prevout.hash).collect())
            .unwrap_or_default();
        let mut waive = HashSet::new();
        let mut fee = 0u64;
        let mut vsize = 0u64;
        let mut below = Vec::new();
        for (index, (tx, witnesses)) in txs.iter().enumerate() {
            let is_last = index + 1 == txs.len();
            let is_parent = spent_by_last.contains(&ids[index]);
            if !is_last && !is_parent {
                continue;
            }
            let tx_fee = self.calculate_transaction_fee(tx, utxo);
            let tx_vsize = self.admit_vsize(tx, witnesses.as_deref(), utxo).0;
            fee = fee.saturating_add(tx_fee);
            vsize = vsize.saturating_add(tx_vsize);
            let own_required = (rate as u128)
                .saturating_mul(tx_vsize as u128)
                .div_ceil(1000);
            if (tx_fee as u128) < own_required {
                below.push(ids[index]);
            }
        }
        let required = (rate as u128).saturating_mul(vsize as u128).div_ceil(1000);
        if vsize == 0 || (fee as u128) < required {
            return waive;
        }
        waive.extend(below);
        waive
    }

    /// `txs[start]` plus every later transaction in the package that spends one of those.
    fn package_descendants(
        txs: &[(Transaction, Option<Vec<Witness>>)],
        start: usize,
    ) -> HashSet<Hash> {
        let ids: Vec<Hash> = txs
            .iter()
            .map(|(tx, _)| blvm_protocol::block::calculate_tx_id(tx))
            .collect();
        let mut members = HashSet::new();
        if start >= ids.len() {
            return members;
        }
        members.insert(ids[start]);
        loop {
            let before = members.len();
            for (index, (tx, _)) in txs.iter().enumerate() {
                if members.contains(&ids[index]) {
                    continue;
                }
                if tx
                    .inputs
                    .iter()
                    .any(|input| members.contains(&input.prevout.hash))
                {
                    members.insert(ids[index]);
                }
            }
            if members.len() == before {
                break;
            }
        }
        members
    }

    /// A zero-fee parent with exactly one dust output, spent by a later package transaction.
    fn ephemeral_dust_parents(
        &self,
        txs: &[(Transaction, Option<Vec<Witness>>)],
        utxo: &UtxoSet,
    ) -> HashSet<Hash> {
        let mut waive = HashSet::new();
        for (index, (tx, _)) in txs.iter().enumerate() {
            if self.calculate_transaction_fee(tx, utxo) != 0 {
                continue;
            }
            let dust: Vec<u32> = tx
                .outputs
                .iter()
                .enumerate()
                .filter(|(_, output)| output_is_dust(output))
                .map(|(output_index, _)| output_index as u32)
                .collect();
            if dust.len() != 1 {
                continue;
            }
            let txid = blvm_protocol::block::calculate_tx_id(tx);
            let spent = OutPoint {
                hash: txid,
                index: dust[0],
            };
            let child_spends = txs
                .iter()
                .skip(index + 1)
                .any(|(later, _)| later.inputs.iter().any(|input| input.prevout == spent));
            if child_spends {
                waive.insert(txid);
            }
        }
        waive
    }

    /// One dust output is allowed only for the package parent recorded in [`Self::ephemeral_dust_parents`].
    fn ephemeral_dust_allowed(&self, tx: &Transaction, tx_hash: Hash) -> bool {
        let dust = tx
            .outputs
            .iter()
            .filter(|output| output_is_dust(output))
            .count();
        dust == 1
            && self
                .ephemeral_dust_waivers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&tx_hash)
    }

    /// Add transaction with optional SegWit witness stacks (one per input).
    pub fn add_transaction_with_witness(
        &self,
        tx: Transaction,
        witnesses: Option<Vec<Witness>>,
    ) -> Result<bool> {
        debug!("Adding transaction to mempool");

        use blvm_protocol::block::calculate_tx_id;
        let tx_hash = calculate_tx_id(&tx);

        // Reject duplicate — already in pool.
        if self.pool_lock().transactions.contains_key(&tx_hash) {
            debug!("Transaction {} already in mempool", hex::encode(tx_hash));
            return Ok(false);
        }

        // Reject transactions with duplicate inputs (invalid by consensus).
        {
            let mut seen = HashSet::with_capacity(tx.inputs.len());
            for input in &tx.inputs {
                if !seen.insert(input.prevout) {
                    warn!(
                        "Transaction {} rejected: duplicate input {:?}",
                        hex::encode(tx_hash),
                        input.prevout
                    );
                    return Ok(false);
                }
            }
        }

        // Reject empty transactions.
        if tx.inputs.is_empty() || tx.outputs.is_empty() {
            warn!(
                "Transaction {} rejected: empty inputs ({}) or outputs ({})",
                hex::encode(tx_hash),
                tx.inputs.len(),
                tx.outputs.len()
            );
            return Ok(false);
        }

        let incoming = accounted_tx_bytes(&tx, witnesses.as_deref());
        let cap_bytes = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.max_mempool_mb)
            .unwrap_or(300)
            .saturating_mul(1_048_576);
        if incoming > cap_bytes {
            warn!(
                "Transaction {} rejected: mempool byte cap ({} > {})",
                hex::encode(tx_hash),
                incoming,
                cap_bytes
            );
            return Ok(false);
        }

        let effective_policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        if effective_policy.require_standard && !(1..=2).contains(&tx.version) {
            warn!(
                "Transaction {} rejected: non-standard version {}",
                hex::encode(tx_hash),
                tx.version
            );
            return Ok(false);
        }
        let weight = self.transaction_weight(&tx, witnesses.as_deref());
        if effective_policy.require_standard && weight > MAX_STANDARD_TX_WEIGHT {
            warn!(
                "Transaction {} rejected: standard weight {} > {}",
                hex::encode(tx_hash),
                weight,
                MAX_STANDARD_TX_WEIGHT
            );
            return Ok(false);
        }
        let nonwitness_size = blvm_protocol::wire::serialize_tx(&tx)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if effective_policy.require_standard && nonwitness_size < MIN_STANDARD_TX_NONWITNESS_SIZE {
            warn!(
                "Transaction {} rejected: non-witness size {} is below {}",
                hex::encode(tx_hash),
                nonwitness_size,
                MIN_STANDARD_TX_NONWITNESS_SIZE
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && tx
                .outputs
                .iter()
                .any(|output| !output_script_is_standard(output.script_pubkey.as_ref()))
        {
            warn!(
                "Transaction {} rejected: non-standard output",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && tx
                .inputs
                .iter()
                .any(|input| !blvm_protocol::script::p2sh_push_only_check(&input.script_sig))
        {
            warn!(
                "Transaction {} rejected: scriptSig is not push-only",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && tx
                .inputs
                .iter()
                .any(|input| input.script_sig.len() > MAX_STANDARD_SCRIPTSIG_SIZE)
        {
            warn!(
                "Transaction {} rejected: scriptSig is {} bytes",
                hex::encode(tx_hash),
                tx.inputs
                    .iter()
                    .map(|input| input.script_sig.len())
                    .max()
                    .unwrap_or(0)
            );
            return Ok(false);
        }

        if effective_policy.reject_spam_in_mempool {
            use blvm_protocol::spam_filter::SpamFilter;
            let filter = effective_policy
                .spam_filter
                .as_ref()
                .map(|cfg| SpamFilter::with_config(cfg.clone().into()))
                .unwrap_or_default();
            if filter
                .is_spam_with_witness(&tx, witnesses.as_deref(), None)
                .is_spam
            {
                warn!(
                    "Transaction {} rejected: classified as spam",
                    hex::encode(tx_hash)
                );
                return Ok(false);
            }
        }

        // Min-relay-fee gate: reject transactions whose fee rate is below the
        // configured floor.  We need the UTXO set to compute fees; if it hasn't
        // been wired in yet we skip this check (startup path).
        // try_lock stays non-blocking. A wired set that is busy is not an empty
        // set: admitting against that would skip script, prevout, and fee checks.
        let (mut utxo_snapshot, utxo_wired) = {
            let utxo_slot = self.utxo_set_arc.read().unwrap_or_else(|e| e.into_inner());
            if let Some(arc) = utxo_slot.as_ref() {
                match arc.try_lock() {
                    Ok(guard) => {
                        let mut snapshot = UtxoSet::default();
                        for input in &tx.inputs {
                            if let Some(utxo) = guard.get(&input.prevout) {
                                snapshot.insert(input.prevout, utxo.clone());
                            }
                        }
                        (snapshot, true)
                    }
                    Err(_) => {
                        warn!(
                            "Transaction {} rejected: UTXO set is locked",
                            hex::encode(tx_hash)
                        );
                        return Ok(false);
                    }
                }
            } else {
                (UtxoSet::default(), false)
            }
        };
        if utxo_wired {
            self.fill_snapshot_misses(&tx, &mut utxo_snapshot);
        }

        let witness_ref = witnesses.as_deref();
        // No tip yet: only locktime 0, or a final sequence, is final at height 0.
        let (height, median_time_past) = self.finality_context().unwrap_or((0, 0));
        if !blvm_protocol::mempool::is_final_tx(&tx, height, median_time_past) {
            warn!(
                "Transaction {} rejected: locktime is not final",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.relative_lock_rejects(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: relative locktime is not final",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if tx.outputs.iter().any(output_is_dust) && !self.ephemeral_dust_allowed(&tx, tx_hash) {
            warn!(
                "Transaction {} rejected: output value is below the dust floor",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.immature_coinbase_spend(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: coinbase output is immature",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.unknown_prevout(&tx, &utxo_snapshot, utxo_wired) {
            warn!(
                "Transaction {} rejected: input prevout is not available",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.prevout_value_out_of_range(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: prevout value is outside the money range",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.prevout_sum_exceeds_money(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: prevout sum exceeds MAX_MONEY",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.outputs_exceed_inputs(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: output sum exceeds input sum",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && p2sh_redeem_sigops_exceed(&tx, &self.script_prevouts(&tx, &utxo_snapshot))
        {
            warn!(
                "Transaction {} rejected: P2SH redeem script exceeds {} sigops",
                hex::encode(tx_hash),
                MAX_STANDARD_P2SH_SIGOPS
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && self
                .script_prevouts(&tx, &utxo_snapshot)
                .iter()
                .any(|prevout| {
                    prevout.as_ref().is_some_and(|prevout| {
                        let script = prevout.script.as_ref();
                        blvm_protocol::witness::is_upgradable_witness_program(script)
                            && !blvm_protocol::witness::is_pay_to_anchor(script)
                    })
                })
        {
            warn!(
                "Transaction {} rejected: spent witness program is undefined",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && pay_to_anchor_witness_present(
                &self.script_prevouts(&tx, &utxo_snapshot),
                witness_ref,
            )
        {
            warn!(
                "Transaction {} rejected: pay-to-anchor witness is not empty",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && self
                .script_prevouts(&tx, &utxo_snapshot)
                .iter()
                .any(|prevout| {
                    prevout
                        .as_ref()
                        .is_some_and(|prevout| !spent_script_is_standard(prevout.script.as_ref()))
                })
        {
            warn!(
                "Transaction {} rejected: spent script is not standard",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && p2wsh_data_item_exceeds(&tx, &self.script_prevouts(&tx, &utxo_snapshot), witness_ref)
        {
            warn!(
                "Transaction {} rejected: P2WSH witness data item exceeds {} bytes",
                hex::encode(tx_hash),
                MAX_STANDARD_P2WSH_STACK_ITEM_SIZE
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && p2wsh_script_exceeds(&tx, &self.script_prevouts(&tx, &utxo_snapshot), witness_ref)
        {
            warn!(
                "Transaction {} rejected: P2WSH witness script exceeds {} bytes",
                hex::encode(tx_hash),
                MAX_STANDARD_P2WSH_SCRIPT_SIZE
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && p2wsh_stack_count_exceeds(
                &tx,
                &self.script_prevouts(&tx, &utxo_snapshot),
                witness_ref,
            )
        {
            warn!(
                "Transaction {} rejected: P2WSH witness has more than {} stack items",
                hex::encode(tx_hash),
                MAX_STANDARD_P2WSH_STACK_ITEMS
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && taproot_annex_present(&self.script_prevouts(&tx, &utxo_snapshot), witness_ref)
        {
            warn!(
                "Transaction {} rejected: Taproot annex is non-standard",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if effective_policy.require_standard
            && tapscript_data_item_exceeds(&self.script_prevouts(&tx, &utxo_snapshot), witness_ref)
        {
            warn!(
                "Transaction {} rejected: tapscript witness data item exceeds {} bytes",
                hex::encode(tx_hash),
                MAX_STANDARD_TAPSCRIPT_STACK_ITEM_SIZE
            );
            return Ok(false);
        }
        if consensus_checks_reject(
            &tx,
            witness_ref,
            &self.script_prevouts(&tx, &utxo_snapshot),
            effective_policy.require_standard,
        ) {
            warn!(
                "Transaction {} rejected: failed consensus checks",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        let (candidate_vsize, sigop_cost) = self.admit_vsize(&tx, witness_ref, &utxo_snapshot);
        if sigop_cost > blvm_protocol::mempool::MAX_STANDARD_TX_SIGOPS_COST {
            warn!(
                "Transaction {} rejected: sigop cost {} exceeds standard limit {}",
                hex::encode(tx_hash),
                sigop_cost,
                blvm_protocol::mempool::MAX_STANDARD_TX_SIGOPS_COST
            );
            return Ok(false);
        }

        let mut fee_rate_sat_vb = 0u64;
        let mut fee_sats = 0u64;
        if utxo_wired {
            let fee = self.calculate_transaction_fee(&tx, &utxo_snapshot);
            fee_sats = fee;
            let tx_size = candidate_vsize;
            fee_rate_sat_vb = if tx_size > 0 { fee / tx_size } else { 0 };
            let required_fee = (effective_policy.min_relay_fee_rate as u128)
                .saturating_mul(tx_size as u128)
                .div_ceil(1000);
            let waived = self
                .relay_floor_waivers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&tx_hash);
            if !waived && (fee as u128) < required_fee {
                warn!(
                    "Transaction {} rejected: fee {} sat below relay minimum {} sat ({} sat/kvB)",
                    hex::encode(tx_hash),
                    fee,
                    required_fee,
                    effective_policy.min_relay_fee_rate
                );
                return Ok(false);
            }
            if fee < effective_policy.min_tx_fee {
                warn!(
                    "Transaction {} rejected: absolute fee {} sat below min_tx_fee {} sat",
                    hex::encode(tx_hash),
                    fee,
                    effective_policy.min_tx_fee
                );
                return Ok(false);
            }
        }

        // Check for conflicts with existing mempool transactions
        // If conflict exists, check if RBF replacement is allowed
        let mut conflicting_tx_hashes: Vec<Hash> = Vec::new();
        for input in &tx.inputs {
            if let Some(existing_hash) = self.pool_lock().spenders.get(&input.prevout).copied()
                && !conflicting_tx_hashes.contains(&existing_hash)
            {
                conflicting_tx_hashes.push(existing_hash);
            }
        }

        // Descendants join the displacement set only when package replacements are on.
        let direct_conflicts = conflicting_tx_hashes.clone();
        if !conflicting_tx_hashes.is_empty() {
            let allow_packages = self
                .rbf_config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|cfg| cfg.allow_package_replacements)
                .unwrap_or(true);
            let mut displacement_set = HashSet::new();
            for hash in &direct_conflicts {
                let package = self.replacement_package(hash);
                if !allow_packages && package.len() > 1 {
                    return Ok(false);
                }
                if allow_packages {
                    displacement_set.extend(package);
                } else {
                    displacement_set.insert(*hash);
                }
            }
            conflicting_tx_hashes = displacement_set.into_iter().collect();
        }

        // If there are conflicts, attempt RBF replacement.
        // At most 100 distinct clusters. A chain of 25 still counts as one.
        if !conflicting_tx_hashes.is_empty() {
            let clusters = self.conflicting_cluster_count(&direct_conflicts);
            if clusters > 100 {
                debug!("Transaction conflicts with {clusters} clusters (> 100 limit)");
                return Ok(false);
            }

            // Pay for every transaction this replacement removes, once. A fee that
            // beats each conflict and stays under their sum still deletes both.
            let displaced: HashSet<Hash> = conflicting_tx_hashes.iter().copied().collect();
            let (displaced_fee, _) = self.package_fee_and_vsize(&displaced, &utxo_snapshot);
            let new_fee = self.calculate_transaction_fee(&tx, &utxo_snapshot) as i64;
            let bandwidth = (effective_policy.incremental_relay_fee as u128)
                .saturating_mul(candidate_vsize as u128)
                .div_ceil(1000);
            if (new_fee as i128) < displaced_fee as i128 + bandwidth as i128 {
                debug!(
                    "RBF replacement rejected: fee {} sat is below the {} sat it removes plus {} sat relay",
                    new_fee, displaced_fee, bandwidth
                );
                return Ok(false);
            }

            // Feerate and dependency checks still run against each directly conflicting tx.
            for &existing_hash in &direct_conflicts {
                let existing_clone = {
                    let pool = self.pool_lock();
                    pool.transactions.get(&existing_hash).cloned()
                };
                let Some(ref existing_tx) = existing_clone else {
                    continue;
                };
                if !self.check_rbf_replacement_with_witness(
                    &tx,
                    existing_tx,
                    &utxo_snapshot,
                    None,
                    witness_ref,
                )? {
                    debug!(
                        "RBF replacement rejected for conflicting tx {}",
                        hex::encode(existing_hash)
                    );
                    return Ok(false);
                }
            }

            // All checks passed — remove every displaced transaction.
            for existing_hash in &conflicting_tx_hashes {
                debug!(
                    "RBF replacement: removing displaced transaction {}",
                    hex::encode(existing_hash)
                );
                self.remove_transaction(existing_hash);
            }

            // Update RBF tracking (anchor to the primary direct conflict).
            let primary_hash = direct_conflicts[0];
            let original_hash = {
                let tracking = self.rbf_tracking.read().unwrap_or_else(|e| e.into_inner());
                tracking
                    .get(&primary_hash)
                    .map(|t| t.original_tx_hash)
                    .unwrap_or(primary_hash)
            };
            let replacement_count = {
                let tracking = self.rbf_tracking.read().unwrap_or_else(|e| e.into_inner());
                let counts = direct_conflicts.iter().map(|hash| {
                    tracking
                        .get(hash)
                        .map(|entry| entry.replacement_count)
                        .unwrap_or(0)
                });
                next_replacement_count(counts)
            };
            {
                let mut tracking = self.rbf_tracking.write().unwrap_or_else(|e| e.into_inner());
                tracking.insert(
                    tx_hash,
                    RbfTracking {
                        replacement_count,
                        last_replacement_time: Self::current_timestamp(),
                        original_tx_hash: original_hash,
                    },
                );
                for h in &conflicting_tx_hashes {
                    tracking.remove(h);
                }
            }
        } else {
            // No conflict - check if inputs are already spent
            for input in &tx.inputs {
                if self.pool_lock().spent_outputs.contains(&input.prevout) {
                    debug!("Transaction conflicts with existing mempool transaction");
                    return Ok(false);
                }
            }
        }

        // Check ancestor/descendant limits before adding (uses effective_policy from above).
        if !self.check_ancestor_descendant_limits(
            &tx,
            &tx_hash,
            &effective_policy,
            candidate_vsize,
        )? {
            warn!(
                "Transaction {} rejected: exceeds ancestor/descendant limits",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }

        if let Some(wits) = &witnesses
            && wits.len() != tx.inputs.len()
        {
            warn!(
                "Transaction {} rejected: witness count {} != input count {}",
                hex::encode(tx_hash),
                wits.len(),
                tx.inputs.len()
            );
            return Ok(false);
        }

        // Add transaction to mempool (store full transaction + optional witnesses)
        {
            let wtxid = crate::network::txhash::calculate_wtxid(&tx, witnesses.as_deref());
            let mut pool = self.pool_lock();
            if pool.transactions.len() >= effective_policy.max_mempool_txs {
                warn!(
                    "Transaction {} rejected: mempool already holds {} transactions",
                    hex::encode(tx_hash),
                    pool.transactions.len()
                );
                return Ok(false);
            }
            pool.transactions.insert(tx_hash, tx.clone());
            pool.wtxids.insert(tx_hash, wtxid);
            pool.adjusted_vsize.insert(tx_hash, candidate_vsize);
            if let Some(wits) = witnesses {
                pool.tx_witnesses.insert(tx_hash, wits);
            }
            for input in &tx.inputs {
                pool.spent_outputs.insert(input.prevout);
                pool.spenders.insert(input.prevout, tx_hash);
            }
        }
        self.mempool
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tx_hash);

        // Update dependency graph
        self.update_dependency_graph(&tx, &tx_hash);

        // Record timestamp and the chain tip at admission.
        self.tx_timestamps
            .write()
            .unwrap()
            .insert(tx_hash, Self::current_timestamp());
        let admission_height = self
            .finality_context()
            .map(|(next_height, _)| next_height.saturating_sub(1))
            .unwrap_or(0);
        self.tx_heights
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tx_hash, admission_height);

        // Cache fee rate at insert (sat/vB) so eviction/sorting do not see stale zero rates.
        let fee_rate = fee_rate_sat_vb;
        self.fee_cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tx_hash, fee_rate);
        self.fee_basis
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tx_hash, (fee_sats, candidate_vsize));
        self.fee_index
            .write()
            .unwrap()
            .entry(Reverse(fee_rate))
            .or_default()
            .push(tx_hash);
        self.invalidate_fee_ranking();

        let from_reload = self.loading_from_disk.load(Ordering::Acquire);
        let from_package = self.admitting_package.load(Ordering::Acquire);
        let parents = self.dependency_hashes(&tx_hash);
        if let Some(estimator) = self.fee_estimator() {
            estimator.on_tx_admitted(TxAdmission {
                txid: tx_hash,
                fee_sats,
                vsize: candidate_vsize,
                admit_height: admission_height,
                parents,
                from_reload,
                from_package,
            });
        }

        // Publish mempool transaction added event
        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let mempool_size = self.pool_lock().transactions.len();
            let fee_rate_f64 = fee_rate as f64;
            let tx_hash_clone = tx_hash;
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_transaction_added(&tx_hash_clone, fee_rate_f64, mempool_size)
                    .await;
            });
            // NewTransaction (use publish_event to avoid ZMQ Send issues in spawn)
            let tx_hash_ev = tx_hash;
            let ep_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                let _ = ep_clone
                    .publish_event(
                        crate::module::traits::EventType::NewTransaction,
                        crate::module::ipc::protocol::EventPayload::NewTransaction {
                            tx_hash: tx_hash_ev,
                        },
                    )
                    .await;
            });
        }

        Ok(true)
    }

    /// Get mempool size
    pub fn size(&self) -> usize {
        self.pool_lock().transactions.len()
    }

    /// Get mempool transaction hashes
    pub fn transaction_hashes(&self) -> Vec<Hash> {
        self.pool_lock().transactions.keys().cloned().collect()
    }

    /// Get stored SegWit witness stacks for a mempool transaction (one per input).
    pub fn get_transaction_witnesses(&self, hash: &Hash) -> Option<Vec<Witness>> {
        self.pool_lock().tx_witnesses.get(hash).cloned()
    }

    /// Satoshis per virtual byte recorded when the transaction was accepted.
    pub fn cached_fee_rate(&self, hash: &Hash) -> Option<u64> {
        self.fee_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
    }

    /// Absolute fee and vsize. The BIP133 filter uses this instead of the truncated sat/vB rate.
    pub fn cached_fee_basis(&self, hash: &Hash) -> Option<(u64, u64)> {
        self.fee_basis
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
    }

    /// Unix time when the transaction entered the pool.
    pub fn accepted_at(&self, hash: &Hash) -> u64 {
        self.tx_timestamps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    /// Chain tip height when this transaction was accepted.
    pub fn admission_height(&self, hash: &Hash) -> u64 {
        self.tx_heights
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    /// In-pool parents this transaction spends.
    pub fn dependency_hashes(&self, hash: &Hash) -> Vec<Hash> {
        self.tx_dependencies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// In-pool children that spend this transaction.
    pub fn descendant_hashes(&self, hash: &Hash) -> Vec<Hash> {
        self.tx_descendants
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// True when a pool transaction spends this outpoint.
    pub fn spends_outpoint(&self, outpoint: &OutPoint) -> bool {
        self.pool_lock().spent_outputs.contains(outpoint)
    }

    pub(crate) fn spender_of(&self, outpoint: &OutPoint) -> Option<Hash> {
        self.pool_lock().spenders.get(outpoint).copied()
    }

    /// Cumulative mining priority fee delta (satoshis) from `prioritisetransaction`.
    pub fn get_fee_delta(&self, hash: &Hash) -> i64 {
        self.fee_deltas
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    /// Transactions whose witness id shortens to one of `short_ids`.
    ///
    /// Holds the pool lock only for the scan. Clones the matching bodies and
    /// leaves every other transaction in the pool.
    pub fn transactions_for_short_ids(
        &self,
        header: &blvm_protocol::BlockHeader,
        nonce: u64,
        short_ids: &[crate::network::compact_blocks::ShortTxId],
    ) -> HashMap<Hash, (Transaction, Option<Vec<Witness>>)> {
        use crate::network::compact_blocks::calculate_short_tx_id;
        let wanted: HashSet<crate::network::compact_blocks::ShortTxId> =
            short_ids.iter().copied().collect();
        let pool = self.pool_lock();
        let mut matched = HashMap::new();
        for (txid, tx) in &pool.transactions {
            let wtxid = pool.wtxids.get(txid).copied().unwrap_or_else(|| {
                crate::network::txhash::calculate_wtxid(
                    tx,
                    pool.tx_witnesses.get(txid).map(Vec::as_slice),
                )
            });
            let short_id = calculate_short_tx_id(header, &wtxid, nonce);
            if wanted.contains(&short_id) {
                matched.insert(wtxid, (tx.clone(), pool.tx_witnesses.get(txid).cloned()));
            }
        }
        matched
    }

    /// Get transaction by hash
    pub fn get_transaction(&self, hash: &Hash) -> Option<Transaction> {
        self.pool_lock().transactions.get(hash).cloned()
    }

    /// Get all transactions
    pub fn get_transactions(&self) -> Vec<Transaction> {
        self.pool_lock().transactions.values().cloned().collect()
    }

    /// Txid and cached fee basis for a mempool inventory. The transaction body stays in the pool.
    pub fn pooled_fee_basis(&self) -> Vec<(Hash, Option<(u64, u64)>)> {
        let hashes: Vec<Hash> = self.pool_lock().transactions.keys().copied().collect();
        let basis = self.fee_basis.read().unwrap_or_else(|e| e.into_inner());
        hashes
            .into_iter()
            .map(|hash| (hash, basis.get(&hash).copied()))
            .collect()
    }

    /// Get prioritized transactions by fee rate
    ///
    /// Returns transactions sorted by fee rate (satoshis per vbyte) in descending order.
    /// Requires UTXO set to calculate fee rates.
    ///
    /// Optimization: Uses sorted index (BTreeMap) for O(log n) insertion, O(1) top-N retrieval
    /// instead of O(n log n) sort on every call.
    pub fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &UtxoSet,
    ) -> Vec<Transaction> {
        // Recalculate fee rates and update index
        // Note: In a production system, we'd track UTXO set changes and only recalculate when needed
        self.update_fee_index(utxo_set);

        // Collect hashes under fee_index read only; do not call pool_lock while holding fee_index
        // (update_fee_index may interleave with remove_transaction — opposite lock order would deadlock).
        let mut ordered_hashes: Vec<Hash> = Vec::with_capacity(limit);
        {
            let fee_index = self.fee_index.read().unwrap_or_else(|e| e.into_inner());
            for (Reverse(_fee_rate), tx_hashes) in fee_index.iter() {
                for tx_hash in tx_hashes {
                    ordered_hashes.push(*tx_hash);
                    if ordered_hashes.len() >= limit {
                        break;
                    }
                }
                if ordered_hashes.len() >= limit {
                    break;
                }
            }
        }

        let mut result = Vec::with_capacity(limit.min(ordered_hashes.len()));
        let pool = self.pool_lock();
        for h in ordered_hashes {
            if let Some(tx) = pool.transactions.get(&h) {
                result.push(tx.clone());
                if result.len() >= limit {
                    break;
                }
            }
        }
        result
    }

    /// Apply a mining priority fee delta to a mempool transaction (cumulative).
    ///
    /// Returns `false` when the transaction is not in the mempool.
    pub fn prioritise_transaction(&self, hash: &Hash, fee_delta: i64) -> bool {
        if !self.pool_lock().transactions.contains_key(hash) {
            return false;
        }
        {
            let mut deltas = self.fee_deltas.write().unwrap_or_else(|e| e.into_inner());
            let entry = deltas.entry(*hash).or_insert(0);
            *entry = entry.saturating_add(fee_delta);
        }
        self.invalidate_fee_ranking();
        true
    }

    /// The fee index is keyed only by the chain UTXO set. Pool changes must drop
    /// that key so the next ranking rebuilds rates from the transactions now stored.
    fn invalidate_fee_ranking(&self) {
        *self
            .utxo_set_hash
            .write()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Calculate a simple hash of the UTXO set for change detection
    ///
    /// Uses a fast hash of UTXO set size and a sample of keys to detect changes.
    /// This is a heuristic - not perfect but fast enough for optimization purposes.
    fn calculate_utxo_set_hash(utxo_set: &UtxoSet) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut acc = utxo_set.len() as u64;
        for (outpoint, utxo) in utxo_set.iter() {
            let mut hasher = DefaultHasher::new();
            outpoint.hash(&mut hasher);
            utxo.value.hash(&mut hasher);
            acc ^= hasher.finish();
        }
        acc
    }

    /// Update fee index with current UTXO set
    ///
    /// Recalculates fee rates for all transactions and rebuilds the sorted index.
    ///
    /// Optimization: Only recalculates when UTXO set changes (incremental updates)
    /// Optimization: Batch UTXO lookups across all transactions for better cache locality
    fn update_fee_index(&self, utxo_set: &UtxoSet) {
        // Calculate current UTXO set hash
        let current_hash = Self::calculate_utxo_set_hash(utxo_set);

        // Check if UTXO set changed
        let mut last_hash = self
            .utxo_set_hash
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if Some(current_hash) == *last_hash {
            // UTXO set unchanged - skip recalculation
            drop(last_hash);
            return;
        }

        // UTXO set changed - update hash and recalculate
        *last_hash = Some(current_hash);
        drop(last_hash);

        // Clear existing index (we'll rebuild it)
        let mut fee_index = self.fee_index.write().unwrap_or_else(|e| e.into_inner());
        fee_index.clear();
        drop(fee_index);

        {
            let mut fee_cache = self.fee_cache.write().unwrap_or_else(|e| e.into_inner());
            fee_cache.clear();
            self.fee_basis
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }

        // Snapshot under pool lock only — never hold fee_cache/fee_index writes while locking pool
        // (avoids deadlock with remove_transaction: pool → fee_cache).
        let txs_snapshot: Vec<(Hash, Transaction)> = {
            let pool = self.pool_lock();
            pool.transactions
                .iter()
                .map(|(h, t)| (*h, t.clone()))
                .collect()
        };

        let all_prevouts: Vec<(Hash, OutPoint)> = txs_snapshot
            .iter()
            .flat_map(|(tx_hash, tx)| tx.inputs.iter().map(move |input| (*tx_hash, input.prevout)))
            .collect();

        let mut utxo_cache: HashMap<&OutPoint, u64> = HashMap::with_capacity(all_prevouts.len());
        let parents: HashMap<&Hash, &Transaction> =
            txs_snapshot.iter().map(|(hash, tx)| (hash, tx)).collect();
        for (_, prevout) in &all_prevouts {
            if let Some(utxo) = utxo_set.get(prevout) {
                if let Some(sats) = money_sats(utxo.value) {
                    utxo_cache.insert(prevout, sats);
                }
                continue;
            }
            let Some(parent) = parents.get(&prevout.hash) else {
                continue;
            };
            let Some(output) = parent.outputs.get(prevout.index as usize) else {
                continue;
            };
            if let Some(sats) = money_sats(output.value) {
                utxo_cache.insert(prevout, sats);
            }
        }

        for (tx_hash, tx) in &txs_snapshot {
            let mut input_total = 0u64;
            let mut within_money = true;
            let mut fee_known = true;
            for input in &tx.inputs {
                let Some(&value) = utxo_cache.get(&input.prevout) else {
                    fee_known = false;
                    break;
                };
                match add_money(input_total, value) {
                    Some(sum) => input_total = sum,
                    None => {
                        within_money = false;
                        break;
                    }
                }
            }
            if !fee_known {
                continue;
            }

            let fee = if within_money {
                match output_sum_sats(tx) {
                    Some(output_total) => input_total.saturating_sub(output_total),
                    None => 0,
                }
            } else {
                0
            };
            let size = self.pooled_vsize(tx_hash, tx, utxo_set) as usize;
            let delta = self
                .fee_deltas
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(tx_hash)
                .copied()
                .unwrap_or(0);
            let effective_fee = effective_ranked_fee(fee, delta);
            // Store fee rate in sat/vB (consistent with min_relay_fee_rate config units).
            let fee_rate = if size > 0 {
                effective_fee / size as u64
            } else {
                0
            };

            {
                let mut fee_cache = self.fee_cache.write().unwrap_or_else(|e| e.into_inner());
                fee_cache.insert(*tx_hash, fee_rate);
                self.fee_basis
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(*tx_hash, (effective_fee, size as u64));
            }
            let mut fee_index = self.fee_index.write().unwrap_or_else(|e| e.into_inner());
            fee_index
                .entry(Reverse(fee_rate))
                .or_default()
                .push(*tx_hash);
        }
    }

    /// Fee rates for the pooled transactions. Looks up those transactions' prevouts.
    /// An empty pool returns before any UTXO database read.
    pub fn fee_rates_for_storage(&self, storage: Option<&crate::storage::Storage>) -> Vec<u64> {
        if self.size() == 0 {
            return Vec::new();
        }
        let mut utxo_set = UtxoSet::default();
        if let Some(storage) = storage {
            for tx in self.get_transactions() {
                for input in &tx.inputs {
                    if let Ok(Some(utxo)) = storage.read_chain_prevout(&input.prevout) {
                        utxo_set.insert(input.prevout, std::sync::Arc::new(utxo));
                    }
                }
            }
        }
        self.fee_rates_sat_vb(&utxo_set)
    }

    /// Fee rates (sat/vB) for mempool transactions after refreshing the fee index.
    pub fn fee_rates_sat_vb(&self, utxo_set: &UtxoSet) -> Vec<u64> {
        self.update_fee_index(utxo_set);
        self.fee_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .copied()
            .collect()
    }

    /// Calculate transaction fee
    ///
    /// Fee = sum of inputs - sum of outputs
    ///
    /// Optimization: Uses batch UTXO lookup pattern for better cache locality
    pub fn calculate_transaction_fee(&self, tx: &Transaction, utxo_set: &UtxoSet) -> u64 {
        // Optimization: Batch UTXO lookups - collect all prevouts first, then lookup
        // This improves cache locality and reduces HashMap traversal overhead
        let prevouts: Vec<&OutPoint> = tx.inputs.iter().map(|input| &input.prevout).collect();

        // Batch UTXO lookup (single pass through HashMap)
        let mut input_total = 0u64;
        for prevout in prevouts {
            let sats = if let Some(utxo) = utxo_set.get(prevout) {
                money_sats(utxo.value)
            } else {
                self.mempool_input_value(prevout)
            };
            if let Some(sats) = sats {
                let Some(sum) = add_money(input_total, sats) else {
                    return 0;
                };
                input_total = sum;
            }
        }

        // An output outside the money range is not a fee. Casting it would wrap.
        let Some(output_total) = output_sum_sats(tx) else {
            return 0;
        };

        input_total.saturating_sub(output_total)
    }

    /// Legacy sigops count without prevouts. P2SH and witness sigops use chain UTXOs,
    /// and for a parent that exists only in this pool, that parent's `script_pubkey`.
    /// The spending witness is `witnesses`, not the parent's witness.
    fn admit_vsize(
        &self,
        tx: &Transaction,
        witnesses: Option<&[Witness]>,
        utxo_set: &UtxoSet,
    ) -> (u64, u64) {
        use blvm_protocol::UTXO;
        use blvm_protocol::mempool::{DEFAULT_BYTES_PER_SIGOP, sigop_adjusted_vsize};
        use blvm_protocol::script::flags::SEGWIT_STANDARD_FLAGS;
        use blvm_protocol::sigop::get_transaction_sigop_cost_with_utxos;

        let (bytes_per_sigop, count_sigops) = {
            let policy = self.policy_config.read().unwrap_or_else(|e| e.into_inner());
            match policy.as_ref() {
                Some(p) => (p.bytes_per_sigop, p.count_sigops),
                None => (DEFAULT_BYTES_PER_SIGOP, true),
            }
        };

        // A chain coin wins. An empty snapshot still has to price a mempool parent.
        // Skipping that lookup counted those pay-to-script-hash and witness sigops as zero.
        let pool = self.pool_lock();
        let owned_utxos: Vec<Option<UTXO>> = tx
            .inputs
            .iter()
            .map(|input| {
                if let Some(utxo) = utxo_set.get(&input.prevout) {
                    return Some((**utxo).clone());
                }
                pool.transactions
                    .get(&input.prevout.hash)
                    .and_then(|parent| parent.outputs.get(input.prevout.index as usize))
                    .map(|output| UTXO {
                        value: output.value,
                        script_pubkey: output.script_pubkey.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    })
            })
            .collect();
        let refs: Vec<Option<&UTXO>> = owned_utxos.iter().map(|utxo| utxo.as_ref()).collect();
        let sigop_cost = if count_sigops {
            get_transaction_sigop_cost_with_utxos(tx, &refs, witnesses, SEGWIT_STANDARD_FLAGS)
                .unwrap_or_else(|_| blvm_protocol::sigop::get_legacy_sigop_count(tx) as u64 * 4)
        } else {
            0
        };

        let weight = self.transaction_weight(tx, witnesses);
        (
            sigop_adjusted_vsize(weight, sigop_cost, bytes_per_sigop),
            sigop_cost,
        )
    }

    /// Stored adjusted vsize for a pooled transaction, recomputed if the map has no entry.
    fn pooled_vsize(&self, hash: &Hash, tx: &Transaction, utxo_set: &UtxoSet) -> u64 {
        if let Some(vsize) = self.pool_lock().adjusted_vsize.get(hash).copied() {
            return vsize;
        }
        let witnesses = self.pool_lock().tx_witnesses.get(hash).cloned();
        self.admit_vsize(tx, witnesses.as_deref(), utxo_set).0
    }

    /// Transaction weight in weight units.
    ///
    /// Non-empty stacks use the serialized witness weight. Stacks that are all
    /// empty are the legacy encoding, so the weight is four times the stripped size.
    fn transaction_weight(&self, tx: &Transaction, witnesses: Option<&[Witness]>) -> u64 {
        use blvm_protocol::segwit::transaction_weight_from_stacks;

        if let Some(wits) = witnesses
            && let Ok(weight) = transaction_weight_from_stacks(tx, Some(wits))
        {
            return weight;
        }
        self.estimate_transaction_weight(tx)
    }

    /// Guess used when witness stacks were not supplied.
    /// An empty `script_sig` is counted as a native segwit input.
    fn estimate_transaction_weight(&self, tx: &Transaction) -> u64 {
        let mut base_size: usize = 10;
        let mut witness_size: usize = 0;
        let mut segwit_inputs = 0usize;

        for input in &tx.inputs {
            base_size += 41 + input.script_sig.len();
            if input.script_sig.is_empty() {
                witness_size += 107;
                segwit_inputs += 1;
            }
        }
        for output in &tx.outputs {
            base_size += 9 + output.script_pubkey.len();
        }
        if segwit_inputs > 0 {
            witness_size += 2;
            (base_size * 4 + witness_size) as u64
        } else {
            (base_size as u64).saturating_mul(4)
        }
    }

    /// Estimate transaction size in vbytes.
    ///
    /// For transactions with SegWit inputs (detected by empty `script_sig`), applies
    /// an approximate witness weight discount.  The `Transaction` type in this
    /// codebase does not carry witness data inline, so we use the following
    /// heuristic per segwit input:
    ///   * P2WPKH witness: ~107 bytes at 1/4 weight → 107/4 ≈ 27 vbytes
    ///   * The 2-byte segwit marker/flag overhead is ~0.5 vbytes (negligible)
    ///
    /// Inputs with non-empty `script_sig` are assumed non-witness (or P2SH-wrapped).
    pub fn estimate_transaction_size(&self, tx: &Transaction) -> usize {
        // Base: version (4) + input count (var, ~1) + output count (var, ~1) + locktime (4)
        let mut base_size: usize = 10;
        let mut witness_size: usize = 0; // witness bytes (counted at 1/4 weight)
        let mut segwit_inputs = 0usize;

        for input in &tx.inputs {
            // prevout (36) + sequence (4) + script_sig length varint (~1) + script_sig
            base_size += 41 + input.script_sig.len();
            if input.script_sig.is_empty() {
                // Likely a native SegWit input; estimate P2WPKH witness (~107 bytes)
                // or P2WSH (~220 bytes). Use P2WPKH as the conservative estimate.
                witness_size += 107;
                segwit_inputs += 1;
            }
        }

        for output in &tx.outputs {
            // value (8) + script_pubkey length varint (~1) + script_pubkey
            base_size += 9 + output.script_pubkey.len();
        }

        if segwit_inputs > 0 {
            // SegWit marker (1) + flag (1) also counted at discount weight
            witness_size += 2;
            // vsize = ceil((base_size * 4 + witness_size) / 4)
            (base_size * 4 + witness_size).div_ceil(4)
        } else {
            base_size
        }
    }

    /// Apply a chain reorg to the pool.
    ///
    /// Transactions confirmed or conflicted by `connected` are removed. Non-coinbase
    /// transactions from `disconnected` are admitted again against `utxo_set`, oldest
    /// block first, so a child can spend a parent that was just restored.
    /// `disconnected_witnesses` is one list of input stacks per transaction in those
    /// blocks, including the coinbase slot.
    pub fn apply_reorg(
        &self,
        disconnected: &[Block],
        disconnected_witnesses: &[Vec<Vec<Witness>>],
        connected: &[Block],
        utxo_set: &UtxoSet,
    ) {
        use blvm_protocol::transaction::is_coinbase;

        if let Some(estimator) = self.fee_estimator() {
            estimator.on_reorg();
        }
        self.install_utxo_snapshot(utxo_set);
        for block in connected {
            self.remove_for_reorg_block(&block.transactions);
        }
        for (block_index, block) in disconnected.iter().enumerate() {
            let block_witnesses = disconnected_witnesses.get(block_index);
            for (tx_index, tx) in block.transactions.iter().enumerate() {
                if is_coinbase(tx) {
                    continue;
                }
                let witnesses = block_witnesses
                    .and_then(|stacks| stacks.get(tx_index))
                    .cloned();
                let _ = self.add_transaction_with_witness(tx.clone(), witnesses);
            }
        }
    }

    fn install_utxo_snapshot(&self, utxo_set: &UtxoSet) {
        let arc = {
            let slot = self.utxo_set_arc.read().unwrap_or_else(|e| e.into_inner());
            let Some(arc) = slot.as_ref() else {
                return;
            };
            Arc::clone(arc)
        };
        if let Ok(mut guard) = arc.try_lock() {
            *guard = utxo_set.clone();
            return;
        }
        let mut slot = self.utxo_set_arc.write().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = slot.as_ref()
            && let Ok(mut guard) = current.try_lock()
        {
            *guard = utxo_set.clone();
            return;
        }
        *slot = Some(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));
    }

    /// Drop transactions a newly connected block confirmed or conflicted with.
    ///
    /// A transaction included in the block is removed. A different transaction
    /// that spends an output the block already spent is removed, along with its
    /// descendants. A child that spends an output the block just created stays.
    /// No block observation is recorded. A removal here is an unconfirmed failure.
    /// A reorg uses its own removal path so it does not write fee history.
    pub fn remove_for_connected_block(&self, transactions: &[Transaction]) {
        self.remove_connected(None, transactions, true);
    }

    /// Remove a reorg's connected block without a fee-history failure or confirmation.
    fn remove_for_reorg_block(&self, transactions: &[Transaction]) {
        self.remove_connected(None, transactions, false);
    }

    /// Record `obs` on the fee estimator, then run [`Self::remove_for_connected_block`].
    ///
    /// The observation is delivered before any removal, so a transaction that was
    /// in the pool is a confirmation rather than a failure.
    pub fn remove_for_connected_block_at(
        &self,
        obs: Option<BlockObservation>,
        transactions: &[Transaction],
    ) {
        self.remove_connected(obs, transactions, true);
    }

    fn remove_connected(
        &self,
        obs: Option<BlockObservation>,
        transactions: &[Transaction],
        record_failures: bool,
    ) {
        if let Some(obs) = obs.as_ref()
            && let Some(estimator) = self.fee_estimator()
        {
            estimator.on_block_connected(obs);
        }
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::transaction::is_coinbase;

        let mut confirmed = HashSet::new();
        let mut spent = HashSet::new();
        for tx in transactions {
            if is_coinbase(tx) {
                continue;
            }
            confirmed.insert(calculate_tx_id(tx));
            for input in &tx.inputs {
                spent.insert(input.prevout);
            }
        }

        let mut remove_ids = confirmed.clone();
        for tx in self.get_transactions() {
            let id = calculate_tx_id(&tx);
            if tx.inputs.iter().any(|input| spent.contains(&input.prevout)) {
                remove_ids.insert(id);
            }
        }
        let conflicts: Vec<Hash> = remove_ids.difference(&confirmed).copied().collect();
        for id in conflicts {
            remove_ids.extend(self.replacement_package(&id));
        }
        if !record_failures && let Some(estimator) = self.fee_estimator() {
            for id in &remove_ids {
                estimator.forget_without_failure(id);
            }
        }
        for id in &remove_ids {
            self.remove_transaction(id);
        }
    }

    /// Remove transaction from mempool
    pub fn remove_transaction(&self, hash: &Hash) -> bool {
        let tx = {
            let mut pool = self.pool_lock();
            let Some(tx) = pool.transactions.remove(hash) else {
                return false;
            };
            pool.wtxids.remove(hash);
            pool.tx_witnesses.remove(hash);
            pool.adjusted_vsize.remove(hash);
            for input in &tx.inputs {
                pool.spent_outputs.remove(&input.prevout);
                pool.spenders.remove(&input.prevout);
            }
            tx
        };

        self.mempool
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        // Remove from fee index
        if let Some(fee_rate) = self
            .fee_cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash)
        {
            self.fee_basis
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(hash);
            let mut fee_index = self.fee_index.write().unwrap_or_else(|e| e.into_inner());
            if let Some(tx_hashes) = fee_index.get_mut(&Reverse(fee_rate)) {
                tx_hashes.retain(|&h| h != *hash);
                if tx_hashes.is_empty() {
                    fee_index.remove(&Reverse(fee_rate));
                }
            }
        }

        // Remove RBF tracking
        self.rbf_tracking
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        // Remove timestamp
        self.tx_timestamps
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);
        self.tx_heights
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        self.fee_deltas
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        // Remove from dependency graph
        {
            let mut dependencies = self
                .tx_dependencies
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let mut descendants = self
                .tx_descendants
                .write()
                .unwrap_or_else(|e| e.into_inner());

            if let Some(children) = descendants.remove(hash) {
                for child_hash in children {
                    if let Some(parents) = dependencies.get_mut(&child_hash) {
                        parents.remove(hash);
                    }
                }
            }

            if let Some(parents) = dependencies.remove(hash) {
                for parent_hash in parents {
                    if let Some(children) = descendants.get_mut(&parent_hash) {
                        children.remove(hash);
                    }
                }
            }
        }

        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let mempool_size = self.pool_lock().transactions.len();
            let hash_clone = *hash;
            let reason = "removed".to_string();
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_transaction_removed(&hash_clone, reason, mempool_size)
                    .await;
            });
        }

        self.invalidate_fee_ranking();
        if let Some(estimator) = self.fee_estimator() {
            estimator.on_tx_removed(hash);
        }
        true
    }

    /// Clear mempool
    pub fn clear(&self) {
        let (cleared_count,) = {
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let n = pool.transactions.len();
            pool.transactions.clear();
            pool.spent_outputs.clear();
            pool.spenders.clear();
            (n,)
        };
        self.mempool
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_index
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_basis
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_deltas
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.rbf_tracking
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.tx_timestamps
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.tx_heights
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.invalidate_fee_ranking();

        // Publish mempool cleared event
        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let event_pub_clone = Arc::clone(event_pub);
            let cleared_count_clone = cleared_count;
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_cleared(cleared_count_clone)
                    .await;
            });
        }
        if let Some(estimator) = self.fee_estimator() {
            estimator.on_pool_cleared();
        }
    }

    /// Save mempool to disk for persistence.
    ///
    /// The new body is written to a temporary file in the same directory and
    /// renamed over `path`. A failed write leaves the previous file unchanged.
    pub fn save_to_disk<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        use blvm_protocol::block::calculate_tx_id;
        use std::fs::File;
        use std::io::Write;

        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut tmp_name = path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("mempool.dat"));
        tmp_name.push(".tmp");
        let tmp_path = parent.join(tmp_name);

        let transactions = self.get_transactions();
        let write_result = (|| -> Result<()> {
            use crate::node::core_mempool_dat::{CoreMempoolDat, CoreMempoolTx};
            let txs = transactions
                .iter()
                .map(|tx| {
                    let hash = calculate_tx_id(tx);
                    CoreMempoolTx {
                        tx: tx.clone(),
                        witnesses: self.get_transaction_witnesses(&hash),
                        time: 0,
                        fee_delta: self.get_fee_delta(&hash),
                    }
                })
                .collect();
            let bytes = crate::node::core_mempool_dat::encode(&CoreMempoolDat {
                txs,
                ..CoreMempoolDat::default()
            })
            .map_err(|err| anyhow::anyhow!(err))?;
            let mut file = File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_result
    }

    /// Mempool byte cap from `max_mempool_mb`. An unset policy uses the default of 300 MiB.
    pub(crate) fn max_mempool_bytes(&self) -> u64 {
        self.policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
            .max_mempool_mb
            .saturating_mul(1_048_576)
    }

    /// Relay floor as BTC per kvB. 100 sat/kvB is 0.000001.
    pub(crate) fn min_relay_fee_btc_per_kvb(&self) -> f64 {
        self.min_relay_fee_sat_per_kvb() as f64 / 100_000_000.0
    }

    /// Configured relay floor in satoshis per 1,000 virtual bytes. An unset policy uses 100.
    pub(crate) fn min_relay_fee_sat_per_kvb(&self) -> u64 {
        self.policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
            .min_relay_fee_rate
    }

    /// Path from `[mempool] mempool_persistence_path` when that section is set.
    pub fn configured_persistence_path(&self) -> Option<String> {
        self.policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|policy| policy.mempool_persistence_path.clone())
    }
}

impl Default for MempoolManager {
    fn default() -> Self {
        Self::new()
    }
}

// MempoolManager is safe to share across threads: all interior state is
// protected by Mutex or RwLock, which derive Send+Sync automatically.
// The explicit impls below are not needed and have been removed.

impl crate::node::miner::MempoolProvider for MempoolManager {
    fn get_transactions(&self) -> Vec<blvm_protocol::Transaction> {
        self.get_transactions()
    }

    fn get_transaction(&self, hash: &[u8; 32]) -> Option<blvm_protocol::Transaction> {
        use blvm_protocol::Hash;
        let hash_array: Hash = *hash;
        self.get_transaction(&hash_array)
    }

    fn get_mempool_size(&self) -> usize {
        self.size()
    }

    fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Vec<blvm_protocol::Transaction> {
        self.get_prioritized_transactions(limit, utxo_set)
    }

    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool {
        use blvm_protocol::Hash;
        let hash_array: Hash = *hash;
        MempoolManager::remove_transaction(self, &hash_array)
    }

    fn get_transaction_witnesses(&self, hash: &[u8; 32]) -> Option<Vec<Witness>> {
        self.pool_lock().tx_witnesses.get(hash).cloned()
    }
}

impl MempoolManager {
    /// Load mempool from disk.
    ///
    /// Every transaction is decoded before any of them is inserted. A short
    /// read leaves the pool unchanged. An admission refusal rolls back the
    /// transactions this load added and returns the error.
    pub fn load_from_disk<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        let _loading = FlagHold::enter(&self.loading_from_disk);
        let bytes = std::fs::read(path)?;
        if bytes.len() >= 8 {
            let version = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            if version == 1 || version == 2 {
                let dump = crate::node::core_mempool_dat::decode(&bytes)
                    .map_err(|err| anyhow::anyhow!(err))?;
                return self.admit_core_dump(dump);
            }
        }
        self.load_legacy_bytes(&bytes)
    }

    fn admit_core_dump(&self, dump: crate::node::core_mempool_dat::CoreMempoolDat) -> Result<()> {
        use blvm_protocol::block::calculate_tx_id;
        let mut pending = dump.txs;
        let mut added = Vec::new();
        while !pending.is_empty() {
            let mut created_by_pending: HashMap<OutPoint, Hash> = HashMap::new();
            for entry in &pending {
                let hash = calculate_tx_id(&entry.tx);
                for index in 0..entry.tx.outputs.len() {
                    created_by_pending.insert(
                        OutPoint {
                            hash,
                            index: index as u32,
                        },
                        hash,
                    );
                }
            }
            let mut deferred = Vec::new();
            let mut progress = false;
            for entry in pending {
                let hash = calculate_tx_id(&entry.tx);
                let waits_for_dump_parent = entry.tx.inputs.iter().any(|input| {
                    created_by_pending
                        .get(&input.prevout)
                        .is_some_and(|parent| *parent != hash)
                });
                match self.add_transaction_with_witness(entry.tx.clone(), entry.witnesses.clone()) {
                    Ok(true) => {
                        if entry.fee_delta != 0 {
                            self.prioritise_transaction(&hash, entry.fee_delta);
                        }
                        added.push(hash);
                        progress = true;
                    }
                    Ok(false) if waits_for_dump_parent => deferred.push(entry),
                    Ok(false) => {
                        for hash in added.iter().rev() {
                            let _ = self.remove_transaction(hash);
                        }
                        return Err(anyhow::anyhow!(
                            "mempool.dat transaction was refused by policy"
                        ));
                    }
                    Err(err) => {
                        for hash in added.iter().rev() {
                            let _ = self.remove_transaction(hash);
                        }
                        return Err(err);
                    }
                }
            }
            if deferred.is_empty() {
                break;
            }
            if !progress {
                for hash in added.iter().rev() {
                    let _ = self.remove_transaction(hash);
                }
                return Err(anyhow::anyhow!("unknown_prevout"));
            }
            pending = deferred;
        }
        for (txid, delta) in dump.deltas {
            if delta != 0 {
                self.prioritise_transaction(&txid, delta);
            }
        }
        Ok(())
    }

    fn load_legacy_bytes(&self, data: &[u8]) -> Result<()> {
        use blvm_protocol::block::calculate_tx_id;
        if data.len() < 4 {
            return Err(anyhow::anyhow!("mempool file is shorter than a count"));
        }
        let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let remaining = data.len() - 4;
        if count > remaining / 4 {
            return Err(anyhow::anyhow!(
                "mempool file transaction length is truncated"
            ));
        }
        let mut offset = 4;
        let mut decoded = Vec::with_capacity(count);
        for _ in 0..count {
            if data.len() < offset + 4 {
                return Err(anyhow::anyhow!(
                    "mempool file transaction length is truncated"
                ));
            }
            let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            if data.len() < offset + len {
                return Err(anyhow::anyhow!("mempool file transaction is truncated"));
            }
            decoded.push(decode_persisted_tx(&data[offset..offset + len])?);
            offset += len;
        }

        let mut added = Vec::with_capacity(decoded.len());
        for (tx, witnesses) in decoded {
            let hash = calculate_tx_id(&tx);
            match self.add_transaction_with_witness(tx, witnesses) {
                Ok(true) => added.push(hash),
                Ok(false) => {
                    for hash in added.iter().rev() {
                        self.remove_transaction(hash);
                    }
                    anyhow::bail!("mempool load refused a transaction");
                }
                Err(error) => {
                    for hash in added.iter().rev() {
                        self.remove_transaction(hash);
                    }
                    return Err(error);
                }
            }
        }

        Ok(())
    }
}

fn encode_persisted_tx(tx: &Transaction, witnesses: Option<&[Witness]>) -> Result<Vec<u8>> {
    use blvm_protocol::serialization::transaction::{
        serialize_transaction, serialize_transaction_with_witness,
    };
    match witnesses {
        Some(wits) => serialize_transaction_with_witness(tx, wits)
            .map_err(|e| anyhow::anyhow!("mempool persist encode: {e}")),
        None => Ok(serialize_transaction(tx)),
    }
}

fn decode_persisted_tx(bytes: &[u8]) -> Result<(Transaction, Option<Vec<Witness>>)> {
    use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness;
    let (tx, witnesses, _) = deserialize_transaction_with_witness(bytes)
        .map_err(|e| anyhow::anyhow!("mempool persist decode: {e}"))?;
    let witnesses = if witnesses.iter().any(|stack| !stack.is_empty()) {
        Some(witnesses)
    } else {
        None
    };
    Ok((tx, witnesses))
}

#[cfg(test)]
#[path = "mempool_tests.rs"]
mod tests;

#[cfg(test)]
mod replacement_count_tests {
    use super::next_replacement_count;

    #[test]
    fn stored_count_follows_the_highest_direct_conflict() {
        assert_eq!(next_replacement_count([0, 4]), 5);
        assert_eq!(next_replacement_count([4, 0]), 5);
        assert_eq!(next_replacement_count([0]), 1);
    }
}
