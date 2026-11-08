//! Mempool block projection.
//!
//! Selection follows the ancestor-package score used by Bitcoin Core's
//! pre-cluster `BlockAssembler::addPackageTxs`: the highest package rate is
//! taken, its not-yet-included ancestors are ordered before it, and a package
//! that does not fit is skipped. Scores of in-pool descendants are recomputed
//! after each inclusion. The inputs are the fee, virtual size, and parents the
//! estimator stored at admission. This module does not read the UTXO set.

use blvm_protocol::Hash;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

/// One mempool transaction the projection can see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjTx {
    pub id: Hash,
    pub fee: u64,
    pub vsize: u64,
    pub parents: Vec<Hash>,
}

/// A transaction placed in a projected block, priced at the package rate that pulled it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedUnit {
    pub rate_sat_per_kvb: u64,
    pub weight: u64,
}

/// One projected block. `weight` is the sum of its unit weights.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedBlock {
    pub units: Vec<ProjectedUnit>,
    pub weight: u64,
}

/// Project up to `n` blocks.
///
/// A block's transactions weigh at most `max_weight - reserve_weight`.
/// Callers pass the miner's `TEMPLATE_RESERVE_WEIGHT` as `reserve_weight`.
pub fn project_blocks(
    txs: &[ProjTx],
    n: usize,
    max_weight: u64,
    reserve_weight: u64,
) -> Vec<ProjectedBlock> {
    project_placed(txs, n, max_weight, reserve_weight)
        .into_iter()
        .map(|units| {
            let weight = units
                .iter()
                .fold(0u64, |sum, unit| sum.saturating_add(unit.weight));
            ProjectedBlock {
                units: units
                    .into_iter()
                    .map(|unit| ProjectedUnit {
                        rate_sat_per_kvb: unit.rate_sat_per_kvb,
                        weight: unit.weight,
                    })
                    .collect(),
                weight,
            }
        })
        .collect()
}

/// Rate at `pct` percent of `block.weight`, walking from the highest rate.
///
/// A block lighter than `full_threshold * max_weight` returns `floor_sat_per_kvb`.
/// `full_threshold == 0.0` disables that rule. A lower percentile is a higher rate.
pub fn percentile_rate(
    block: &ProjectedBlock,
    pct: u8,
    max_weight: u64,
    full_threshold: f64,
    floor_sat_per_kvb: u64,
) -> u64 {
    let full_at = full_threshold * max_weight as f64;
    if (block.weight as f64) < full_at {
        return floor_sat_per_kvb;
    }
    if block.units.is_empty() || block.weight == 0 {
        return floor_sat_per_kvb;
    }
    let mut order: Vec<usize> = (0..block.units.len()).collect();
    order.sort_by(|&left, &right| {
        block.units[right]
            .rate_sat_per_kvb
            .cmp(&block.units[left].rate_sat_per_kvb)
            .then(left.cmp(&right))
    });
    let target = (block.weight as u128) * u128::from(pct) / 100;
    let mut cumulative = 0u128;
    for index in order {
        let unit = &block.units[index];
        cumulative = cumulative.saturating_add(u128::from(unit.weight));
        if cumulative >= target {
            return unit.rate_sat_per_kvb;
        }
    }
    floor_sat_per_kvb
}

struct Placed {
    id: Hash,
    rate_sat_per_kvb: u64,
    weight: u64,
}

struct Node {
    id: Hash,
    fee: u64,
    vsize: u64,
    weight: u64,
    parents: Vec<usize>,
    children: Vec<usize>,
    /// Parent-before-child closure, excluding self.
    ancestors: Vec<usize>,
    package_fee: u128,
    package_vsize: u128,
    generation: u64,
    selected: bool,
    /// Set once this node's own fee and vsize have been removed from descendants.
    removed_from_descendants: bool,
    failed: bool,
    cyclic: bool,
}

struct Graph {
    nodes: Vec<Node>,
}

