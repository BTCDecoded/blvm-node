//! Resume choice when the UTXO watermark is ahead of the durable block tip.

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
    fn a_watermark_past_the_tip_resumes_from_the_checkpoint_at_or_below_it() {
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 330_797, true), (1, 330_796, true)]),
            Some((1, 330_796))
        );
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 330_797, true), (1, 200_000, true)]),
            Some((1, 200_000))
        );
        assert_eq!(
            checkpoint_at_or_below_tip(330_796, [(0, 300_000, true), (1, 200_000, true)]),
            Some((0, 300_000))
        );
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
