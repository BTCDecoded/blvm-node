//! Storage decisions for the reliability pass. `Err` is not a missing key.

/// `Err` stays `Err`. A missing watermark is 0.
pub(crate) fn watermark_for_repair<E>(read: Result<Option<u64>, E>) -> Result<u64, E> {
    Ok(read?.unwrap_or(0))
}

/// A count read that fails is not under the cap.
pub(crate) fn count_within_cap<E>(count: Result<usize, E>, cap: usize) -> Result<bool, E> {
    Ok(count? < cap.saturating_mul(8) / 10)
}

/// Clear a checkpoint tree only when `is_empty` is `Ok(true)`.
pub(crate) fn should_clear_checkpoint<E>(is_empty: Result<bool, E>) -> Result<bool, E> {
    is_empty
}

/// Store the checkpoint only when it is at or under the block tip and past the current export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointComplete {
    Store,
    Refuse,
}

pub(crate) fn checkpoint_complete_decision(
    height: u64,
    block_tip: u64,
    current: u64,
) -> CheckpointComplete {
    if height > block_tip || height <= current {
        CheckpointComplete::Refuse
    } else {
        CheckpointComplete::Store
    }
}

/// Both keys are written only after the tree sync succeeds and the height is at or under the tip.
pub(crate) fn canonical_export_writes_both(sync_ok: bool, height: u64, block_tip: u64) -> bool {
    sync_ok && height <= block_tip
}

/// Newest checkpoint that does not include a block the block store never made durable.
///
/// `slots` is `(slot, height, has_rows)`. A label above `chain_tip` means that snapshot
/// applied a block whose body is not stored. Resume from the greatest height that is
/// still at or below the tip and still has rows. `None` means every labeled snapshot
/// is past the tip or empty — there is no checkpoint to resume from.
pub(crate) fn checkpoint_at_or_below_tip(
    chain_tip: u64,
    slots: [(u8, u64, bool); 2],
) -> Option<(u8, u64)> {
    let mut best: Option<(u8, u64)> = None;
    for (slot, height, has_rows) in slots {
        if !has_rows || height == 0 || height > chain_tip {
            continue;
        }
        if best.map(|(_, chosen)| height > chosen).unwrap_or(true) {
            best = Some((slot, height));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_watermark_read_does_not_delete_the_engine() {
        assert!(watermark_for_repair::<&str>(Err("read")).is_err());
        assert_eq!(watermark_for_repair(Ok::<Option<u64>, &str>(None)).unwrap(), 0);
        assert_eq!(watermark_for_repair(Ok::<Option<u64>, &str>(Some(0))).unwrap(), 0);
        assert_eq!(watermark_for_repair(Ok::<Option<u64>, &str>(Some(4))).unwrap(), 4);
    }

    #[test]
    fn a_failed_storage_count_is_not_within_bounds() {
        assert!(count_within_cap::<&str>(Err("len"), 100).is_err());
        assert_eq!(count_within_cap(Ok::<usize, &str>(0usize), 100).unwrap(), true);
        assert_eq!(count_within_cap(Ok::<usize, &str>(79usize), 100).unwrap(), true);
        assert_eq!(count_within_cap(Ok::<usize, &str>(80usize), 100).unwrap(), false);
    }

    #[test]
    fn a_refused_checkpoint_is_not_persisted() {
        assert_eq!(
            checkpoint_complete_decision(10, 20, 5),
            CheckpointComplete::Store
        );
        assert_eq!(
            checkpoint_complete_decision(21, 20, 5),
            CheckpointComplete::Refuse
        );
        assert_eq!(
            checkpoint_complete_decision(5, 20, 5),
            CheckpointComplete::Refuse
        );
    }

    #[test]
    fn a_nonempty_checkpoint_is_not_cleared_before_export() {
        assert_eq!(should_clear_checkpoint(Ok::<bool, &str>(true)).unwrap(), true);
        assert_eq!(should_clear_checkpoint(Ok::<bool, &str>(false)).unwrap(), false);
        assert!(should_clear_checkpoint::<&str>(Err("empty")).is_err());
    }

    #[test]
    fn a_full_watermark_records_the_tree_and_the_height_together() {
        assert!(canonical_export_writes_both(true, 10, 10));
        assert!(!canonical_export_writes_both(false, 10, 10));
        assert!(!canonical_export_writes_both(true, 11, 10));
    }

    #[test]
    fn a_watermark_past_the_tip_resumes_from_the_checkpoint_at_or_below_it() {
        // Crash left the active snapshot one block past the durable tip.
        // The previous slot is still a complete set at the tip. Resume there.
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 330_797, true), (1, 330_796, true)]),
            Some((1, 330_796))
        );
        // An older labeled slot is the resume when nothing sits on the tip.
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 330_797, true), (1, 200_000, true)]),
            Some((1, 200_000))
        );
        // The higher of two usable slots wins.
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 300_000, true), (1, 200_000, true)]),
            Some((0, 300_000))
        );
        // A label above the tip is not a resume point, even with rows.
        // An empty slot and a zero height are not either.
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 330_797, true), (1, 330_796, false)]),
            None
        );
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 0, true), (1, 0, true)]),
            None
        );
    }
}
