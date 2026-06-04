//! Core types for the IBD UTXO engine.
//!
//! Key design choices vs the existing disk_utxo layer:
//! - `OutputKey = [u8; 36]` (txid 32B + vout u32 BE 4B) vs legacy `OutPointKey = [u8; 40]`
//!   (txid 32B + vout u64 LE 8B). 36B fits tighter in cache and sorts lexicographically.
//! - `OutputHeader` is `repr(C)` fixed-layout — memcpy safe, no bincode round-trip.
//! - `OutputKV` is 52 bytes: fits ~78 per 4096-byte cache line group; height and table id
//!   are separate fields rather than bit-packed into the key.
//! - `IdCodec` packs `{offset: 44 bits, length: 20 bits}` into a single `u64` table id.

/// 36-byte UTXO key: txid (32 bytes) || vout (u32 big-endian, 4 bytes).
///
/// Big-endian vout means the key is lexicographically sortable and maps to `OutPoint` ordering.
/// Converted from the existing `OutPointKey = [u8; 40]` (u64 LE vout) via `to_output_key`.
pub type OutputKey = [u8; 36];

/// Packed table offset + length: `offset << 20 | length`.
/// - Offset: upper 44 bits → max 16 TB flat file.
/// - Length: lower 20 bits → max 1 MB per UTXO (sufficient for any script).
pub type OutputId = u64;

/// Sentinel: UTXO found in the memory age hierarchy but already spent (Delete entry visible).
///
/// Distinguishes "spent in memory" from `OutputId::MAX` ("not found anywhere yet").
/// When set, disk-index lookup is skipped — the memory Delete has already resolved the key.
/// After all lookups, callers treat this identically to `OutputId::MAX` (UTXO unavailable).
pub const OUTPUT_ID_DELETED: OutputId = OutputId::MAX - 1;

/// Fixed-layout UTXO metadata in the flat table file.
///
/// Written as raw bytes (`memcpy`). No bincode overhead. 16 bytes, cache-aligned.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputHeader {
    /// Block height where this UTXO was created. i32 matches Bitcoin's block height range.
    pub height: i32,
    /// Packed flags. Bit 0: `is_coinbase`.
    pub flags: u32,
    /// Satoshi value.
    pub amount: i64,
}

impl OutputHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn is_coinbase(self) -> bool {
        self.flags & 1 != 0
    }
}

/// One entry in a `MemoryRun`. Sorted by `(key, height desc, op)`.
///
/// 52 bytes. `Add` entries carry a non-zero `id`; `Delete` entries carry `id = 0`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputKV {
    /// 36-byte UTXO key.
    pub key: OutputKey,
    /// Block height this entry was written at (signed to allow sentinel use of -1).
    pub height: i32,
    /// Operation: `Add = 0`, `Delete = -1`.
    pub op: i8,
    pub _pad: [u8; 3],
    /// Table location for `Add` entries (`IdCodec::encode`). Zero for `Delete`.
    pub id: OutputId,
}

// OutputKV is 56 bytes in repr(C): key(36) + height(4) + op(1) + _pad(3) + id(8, needs align 8 → 4B gap after _pad).
// Struct alignment = 8 (from u64 id), total size rounded up to 56. Fits ~73 per 4 KB cache block.
const _: () = assert!(std::mem::size_of::<OutputKV>() == 56);

impl OutputKV {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn new_add(key: OutputKey, height: i32, id: OutputId) -> Self {
        Self {
            key,
            height,
            op: 0,
            _pad: [0; 3],
            id,
        }
    }

    pub fn new_delete(key: OutputKey, height: i32) -> Self {
        Self {
            key,
            height,
            op: -1,
            _pad: [0; 3],
            id: 0,
        }
    }

    pub fn is_add(self) -> bool {
        self.op == 0
    }

    pub fn is_delete(self) -> bool {
        self.op == -1
    }
}

/// Ordering: key asc → height desc (newest first) → Add before Delete for same (key, height).
///
/// `op` is reversed (`other.op.cmp(&self.op)`) so Add (op=0) < Delete (op=-1) in sorted order.
/// This guarantees that in a k-way merge, an Add entry for (key, height) is emitted before
/// its paired Delete, enabling the cancellation check in `MemoryRun::merge`.
impl Ord for OutputKV {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| other.height.cmp(&self.height))
            .then_with(|| other.op.cmp(&self.op)) // reversed: Add(0) > Delete(-1) → Add sorts first
    }
}

