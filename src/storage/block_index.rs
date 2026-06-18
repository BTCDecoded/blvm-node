//! Per-block chain tree metadata (parent links, tip enumeration).
//!
//! Complements `blockstore` height→hash (one per height) with a hash-keyed index so
//! competing blocks at the same height and orphan branches remain visible to RPC.

use crate::storage::database::Tree;
use anyhow::Result;
use blvm_protocol::Hash;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

/// Validation / inclusion status stored in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockIndexStatus {
    Valid,
    Invalid,
}

/// Metadata for one block hash in the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockIndexEntry {
    pub height: u64,
    pub prev_hash: Hash,
    pub status: BlockIndexStatus,
    /// Monotonic connect order (lower = seen first). Used for equal-work tie-break.
    #[serde(default)]
    pub sequence_id: u64,
}

/// Reserved tree key for the next sequence counter (not a block hash).
const SEQUENCE_COUNTER_KEY: &[u8] = &[0xff; 32];

/// Hash-keyed block tree index (persisted).
pub struct BlockIndex {
    entries: Arc<dyn Tree>,
}

impl BlockIndex {
    pub fn new(db: Arc<dyn crate::storage::database::Database>) -> Result<Self> {
        let entries = Arc::from(db.open_tree("block_index")?);
        Ok(Self { entries })
    }

    pub fn reset(&self) -> Result<()> {
        self.entries.clear()?;
        Ok(())
    }

    /// Record or refresh a block in the index.
    pub fn insert(
        &self,
        hash: &Hash,
        height: u64,
        prev_hash: &Hash,
        status: BlockIndexStatus,
    ) -> Result<()> {
        let sequence_id = if let Some(existing) = self.get(hash)? {
            existing.sequence_id
        } else {
            self.next_sequence_id()?
        };
        let entry = BlockIndexEntry {
            height,
            prev_hash: *prev_hash,
            status,
            sequence_id,
        };
        let data = bincode::serialize(&entry)?;
        self.entries.insert(hash.as_slice(), &data)?;
        Ok(())
    }

    fn next_sequence_id(&self) -> Result<u64> {
        let current = match self.entries.get(SEQUENCE_COUNTER_KEY)? {
            Some(bytes) if bytes.len() >= 8 => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes[..8]);
                u64::from_be_bytes(arr)
            }
            _ => 0,
        };
        let next = current.saturating_add(1);
        self.entries
            .insert(SEQUENCE_COUNTER_KEY, &next.to_be_bytes())?;
        Ok(next)
    }

    pub fn get(&self, hash: &Hash) -> Result<Option<BlockIndexEntry>> {
        match self.entries.get(hash.as_slice())? {
            Some(data) => Ok(Some(bincode::deserialize(&data)?)),
            None => Ok(None),
        }
    }

    pub fn mark_invalid(&self, hash: &Hash) -> Result<()> {
        if let Some(mut entry) = self.get(hash)? {
            entry.status = BlockIndexStatus::Invalid;
            let data = bincode::serialize(&entry)?;
            self.entries.insert(hash.as_slice(), &data)?;
        }
        Ok(())
    }

    /// All indexed blocks that are chain tips (no other indexed block lists them as parent).
    pub fn chain_tips(&self) -> Result<Vec<(Hash, BlockIndexEntry)>> {
        let mut all: Vec<(Hash, BlockIndexEntry)> = Vec::new();
        let mut referenced_as_parent: HashSet<Hash> = HashSet::new();

        for result in self.entries.iter() {
            let (key, data) = result?;
            if key.as_slice() == SEQUENCE_COUNTER_KEY || key.len() != 32 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&key);
            let entry: BlockIndexEntry = bincode::deserialize(&data)?;
            referenced_as_parent.insert(entry.prev_hash);
            all.push((hash, entry));
        }

        let tips: Vec<_> = all
            .into_iter()
            .filter(|(hash, _)| !referenced_as_parent.contains(hash))
            .collect();
        Ok(tips)
    }

    /// Blocks on the branch from `tip` down to (but not including) the active chain.
    pub fn branch_length(&self, tip: &Hash, active_tip: &Hash) -> Result<u64> {
        if tip == active_tip {
            return Ok(0);
        }
        let active_chain = self.ancestors_of(active_tip)?;
        let mut len = 0u64;
        let mut current = *tip;
        let mut visited = HashSet::new();
        while !active_chain.contains(&current) {
            if !visited.insert(current) {
                break;
            }
            let Some(entry) = self.get(&current)? else {
                break;
            };
            len += 1;
            current = entry.prev_hash;
            if len > 10_000 {
                break;
            }
        }
        Ok(len)
    }

    fn ancestors_of(&self, tip: &Hash) -> Result<HashSet<Hash>> {
        let mut set = HashSet::new();
        let mut current = *tip;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                break;
            }
            set.insert(current);
            let Some(entry) = self.get(&current)? else {
                break;
            };
            current = entry.prev_hash;
            if set.len() > 10_000 {
                break;
            }
        }
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::database::{create_database, default_backend};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn open_index() -> (TempDir, BlockIndex) {
        let dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(dir.path(), default_backend(), None).unwrap());
        let index = BlockIndex::new(db).unwrap();
        (dir, index)
    }

    #[test]
    fn two_branches_yield_two_tips() {
        let (_dir, index) = open_index();
        let genesis = [0u8; 32];
        let main_a = [1u8; 32];
        let fork_b = [2u8; 32];

        index
            .insert(&genesis, 0, &[0u8; 32], BlockIndexStatus::Valid)
            .unwrap();
        index
            .insert(&main_a, 1, &genesis, BlockIndexStatus::Valid)
            .unwrap();
        index
            .insert(&fork_b, 1, &genesis, BlockIndexStatus::Valid)
            .unwrap();

        let mut tips = index.chain_tips().unwrap();
        tips.sort_by_key(|(_, e)| e.height);
        assert_eq!(tips.len(), 2);
        assert!(tips.iter().any(|(h, _)| *h == main_a));
        assert!(tips.iter().any(|(h, _)| *h == fork_b));
        assert_eq!(index.branch_length(&main_a, &main_a).unwrap(), 0);
        assert_eq!(index.branch_length(&fork_b, &main_a).unwrap(), 1);
    }

    #[test]
    fn sequence_id_monotonic_and_stable_on_refresh() {
        let (_dir, index) = open_index();
        let genesis = [0u8; 32];
        let main_a = [1u8; 32];
        let fork_b = [2u8; 32];

        index
            .insert(&genesis, 0, &[0u8; 32], BlockIndexStatus::Valid)
            .unwrap();
        index
            .insert(&main_a, 1, &genesis, BlockIndexStatus::Valid)
            .unwrap();
        index
            .insert(&fork_b, 1, &genesis, BlockIndexStatus::Valid)
            .unwrap();

        let seq_a = index.get(&main_a).unwrap().unwrap().sequence_id;
        let seq_b = index.get(&fork_b).unwrap().unwrap().sequence_id;
        assert!(seq_a < seq_b, "later connect gets higher sequence_id");

        index
            .insert(&main_a, 1, &genesis, BlockIndexStatus::Valid)
            .unwrap();
        assert_eq!(
            index.get(&main_a).unwrap().unwrap().sequence_id,
            seq_a,
            "refresh must not bump sequence_id"
        );
    }
}
