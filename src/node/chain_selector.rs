//! Fork-choice helpers: compare cumulative chainwork at tips.

use crate::storage::Storage;
use anyhow::Result;
use blvm_protocol::Hash;

use blvm_consensus::pow::U256;

/// Return cumulative chainwork for a block hash (zero when unknown).
pub fn chainwork_at(storage: &Storage, hash: &Hash) -> Result<U256> {
    Ok(storage
        .chain()
        .get_chainwork(hash)?
        .unwrap_or_else(U256::zero))
}

/// True when `candidate_tip` has strictly more cumulative chainwork than the active tip.
pub fn should_activate_over_active_tip(storage: &Storage, candidate_tip: &Hash) -> Result<bool> {
    let (active_tip, _) = storage.chain().get_tip_hash_and_height()?;
    if *candidate_tip == active_tip {
        return Ok(false);
    }
    let active_work = chainwork_at(storage, &active_tip)?;
    let candidate_work = chainwork_at(storage, candidate_tip)?;
    Ok(candidate_work > active_work)
}

/// True when `candidate_tip` has strictly more cumulative chainwork than the active tip.
pub fn heavier_than_active_tip(storage: &Storage, candidate_tip: &Hash) -> Result<bool> {
    let (active_tip, _) = storage.chain().get_tip_hash_and_height()?;
    if *candidate_tip == active_tip {
        return Ok(false);
    }
    let active_work = chainwork_at(storage, &active_tip)?;
    let candidate_work = chainwork_at(storage, candidate_tip)?;
    Ok(candidate_work > active_work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::storage::block_index::BlockIndexStatus;
    use blvm_protocol::BlockHeader;
    use tempfile::TempDir;

    use blvm_consensus::pow::U256;

    fn header() -> BlockHeader {
        BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 0,
            bits: 0x207fffff,
            nonce: 0,
        }
    }

    fn storage_with_sibling_tips(first: Hash, second: Hash, tip: Hash, work: U256) -> Storage {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage.chain().initialize(&header()).unwrap();

        let parent = [9u8; 32];
        storage
            .chain()
            .block_index()
            .insert(&parent, 0, &[0u8; 32], BlockIndexStatus::Valid)
            .unwrap();
        storage
            .chain()
            .block_index()
            .insert(&first, 1, &parent, BlockIndexStatus::Valid)
            .unwrap();
        storage
            .chain()
            .block_index()
            .insert(&second, 1, &parent, BlockIndexStatus::Valid)
            .unwrap();

        let tip_header = header();
        storage
            .chain()
            .cache_block_chainwork(&tip_header.prev_block_hash, &tip_header, 0)
            .unwrap();
        storage.chain().update_tip(&tip, &tip_header, 1).unwrap();
        storage.chain().store_chainwork(&first, work).unwrap();
        storage.chain().store_chainwork(&second, work).unwrap();
        storage
    }

    #[test]
    fn equal_work_prefers_lower_sequence() {
        let active = [1u8; 32];
        let candidate = [2u8; 32];
        let storage = storage_with_sibling_tips(active, candidate, active, U256::from_u128(100));
        assert!(!should_activate_over_active_tip(&storage, &candidate).unwrap());
    }

    #[test]
    fn equal_work_keeps_the_active_tip() {
        let active = [2u8; 32];
        let candidate = [1u8; 32];
        let storage = storage_with_sibling_tips(candidate, active, active, U256::from_u128(100));
        assert!(!should_activate_over_active_tip(&storage, &candidate).unwrap());
    }

    #[test]
    fn heavier_work_always_activates() {
        let active = [1u8; 32];
        let candidate = [2u8; 32];
        let storage = storage_with_sibling_tips(active, candidate, active, U256::from_u128(100));
        storage
            .chain()
            .store_chainwork(&candidate, U256::from_u128(200))
            .unwrap();
        assert!(should_activate_over_active_tip(&storage, &candidate).unwrap());
    }

    #[test]
    fn a_sequence_read_error_is_not_max_and_the_tip_stays() {
        let active = [1u8; 32];
        let candidate = [2u8; 32];
        let storage = storage_with_sibling_tips(active, candidate, active, U256::from_u128(100));
        storage
            .chain()
            .block_index()
            .insert_undecodable(&active)
            .unwrap();
        assert!(storage.chain().block_index().get(&active).is_err());
        assert!(!should_activate_over_active_tip(&storage, &candidate).unwrap());
        assert_eq!(storage.chain().get_tip_hash().unwrap().unwrap(), active);
    }
}