impl PartialOrd for OutputKV {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Decoded UTXO detail from the flat table. Consumed by `session_to_utxo_set`.
#[derive(Debug, Clone)]
pub struct OutputDetail {
    pub header: OutputHeader,
    /// Raw script bytes. Consumed to build `SharedByteString` for consensus.
    pub script: Vec<u8>,
}

/// Encodes/decodes `{offset, length}` pairs into a single `OutputId` (`u64`).
pub struct IdCodec;

impl IdCodec {
    pub const LEN_BITS: u32 = 20;
    const LEN_MASK: u64 = (1 << Self::LEN_BITS) - 1;

    pub fn encode(offset: u64, length: usize) -> OutputId {
        debug_assert!(
            length < (1 << Self::LEN_BITS),
            "script too long for IdCodec"
        );
        (offset << Self::LEN_BITS) | (length as u64 & Self::LEN_MASK)
    }

    pub fn decode(id: OutputId) -> (u64, usize) {
        let offset = id >> Self::LEN_BITS;
        let length = (id & Self::LEN_MASK) as usize;
        (offset, length)
    }
}

// ─── Key conversion helpers ─────────────────────────────────────────────────

/// Convert from the legacy `OutPointKey = [u8; 40]` (txid 32B + vout u64 LE 8B)
/// to `OutputKey = [u8; 36]` (txid 32B + vout u32 BE 4B).
///
/// Called once per input key per block during `SpendSession::resolve`.
#[inline]
pub fn to_output_key(k: &[u8; 40]) -> OutputKey {
    let mut out = [0u8; 36];
    out[..32].copy_from_slice(&k[..32]);
    let vout = u64::from_le_bytes(k[32..40].try_into().unwrap()) as u32;
    out[32..36].copy_from_slice(&vout.to_be_bytes());
    out
}

/// Convert from `OutputKey = [u8; 36]` back to `blvm_protocol::types::OutPoint`.
#[inline]
pub fn output_key_to_outpoint(k: &OutputKey) -> blvm_protocol::types::OutPoint {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&k[..32]);
    let vout = u32::from_be_bytes(k[32..36].try_into().unwrap());
    blvm_protocol::types::OutPoint { hash, index: vout }
}

/// Convert from `blvm_protocol::types::OutPoint` to `OutputKey`.
#[inline]
pub fn outpoint_to_output_key(op: &blvm_protocol::types::OutPoint) -> OutputKey {
    let mut out = [0u8; 36];
    out[..32].copy_from_slice(&op.hash);
    out[32..36].copy_from_slice(&op.index.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id_codec_roundtrip() {
        let offset = 0x123456789u64;
        let length = 0x5678usize;
        let id = IdCodec::encode(offset, length);
        let (o2, l2) = IdCodec::decode(id);
        assert_eq!(o2, offset);
        assert_eq!(l2, length);
    }

    #[test]
    fn test_outpoint_key_roundtrip() {
        let mut k40 = [0u8; 40];
        k40[0..32].fill(0xab);
        let vout: u64 = 7;
        k40[32..40].copy_from_slice(&vout.to_le_bytes());
        let k36 = to_output_key(&k40);
        let op = output_key_to_outpoint(&k36);
        assert_eq!(&op.hash, &k40[..32]);
        assert_eq!(op.index, 7u32);
    }

    #[test]
    fn test_output_kv_ordering() {
        let key_a = [0u8; 36];
        let mut key_b = [0u8; 36];
        key_b[35] = 1;
        let a_h10 = OutputKV::new_add(key_a, 10, 1);
        let a_h5 = OutputKV::new_add(key_a, 5, 2);
        let d_h5 = OutputKV::new_delete(key_a, 5); // same height as a_h5
        let d_h3 = OutputKV::new_delete(key_a, 3);
        let b_h1 = OutputKV::new_add(key_b, 1, 3);
        // Same key: height desc (h=10 before h=5 before h=3)
        assert!(a_h10 < a_h5, "newer Add should sort first");
        assert!(a_h5 < d_h3, "h=5 before h=3");
        // Same key, same height: Add before Delete (for cancellation)
        assert!(a_h5 < d_h5, "Add should sort before Delete at same height");
        // Different key: key_a < key_b
        assert!(d_h3 < b_h1);
    }

    #[test]
    fn test_output_header_flags() {
        let h = OutputHeader {
            height: 100,
            flags: 1,
            amount: 5000,
        };
        assert!(h.is_coinbase());
        let h2 = OutputHeader {
            height: 100,
            flags: 0,
            amount: 5000,
        };
        assert!(!h2.is_coinbase());
    }
}