impl Graph {
    fn build(txs: &[ProjTx]) -> Self {
        let mut index_of: HashMap<Hash, usize> = HashMap::new();
        let mut nodes = Vec::new();
        for tx in txs {
            if index_of.contains_key(&tx.id) {
                continue;
            }
            let index = nodes.len();
            index_of.insert(tx.id, index);
            nodes.push(Node {
                id: tx.id,
                fee: tx.fee,
                vsize: tx.vsize,
                weight: tx.vsize.saturating_mul(4),
                parents: Vec::new(),
                children: Vec::new(),
                ancestors: Vec::new(),
                package_fee: 0,
                package_vsize: 0,
                generation: 0,
                selected: false,
                removed_from_descendants: false,
                failed: false,
                cyclic: false,
            });
        }
        let mut attached = HashSet::new();
        for tx in txs {
            let Some(&child) = index_of.get(&tx.id) else {
                continue;
            };
            if !attached.insert(child) {
                continue;
            }
            let mut seen = HashSet::new();
            for parent_id in &tx.parents {
                let Some(&parent) = index_of.get(parent_id) else {
                    continue;
                };
                if parent == child {
                    nodes[child].cyclic = true;
                    continue;
                }
                if seen.insert(parent) {
                    nodes[child].parents.push(parent);
                }
            }
        }
        let count = nodes.len();
        for child in 0..count {
            let parents = nodes[child].parents.clone();
            for parent in parents {
                nodes[parent].children.push(child);
            }
        }
        let mut graph = Self { nodes };
        graph.close_ancestors();
        graph
    }

    fn close_ancestors(&mut self) {
        let count = self.nodes.len();
        let mut done = vec![false; count];
        let mut visiting = vec![false; count];
        for start in 0..count {
            self.walk_ancestors(start, &mut done, &mut visiting);
        }
        let fees: Vec<u64> = self.nodes.iter().map(|node| node.fee).collect();
        let vsizes: Vec<u64> = self.nodes.iter().map(|node| node.vsize).collect();
        for index in 0..self.nodes.len() {
            let ancestors = self.nodes[index].ancestors.clone();
            if ancestors.contains(&index)
                || self.nodes[index].cyclic
                || self.nodes[index].vsize == 0
            {
                self.nodes[index].cyclic = true;
                self.nodes[index].failed = true;
                continue;
            }
            let mut package_fee = u128::from(fees[index]);
            let mut package_vsize = u128::from(vsizes[index]);
            for ancestor in ancestors {
                package_fee = package_fee.saturating_add(u128::from(fees[ancestor]));
                package_vsize = package_vsize.saturating_add(u128::from(vsizes[ancestor]));
            }
            self.nodes[index].package_fee = package_fee;
            self.nodes[index].package_vsize = package_vsize;
        }
    }

    fn walk_ancestors(&mut self, index: usize, done: &mut [bool], visiting: &mut [bool]) {
        if done[index] {
            return;
        }
        if visiting[index] {
            self.nodes[index].cyclic = true;
            return;
        }
        visiting[index] = true;
        let parents = self.nodes[index].parents.clone();
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();
        for parent in parents {
            self.walk_ancestors(parent, done, visiting);
            if self.nodes[parent].cyclic {
                self.nodes[index].cyclic = true;
            }
            for &older in &self.nodes[parent].ancestors {
                if seen.insert(older) {
                    ordered.push(older);
                }
            }
            if seen.insert(parent) {
                ordered.push(parent);
            }
        }
        self.nodes[index].ancestors = ordered;
        visiting[index] = false;
        done[index] = true;
    }

