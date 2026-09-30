//! R-359: in-order MuHash folder for the engine validation pipeline.
//!
//! Workers used to compute the per-block MuHash contribution **before** sending the
//! `ValidateResult`, so the ~2 µs/element fold (≈ 7.5 ms at 340–370k element counts) sat on
//! the head-of-line path the orchestrator blocks on in `collect_wait`. Now the worker sends
//! the result first and ships `(height, sub, compute_us)` on a side channel; the orchestrator
//! folds subs into the running accumulator **strictly in height order** so the accumulator
//! always equals "exactly blocks ≤ folded_through()", which is what the periodic
//! `persist_ibd_utxo_muhash_running_only` and the shutdown checkpoint require.
//!
//! MuHash is commutative, so order does not change the final value; the in-order rule only
//! exists so a persisted running state never includes a block above the persisted height.

use std::collections::BTreeMap;

use blvm_muhash::MuHash3072;

/// One block's MuHash contribution, computed on a validation worker after the result was sent.
pub(crate) struct MuHashSub {
    pub height: u64,
    pub sub: MuHash3072,
    /// Worker wall time spent building `sub` (µs). Reported as `eng_muhash_sum` in MS_BREAKDOWN.
    pub compute_us: u64,
}

/// Holds out-of-order subs and folds them into an accumulator in height order.
pub(crate) struct InOrderMuHashFolder {
    next: u64,
    pending: BTreeMap<u64, (MuHash3072, u64)>,
}

impl InOrderMuHashFolder {
    /// `first_height` is the first height that will be folded (the validation start height).
    pub(crate) fn new(first_height: u64) -> Self {
        Self {
            next: first_height,
            pending: BTreeMap::new(),
        }
    }

    /// Buffer a sub. Heights below `next` (already folded) are dropped.
    pub(crate) fn push(&mut self, s: MuHashSub) {
        if s.height < self.next {
            return;
        }
        self.pending.insert(s.height, (s.sub, s.compute_us));
    }

    /// Fold every contiguous sub starting at `next` into `acc`. Returns the summed worker
    /// compute time (µs) of the subs folded by this call.
    pub(crate) fn fold_ready(&mut self, acc: &mut MuHash3072) -> u64 {
        let mut us = 0u64;
        while let Some((sub, compute_us)) = self.pending.remove(&self.next) {
            acc.multiply_mut(&sub);
            us = us.saturating_add(compute_us);
            self.next += 1;
        }
        us
    }

    /// Highest height whose sub has been folded (`first_height - 1` when nothing has).
    pub(crate) fn folded_through(&self) -> u64 {
        self.next.saturating_sub(1)
    }

    /// Number of buffered subs waiting on a lower height.
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub_for(tag: u8) -> MuHash3072 {
        let mut m = MuHash3072::new();
        m.insert_mut(&[tag; 40]);
        m
    }

    #[test]
    fn r359_folder_folds_in_height_order_and_matches_serial_product() {
        let mut folder = InOrderMuHashFolder::new(10);
        let mut acc = MuHash3072::new();

        // Out of order: 12 and 11 arrive before 10 — nothing may fold yet.
        folder.push(MuHashSub { height: 12, sub: sub_for(12), compute_us: 300 });
        folder.push(MuHashSub { height: 11, sub: sub_for(11), compute_us: 200 });
        assert_eq!(folder.fold_ready(&mut acc), 0);
        assert_eq!(folder.folded_through(), 9);
        assert_eq!(folder.pending_len(), 2);

        // 10 arrives: 10, 11, 12 fold in one call and report the summed compute time.
        folder.push(MuHashSub { height: 10, sub: sub_for(10), compute_us: 100 });
        assert_eq!(folder.fold_ready(&mut acc), 600);
        assert_eq!(folder.folded_through(), 12);
        assert_eq!(folder.pending_len(), 0);

        // Same value as the serial in-order fold the drain used to do.
        let mut serial = MuHash3072::new();
        for h in [10u8, 11, 12] {
            serial = serial.multiply(&sub_for(h));
        }
        assert_eq!(acc.clone().finalize(), serial.finalize());

        // A stale height (already folded) is ignored, a gap holds the fold.
        folder.push(MuHashSub { height: 11, sub: sub_for(11), compute_us: 1 });
        folder.push(MuHashSub { height: 14, sub: sub_for(14), compute_us: 1 });
        assert_eq!(folder.fold_ready(&mut acc), 0);
        assert_eq!(folder.folded_through(), 12);
        assert_eq!(folder.pending_len(), 1);
        folder.push(MuHashSub { height: 13, sub: sub_for(13), compute_us: 1 });
        assert_eq!(folder.fold_ready(&mut acc), 2);
        assert_eq!(folder.folded_through(), 14);
    }
}
