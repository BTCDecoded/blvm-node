//! Append-only hash chain for `Durable { audit: true }` timers.
//!
//! `hash = SHA256(prev_hash || canonical_bytes)`. Canonical bytes are the
//! bincode of the record with the hash field removed. A writer who can replace
//! the whole tree can recompute the chain. Export the latest hash outside the
//! datadir if that matters.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::TimerId;
use crate::storage::database::Tree;

pub const MODULE_TIMER_JOURNAL: &str = "module_timer_journal";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalEvent {
    Created,
    Updated,
    Fired(u64),
    Acked(u64),
    Failed(u64),
    Cancelled,
    Expired,
    Paused,
    Resumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub index: u64,
    pub unix_ms: u64,
    pub module: String,
    pub timer_id: TimerId,
    pub key: Option<String>,
    pub event: JournalEvent,
    pub detail: String,
    pub prev_hash: [u8; 32],
    pub hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalBreak {
    pub index: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalBody {
    index: u64,
    unix_ms: u64,
    module: String,
    timer_id: TimerId,
    key: Option<String>,
    event: JournalEvent,
    detail: String,
    prev_hash: [u8; 32],
}

pub fn append(
    tree: &dyn Tree,
    prev_hash: [u8; 32],
    index: u64,
    unix_ms: u64,
    module: &str,
    timer_id: TimerId,
    key: Option<String>,
    event: JournalEvent,
    detail: String,
) -> Result<JournalRecord, String> {
    let body = JournalBody {
        index,
        unix_ms,
        module: module.to_string(),
        timer_id,
        key,
        event,
        detail,
        prev_hash,
    };
    let canonical = bincode::serialize(&body).map_err(|err| err.to_string())?;
    let hash = chain_hash(&prev_hash, &canonical);
    let record = JournalRecord {
        index: body.index,
        unix_ms: body.unix_ms,
        module: body.module,
        timer_id: body.timer_id,
        key: body.key,
        event: body.event,
        detail: body.detail,
        prev_hash,
        hash,
    };
    let bytes = bincode::serialize(&record).map_err(|err| err.to_string())?;
    tree.insert(&index.to_be_bytes(), &bytes)
        .map_err(|err| err.to_string())?;
    Ok(record)
}

pub fn tip(tree: &dyn Tree) -> ([u8; 32], u64) {
    let mut prev = [0u8; 32];
    let mut next = 0u64;
    let Ok(rows) = rows(tree) else {
        return (prev, next);
    };
    for record in rows {
        prev = record.hash;
        next = record.index.saturating_add(1);
    }
    (prev, next)
}

pub fn read(
    tree: &dyn Tree,
    from: u64,
    to: u64,
    module: Option<&str>,
) -> Result<Vec<JournalRecord>, String> {
    let mut out = Vec::new();
    for record in rows(tree)? {
        if record.index < from || record.index > to {
            continue;
        }
        if module.is_some_and(|name| record.module != name) {
            continue;
        }
        out.push(record);
    }
    Ok(out)
}

pub fn verify(tree: &dyn Tree) -> Result<(), JournalBreak> {
    let mut expected_prev = [0u8; 32];
    let mut expected_index = 0u64;
    for record in rows(tree).map_err(|reason| JournalBreak { index: 0, reason })? {
        if record.index != expected_index {
            return Err(JournalBreak {
                index: record.index,
                reason: format!("expected index {expected_index}"),
            });
        }
        if record.prev_hash != expected_prev {
            return Err(JournalBreak {
                index: record.index,
                reason: "prev_hash does not match the previous record".into(),
            });
        }
        let body = JournalBody {
            index: record.index,
            unix_ms: record.unix_ms,
            module: record.module.clone(),
            timer_id: record.timer_id,
            key: record.key.clone(),
            event: record.event.clone(),
            detail: record.detail.clone(),
            prev_hash: record.prev_hash,
        };
        let canonical = bincode::serialize(&body).map_err(|err| JournalBreak {
            index: record.index,
            reason: err.to_string(),
        })?;
        let hash = chain_hash(&record.prev_hash, &canonical);
        if hash != record.hash {
            return Err(JournalBreak {
                index: record.index,
                reason: "hash does not match the record".into(),
            });
        }
        expected_prev = record.hash;
        expected_index = expected_index.saturating_add(1);
    }
    Ok(())
}

fn chain_hash(prev_hash: &[u8; 32], canonical: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash);
    hasher.update(canonical);
    hasher.finalize().into()
}

fn rows(tree: &dyn Tree) -> Result<Vec<JournalRecord>, String> {
    let mut records = Vec::new();
    for row in tree.iter() {
        let (key, value) = row.map_err(|err| err.to_string())?;
        let record: JournalRecord = bincode::deserialize(&value)
            .map_err(|err| format!("corrupt record at {}: {err}", key_index(&key).unwrap_or(0)))?;
        records.push(record);
    }
    records.sort_by_key(|record| record.index);
    Ok(records)
}

fn key_index(key: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = key.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::database::{BatchWriter, Tree};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    struct Mem {
        map: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
    }

    impl Mem {
        fn new() -> Self {
            Self {
                map: Mutex::new(BTreeMap::new()),
            }
        }
    }

    struct MemBatch {
        map: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
        ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    }

    impl BatchWriter for MemBatch {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.ops.push((key.to_vec(), Some(value.to_vec())));
        }

        fn delete(&mut self, key: &[u8]) {
            self.ops.push((key.to_vec(), None));
        }

        fn commit(self: Box<Self>) -> anyhow::Result<()> {
            let mut map = self.map.lock().unwrap_or_else(|err| err.into_inner());
            for (key, value) in self.ops {
                match value {
                    Some(value) => {
                        map.insert(key, value);
                    }
                    None => {
                        map.remove(&key);
                    }
                }
            }
            Ok(())
        }

        fn len(&self) -> usize {
            self.ops.len()
        }
    }

    impl Tree for Mem {
        fn insert(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
            self.map
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .insert(key.to_vec(), value.to_vec());
            Ok(())
        }

        fn get(&self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self
                .map
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .get(key)
                .cloned())
        }

        fn remove(&self, key: &[u8]) -> anyhow::Result<()> {
            self.map
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .remove(key);
            Ok(())
        }

        fn contains_key(&self, key: &[u8]) -> anyhow::Result<bool> {
            Ok(self
                .map
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .contains_key(key))
        }

        fn clear(&self) -> anyhow::Result<()> {
            self.map
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .clear();
            Ok(())
        }

        fn len(&self) -> anyhow::Result<usize> {
            Ok(self.map.lock().unwrap_or_else(|err| err.into_inner()).len())
        }

        fn iter(&self) -> Box<dyn Iterator<Item = anyhow::Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let pairs: Vec<_> = self
                .map
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .iter()
                .map(|(key, value)| Ok((key.clone(), value.clone())))
                .collect();
            Box::new(pairs.into_iter())
        }

        fn batch(&self) -> anyhow::Result<Box<dyn BatchWriter + '_>> {
            Ok(Box::new(MemBatch {
                map: Arc::new(Mutex::new(BTreeMap::new())),
                ops: Vec::new(),
            }))
        }
    }

    #[test]
    fn a_modified_record_fails_verification() {
        let tree = Mem::new();
        let id = TimerId([7; 16]);
        append(
            &tree,
            [0; 32],
            0,
            1_700_000_000_000,
            "fees",
            id,
            Some("housekeeping".into()),
            JournalEvent::Created,
            "created".into(),
        )
        .unwrap();
        verify(&tree).unwrap();

        let key = 0u64.to_be_bytes();
        let raw = tree.get(&key).unwrap().unwrap();
        let mut record: JournalRecord = bincode::deserialize(&raw).unwrap();
        record.detail = "replaced".into();
        tree.insert(&key, &bincode::serialize(&record).unwrap())
            .unwrap();

        let broken = verify(&tree).unwrap_err();
        assert_eq!(broken.index, 0);
        assert!(
            broken.reason.contains("hash"),
            "expected a hash mismatch, got {}",
            broken.reason
        );
    }
}