    fn fill_block(&mut self, capacity: u64) -> Vec<Placed> {
        for node in &mut self.nodes {
            if !node.selected {
                node.failed = node.cyclic || node.vsize == 0;
            }
        }
        let mut heap = BinaryHeap::new();
        for index in 0..self.nodes.len() {
            if let Some(entry) = self.heap_entry(index) {
                heap.push(entry);
            }
        }
        let mut placed = Vec::new();
        let mut remaining = capacity;
        while let Some((rate, Reverse(index), generation)) = heap.pop() {
            let node = &self.nodes[index];
            if node.selected || node.failed || node.generation != generation {
                continue;
            }
            if self.score(index) != Some(rate) {
                continue;
            }
            let package = self.package_order(index);
            let mut weight = 0u64;
            let mut fits = true;
            for &member in &package {
                match weight.checked_add(self.nodes[member].weight) {
                    Some(sum) if sum <= remaining => weight = sum,
                    _ => {
                        fits = false;
                        break;
                    }
                }
            }
            if !fits || weight == 0 {
                self.nodes[index].failed = true;
                continue;
            }
            let rate_sat_per_kvb = u64::try_from(rate).unwrap_or(u64::MAX);
            for member in package {
                let node = &mut self.nodes[member];
                node.selected = true;
                placed.push(Placed {
                    id: node.id,
                    rate_sat_per_kvb,
                    weight: node.weight,
                });
            }
            remaining -= weight;
            self.drop_selected_from_descendants(&mut heap);
        }
        placed
    }

    fn package_order(&self, index: usize) -> Vec<usize> {
        let mut order = Vec::new();
        for &ancestor in &self.nodes[index].ancestors {
            if !self.nodes[ancestor].selected {
                order.push(ancestor);
            }
        }
        order.push(index);
        order
    }

    fn drop_selected_from_descendants(
        &mut self,
        heap: &mut BinaryHeap<(u128, Reverse<usize>, u64)>,
    ) {
        let fresh: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.selected && !node.removed_from_descendants)
            .map(|(index, _)| index)
            .collect();
        for ancestor in fresh {
            let fee = u128::from(self.nodes[ancestor].fee);
            let vsize = u128::from(self.nodes[ancestor].vsize);
            let children = self.nodes[ancestor].children.clone();
            self.nodes[ancestor].removed_from_descendants = true;
            let mut queue: VecDeque<usize> = children.into();
            let mut seen = HashSet::new();
            while let Some(desc) = queue.pop_front() {
                if !seen.insert(desc) {
                    continue;
                }
                let grandchildren = self.nodes[desc].children.clone();
                queue.extend(grandchildren);
                if self.nodes[desc].selected || self.nodes[desc].cyclic {
                    continue;
                }
                self.nodes[desc].package_fee = self.nodes[desc].package_fee.saturating_sub(fee);
                self.nodes[desc].package_vsize =
                    self.nodes[desc].package_vsize.saturating_sub(vsize);
                self.nodes[desc].generation = self.nodes[desc].generation.saturating_add(1);
                self.nodes[desc].failed = false;
                if let Some(entry) = self.heap_entry(desc) {
                    heap.push(entry);
                }
            }
        }
    }

    fn heap_entry(&self, index: usize) -> Option<(u128, Reverse<usize>, u64)> {
        let rate = self.score(index)?;
        Some((rate, Reverse(index), self.nodes[index].generation))
    }

    fn score(&self, index: usize) -> Option<u128> {
        let node = &self.nodes[index];
        if node.selected || node.failed || node.cyclic || node.package_vsize == 0 {
            return None;
        }
        Some(node.package_fee.saturating_mul(1000) / node.package_vsize)
    }
}

