//! H-1: Staged UTXO deltas for **View(h)** — fold validated-not-yet-retired deltas in height order
//! on top of the in-memory IBD store snapshot, without writing canonical state until a single
//! **retire** pass applies them in order. The validation loop can advance `View(h+1)` while
//! D(h) is only staged, once those entries are present in `staged_deltas`.

use blvm_protocol::block::UtxoDelta;
use blvm_protocol::types::UtxoSet;
use blvm_protocol::utxo_overlay::utxo_deletion_key_to_outpoint;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Return when a height in the required fold range has no entry in the staged map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingStagedDelta {
    pub height: u64,
}

/// No-op overlay (when `connect_block_ibd` returns no delta; retire still advances the watermark).
pub fn empty_utxo_delta() -> UtxoDelta {
    UtxoDelta {
        additions: Default::default(),
        deletions: Default::default(),
    }
}

/// Apply one overlay delta to an in-memory `UtxoSet` (consensus outpoint space).
/// Same UTXO semantics as [`crate::storage::ibd_utxo_store::IbdUtxoStore::apply_utxo_delta`]
/// on the outpoint key space, but for the `UtxoSet` type used in `connect_block_ibd`.
pub fn apply_utxo_delta_to_utxo_set(set: &mut UtxoSet, delta: &UtxoDelta) {
    for dk in &delta.deletions {
        let op = utxo_deletion_key_to_outpoint(dk);
        set.remove(&op);
    }
    for (op, arc) in &delta.additions {
        set.insert(*op, Arc::clone(arc));
    }
}

/// Fold all staged block deltas with heights in `(last_retired_height, connect_height)` into `set`
/// in ascending height order. The set must already match `IbdUtxoStore` as-of
/// `last_retired_height` (state after the block at that height).
///
/// `connect_height` is the block being **connected**; the view must reflect state *after* block
/// `connect_height - 1`, so we apply `D(k)` for `k = last_retired_height + 1 .. connect_height`.
pub fn fold_staged_deltas_into_utxo_set(
    set: &mut UtxoSet,
    staged: &BTreeMap<u64, UtxoDelta>,
    last_retired_height: u64,
    connect_height: u64,
) -> Result<(), MissingStagedDelta> {
    for h in (last_retired_height + 1)..connect_height {
        let Some(d) = staged.get(&h) else {
            return Err(MissingStagedDelta { height: h });
        };
        apply_utxo_delta_to_utxo_set(set, d);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_no_staged_empty_range_ok() {
        let mut s = UtxoSet::default();
        let staged: BTreeMap<u64, UtxoDelta> = BTreeMap::new();
        // last=4, connect=5 → (5..5) empty
        fold_staged_deltas_into_utxo_set(&mut s, &staged, 4, 5).unwrap();
    }

    #[test]
    fn fold_errors_when_delta_missing() {
        let mut s = UtxoSet::default();
        let staged: BTreeMap<u64, UtxoDelta> = BTreeMap::new();
        // need D(5) for connect_height 6 but map empty
        let e = fold_staged_deltas_into_utxo_set(&mut s, &staged, 4, 6).unwrap_err();
        assert_eq!(e.height, 5);
    }
}
