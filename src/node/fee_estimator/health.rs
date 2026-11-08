//! Local-mempool health ring.
//!
//! The mempool layer is trusted only when recent blocks show the local pool
//! matches what miners included. `CBlockPolicyEstimator::processBlock` in
//! Bitcoin Core ignores `nBlockHeight <= nBestSeenHeight` and does not compare
//! tracked weight to block weight, so this ring uses the aggregate check from
//! the fee-estimator design: `sum(found) * 100 >= min_ratio_pct * sum(total)`
//! over the whole window. `health_min_ratio` 0.75 is 75 percent.

use std::collections::VecDeque;

/// Recent connected blocks, oldest first.
pub struct HealthRing {
    window: usize,
    entries: VecDeque<(u64, u64, u64)>,
}

impl HealthRing {
    pub fn new(window: usize) -> Self {
        Self {
            window,
            entries: VecDeque::new(),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record one connected block.
    ///
    /// `total` is the block weight. `found` is the weight of that block's
    /// transactions that were already in the local pool. A height at or below
    /// the last recorded height is ignored. A height gap clears the ring, then
    /// the new block starts a fresh window.
    pub fn observe(&mut self, height: u64, total: u64, found: u64) {
        if let Some(&(last_height, _, _)) = self.entries.back() {
            if height <= last_height {
                return;
            }
            if height > last_height.saturating_add(1) {
                self.entries.clear();
            }
        }
        self.entries.push_back((height, total, found));
        while self.entries.len() > self.window {
            self.entries.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn entries(&self) -> Vec<(u64, u64, u64)> {
        self.entries.iter().copied().collect()
    }

    /// Replace the ring with a saved window. Entries beyond `window` are dropped
    /// from the oldest end.
    pub(crate) fn restore(&mut self, saved: &[(u64, u64, u64)]) {
        self.entries.clear();
        let skip = saved.len().saturating_sub(self.window);
        self.entries.extend(saved.iter().skip(skip).copied());
    }

    /// `found / total` over the blocks currently in the ring. Empty is 0.
    pub fn aggregate_ratio(&self) -> f64 {
        let mut found = 0u128;
        let mut total = 0u128;
        for &(_, block_total, block_found) in &self.entries {
            found = found.saturating_add(u128::from(block_found));
            total = total.saturating_add(u128::from(block_total));
        }
        if total == 0 {
            0.0
        } else {
            (found as f64) / (total as f64)
        }
    }

    /// True when the window is full and the aggregate found weight meets `min_ratio`.
    pub fn is_healthy(&self, min_ratio: f64) -> bool {
        if self.window == 0 || self.entries.len() != self.window {
            return false;
        }
        let mut found = 0u128;
        let mut total = 0u128;
        for &(_, block_total, block_found) in &self.entries {
            found = found.saturating_add(u128::from(block_found));
            total = total.saturating_add(u128::from(block_total));
        }
        ratio_met(found, total, min_ratio)
    }
}

/// `Some` uses the configured bit. `None` is required off regtest and on everywhere else.
pub fn health_required(configured: Option<bool>, network_is_regtest: bool) -> bool {
    match configured {
        Some(required) => required,
        None => !network_is_regtest,
    }
}

/// The mempool layer may lower an estimate when the gate is not required, or when the ring is healthy.
pub fn mempool_layer_trusted(ring: &HealthRing, min_ratio: f64, required: bool) -> bool {
    !required || ring.is_healthy(min_ratio)
}

/// `min_ratio` 0.75 means 75 percent: `found * 100 >= 75 * total`.
fn ratio_met(found: u128, total: u128, min_ratio: f64) -> bool {
    if !min_ratio.is_finite() || !(0.0..=1.0).contains(&min_ratio) {
        return false;
    }
    let pct = (min_ratio * 100.0).round();
    if !(0.0..=100.0).contains(&pct) {
        return false;
    }
    found.saturating_mul(100) >= (pct as u128).saturating_mul(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(ring: &mut HealthRing, start: u64, count: u64, total: u64, found: u64) {
        for step in 0..count {
            ring.observe(start + step, total, found);
        }
    }

    #[test]
    fn not_healthy_until_the_window_is_full() {
        let mut ring = HealthRing::new(3);
        fill(&mut ring, 1, 2, 100, 100);
        assert!(!ring.is_healthy(0.75));
        ring.observe(3, 100, 100);
        assert!(ring.is_healthy(0.75));
        assert!(mempool_layer_trusted(&ring, 0.75, true));
    }

    #[test]
    fn flips_at_exactly_the_configured_ratio() {
        let mut exact = HealthRing::new(1);
        exact.observe(1, 100, 75);
        assert!(exact.is_healthy(0.75));

        let mut below = HealthRing::new(1);
        below.observe(1, 100, 74);
        assert!(!below.is_healthy(0.75));

        let mut aggregate = HealthRing::new(2);
        aggregate.observe(1, 100, 100);
        aggregate.observe(2, 100, 50);
        assert!(aggregate.is_healthy(0.75));
        aggregate.observe(3, 100, 49);
        assert_eq!(aggregate.len(), 2);
        assert!(!aggregate.is_healthy(0.75));
    }

    #[test]
    fn a_height_gap_clears_the_ring_and_a_repeat_does_not() {
        let mut ring = HealthRing::new(2);
        fill(&mut ring, 10, 2, 100, 100);
        assert!(ring.is_healthy(0.75));
        ring.observe(11, 1, 1);
        assert!(ring.is_healthy(0.75));
        ring.observe(13, 100, 100);
        assert_eq!(ring.len(), 1);
        assert!(!ring.is_healthy(0.75));
        ring.observe(14, 100, 100);
        assert!(ring.is_healthy(0.75));
    }

    #[test]
    fn an_unrequired_gate_passes_on_an_empty_ring() {
        let ring = HealthRing::new(6);
        assert!(!health_required(None, true));
        assert!(health_required(None, false));
        assert!(health_required(Some(true), true));
        assert!(!health_required(Some(false), false));
        assert!(mempool_layer_trusted(
            &ring,
            0.75,
            health_required(None, true)
        ));
        assert!(!mempool_layer_trusted(
            &ring,
            0.75,
            health_required(None, false)
        ));
        assert!(mempool_layer_trusted(
            &ring,
            0.75,
            health_required(Some(false), false)
        ));
    }
}