fn project_placed(
    txs: &[ProjTx],
    n: usize,
    max_weight: u64,
    reserve_weight: u64,
) -> Vec<Vec<Placed>> {
    if n == 0 {
        return Vec::new();
    }
    let capacity = max_weight.saturating_sub(reserve_weight);
    let mut graph = Graph::build(txs);
    let mut blocks = Vec::new();
    while blocks.len() < n {
        let placed = graph.fill_block(capacity);
        if placed.is_empty() {
            break;
        }
        blocks.push(placed);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::miner::TEMPLATE_RESERVE_WEIGHT;
    use proptest::prelude::*;

    fn tx(id: u8, fee: u64, vsize: u64, parents: &[u8]) -> ProjTx {
        ProjTx {
            id: [id; 32],
            fee,
            vsize,
            parents: parents.iter().map(|parent| [*parent; 32]).collect(),
        }
    }

    fn included_ids(txs: &[ProjTx], n: usize, max_weight: u64, reserve_weight: u64) -> Vec<Hash> {
        project_placed(txs, n, max_weight, reserve_weight)
            .into_iter()
            .flatten()
            .map(|unit| unit.id)
            .collect()
    }

    #[test]
    fn zero_fee_parent_is_priced_at_the_child_package_rate() {
        let parent = tx(1, 0, 100, &[]);
        let child = tx(2, 5_000, 100, &[1]);
        let blocks = project_blocks(&[parent, child], 1, 4_000_000, TEMPLATE_RESERVE_WEIGHT);
        assert_eq!(blocks.len(), 1);
        let rate = 5_000u64 * 1000 / 200;
        assert_eq!(blocks[0].units.len(), 2);
        assert!(
            blocks[0]
                .units
                .iter()
                .all(|unit| unit.rate_sat_per_kvb == rate)
        );
        assert_eq!(blocks[0].weight, 800);
        let ids = included_ids(
            &[tx(1, 0, 100, &[]), tx(2, 5_000, 100, &[1])],
            1,
            4_000_000,
            TEMPLATE_RESERVE_WEIGHT,
        );
        assert_eq!(ids, vec![[1; 32], [2; 32]]);
    }

    #[test]
    fn a_package_that_does_not_fit_is_skipped_for_a_smaller_transaction() {
        let wide = tx(1, 10_000, 1_000, &[]);
        let narrow = tx(2, 100, 100, &[]);
        let blocks = project_blocks(&[wide.clone(), narrow.clone()], 1, 500, 0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].weight, 400);
        assert_eq!(blocks[0].units[0].rate_sat_per_kvb, 1_000);
        assert_eq!(included_ids(&[wide, narrow], 1, 500, 0), vec![[2; 32]]);
    }

    #[test]
    fn capacity_never_exceeds_max_minus_reserve_and_a_parent_can_lead_the_next_block() {
        let parent = tx(1, 0, 200, &[]);
        let child = tx(2, 9_000, 200, &[1]);
        let blocks = project_blocks(&[parent.clone(), child.clone()], 2, 1_000, 200);
        let capacity = 800;
        assert!(blocks.iter().all(|block| block.weight <= capacity));
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|block| block.weight == 800));
        assert_eq!(
            included_ids(&[parent, child], 2, 1_000, 200),
            vec![[1; 32], [2; 32]]
        );
    }

    #[test]
    fn percentile_is_non_increasing_and_the_short_block_rule_has_a_zero_switch() {
        let txs = vec![tx(1, 5_000, 100, &[]), tx(2, 1_000, 100, &[])];
        let blocks = project_blocks(&txs, 1, 800, 0);
        let block = &blocks[0];
        assert_eq!(block.weight, 800);
        let mut previous = u64::MAX;
        for pct in 1..=99 {
            let rate = percentile_rate(block, pct, 4_000_000, 0.0, 100);
            assert!(rate <= previous, "pct {pct} rate {rate} after {previous}");
            previous = rate;
        }
        assert_eq!(percentile_rate(block, 50, 4_000_000, 0.95, 100), 100);
        assert_ne!(percentile_rate(block, 50, 4_000_000, 0.0, 100), 100);
    }

    #[test]
    fn a_missing_parent_does_not_change_the_child_rate() {
        let child = tx(2, 2_000, 100, &[9]);
        let blocks = project_blocks(&[child], 1, 4_000_000, 0);
        assert_eq!(blocks[0].units[0].rate_sat_per_kvb, 20_000);
    }

    proptest! {
        #[test]
        fn random_dags_keep_the_projection_properties(
            raw in prop::collection::vec((0u64..20_000, 1u64..300, any::<u32>()), 0..24),
            max_weight in 1u64..40_000,
            reserve_weight in 0u64..40_000,
        ) {
            let mut txs = Vec::new();
            for (index, (fee, vsize, mask)) in raw.into_iter().enumerate() {
                let mut parents = Vec::new();
                if index > 0 {
                    parents.push((mask as usize % index) as u8);
                    if index > 1 && mask % 2 == 0 {
                        let other = (mask as usize / 5) % index;
                        if other != parents[0] as usize {
                            parents.push(other as u8);
                        }
                    }
                }
                txs.push(tx(index as u8, fee, vsize, &parents));
            }
            let blocks = project_blocks(&txs, 4, max_weight, reserve_weight);
            let capacity = max_weight.saturating_sub(reserve_weight);
            for block in &blocks {
                prop_assert!(block.weight <= capacity);
                prop_assert_eq!(
                    block.weight,
                    block.units.iter().map(|unit| unit.weight).sum::<u64>()
                );
                let mut previous = u64::MAX;
                for pct in [1u8, 25, 50, 75, 99] {
                    let rate = percentile_rate(block, pct, max_weight, 0.0, 1);
                    prop_assert!(rate <= previous);
                    previous = rate;
                }
                if (block.weight as f64) < 0.95 * max_weight as f64 {
                    prop_assert_eq!(percentile_rate(block, 50, max_weight, 0.95, 7), 7);
                }
            }
            let ids = included_ids(&txs, 4, max_weight, reserve_weight);
            let mut seen = HashSet::new();
            for id in &ids {
                prop_assert!(seen.insert(*id));
            }
            let pos: HashMap<Hash, usize> = ids
                .iter()
                .copied()
                .enumerate()
                .map(|(index, id)| (id, index))
                .collect();
            for tx in &txs {
                let Some(&child_at) = pos.get(&tx.id) else {
                    continue;
                };
                for parent in &tx.parents {
                    if let Some(&parent_at) = pos.get(parent) {
                        prop_assert!(parent_at < child_at);
                    }
                }
            }
        }
    }

    fn first_block_ids(txs: &[ProjTx], max_weight: u64, reserve_weight: u64) -> Vec<Hash> {
        project_placed(txs, 1, max_weight, reserve_weight)
            .into_iter()
            .next()
            .unwrap_or_default()
            .into_iter()
            .map(|unit| unit.id)
            .collect()
    }

    /// Same set, or a one-transaction difference that is the last unit on each
    /// side after the block has filled.
    fn same_set_or_final_unit(projected: &[Hash], mined: &[Hash], pool_len: usize) -> bool {
        let projected_set: HashSet<Hash> = projected.iter().copied().collect();
        let mined_set: HashSet<Hash> = mined.iter().copied().collect();
        let only_projected: Vec<Hash> = projected_set.difference(&mined_set).copied().collect();
        let only_mined: Vec<Hash> = mined_set.difference(&projected_set).copied().collect();
        if only_projected.is_empty() && only_mined.is_empty() {
            return true;
        }
        let filled = projected.len() < pool_len || mined.len() < pool_len;
        filled
            && only_projected.len() <= 1
            && only_mined.len() <= 1
            && only_projected.iter().all(|id| projected.last() == Some(id))
            && only_mined.iter().all(|id| mined.last() == Some(id))
    }

    fn admit_independent(fees: &[u64]) -> (crate::node::mempool::MempoolManager, Vec<ProjTx>) {
        use crate::node::fee_estimator::FeeEstimator;
        use crate::node::mempool::MempoolManager;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO};

        let mempool = MempoolManager::new();
        let mut fees_cfg = crate::config::FeeEstimatorConfig::default();
        fees_cfg.health_required = Some(false);
        let estimator = std::sync::Arc::new(FeeEstimator::new(fees_cfg, true));
        mempool.set_fee_estimator(Some(std::sync::Arc::clone(&estimator)));
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.require_standard = false;
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 0;
        mempool.set_policy_config(Some(policy));

        let mut utxos = blvm_protocol::UtxoSet::default();
        let mut txs = Vec::with_capacity(fees.len());
        for (index, fee) in fees.iter().copied().enumerate() {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(index as u64).to_le_bytes());
            let prevout = OutPoint { hash, index: 0 };
            utxos.insert(
                prevout,
                std::sync::Arc::new(UTXO {
                    value: 50_000 + fee as i64,
                    script_pubkey: vec![OP_1].into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            txs.push(Transaction {
                version: 1,
                inputs: vec![TransactionInput {
                    prevout,
                    script_sig: vec![OP_1],
                    sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
                }]
                .into(),
                outputs: vec![TransactionOutput {
                    value: 50_000,
                    script_pubkey: vec![OP_1].into(),
                }]
                .into(),
                lock_time: 0,
            });
        }
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxos)));
        for (index, tx) in txs.into_iter().enumerate() {
            assert!(
                mempool.add_transaction(tx).unwrap(),
                "independent transaction {index} was rejected"
            );
        }
        (mempool, estimator.tracked_for_projection())
    }

    fn miner_ids(
        mempool: &crate::node::mempool::MempoolManager,
        utxos: &blvm_protocol::UtxoSet,
        max_weight: u64,
    ) -> Vec<Hash> {
        use crate::node::miner::{MempoolProvider, TransactionSelector};
        use blvm_protocol::block::calculate_tx_id;

        let selector = TransactionSelector::with_params(10_000_000, max_weight, 0);
        selector
            .select_transactions(mempool, utxos)
            .iter()
            .map(calculate_tx_id)
            .collect()
    }

    fn pool_utxos(fees: &[u64]) -> blvm_protocol::UtxoSet {
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, UTXO};

        let mut utxos = blvm_protocol::UtxoSet::default();
        for (index, fee) in fees.iter().copied().enumerate() {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(index as u64).to_le_bytes());
            utxos.insert(
                OutPoint { hash, index: 0 },
                std::sync::Arc::new(UTXO {
                    value: 50_000 + fee as i64,
                    script_pubkey: vec![OP_1].into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
        }
        utxos
    }

    #[test]
    fn an_independent_pool_matches_the_miner_except_the_final_unit() {
        let fees: Vec<u64> = (1..24).map(|fee| fee * 1_000).collect();
        let max_weight = 7_000;
        let (mempool, tracked) = admit_independent(&fees);
        assert!(tracked.iter().all(|tx| tx.parents.is_empty()));
        let projected = first_block_ids(&tracked, max_weight, TEMPLATE_RESERVE_WEIGHT);
        let mined = miner_ids(&mempool, &pool_utxos(&fees), max_weight);
        assert!(
            same_set_or_final_unit(&projected, &mined, tracked.len()),
            "projected {projected:?}\nmined {mined:?}"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn random_independent_pools_match_the_miner_except_the_final_unit(
            raw_fees in prop::collection::vec(1u64..5_000, 0..28),
        ) {
            let fees: Vec<u64> = raw_fees
                .iter()
                .enumerate()
                .map(|(index, fee)| fee + (index as u64 + 1) * 10_000)
                .collect();
            let max_weight = 7_000u64;
            let (mempool, tracked) = admit_independent(&fees);
            prop_assert!(tracked.iter().all(|tx| tx.parents.is_empty()));
            prop_assert_eq!(tracked.len(), fees.len());
            let projected = first_block_ids(&tracked, max_weight, TEMPLATE_RESERVE_WEIGHT);
            let mined = miner_ids(&mempool, &pool_utxos(&fees), max_weight);
            prop_assert!(
                same_set_or_final_unit(&projected, &mined, tracked.len()),
                "projected {projected:?}\nmined {mined:?}"
            );
        }
    }
}
