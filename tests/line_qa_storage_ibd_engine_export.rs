//! Test-line QA: checkpoint sync returns the tree's disk flush error.

use anyhow::Result;
use blvm_node::storage::database::{BatchWriter, Tree};
use blvm_node::storage::ibd_engine::export::sync_tree_after_persist;

struct FlushFails;

impl Tree for FlushFails {
    fn insert(&self, _: &[u8], _: &[u8]) -> Result<()> {
        Ok(())
    }
    fn get(&self, _: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn remove(&self, _: &[u8]) -> Result<()> {
        Ok(())
    }
    fn contains_key(&self, _: &[u8]) -> Result<bool> {
        Ok(false)
    }
    fn clear(&self) -> Result<()> {
        Ok(())
    }
    fn len(&self) -> Result<usize> {
        Ok(0)
    }
    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
        Box::new(std::iter::empty())
    }
    fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
        struct EmptyBatch;
        impl BatchWriter for EmptyBatch {
            fn put(&mut self, _: &[u8], _: &[u8]) {}
            fn delete(&mut self, _: &[u8]) {}
            fn commit(self: Box<Self>) -> Result<()> {
                Ok(())
            }
            fn len(&self) -> usize {
                0
            }
        }
        Ok(Box::new(EmptyBatch))
    }
    fn flush_to_disk(&self) -> Result<()> {
        Err(anyhow::anyhow!("disk flush refused"))
    }
}

#[test]
fn sync_tree_after_persist_returns_the_disk_error() {
    match sync_tree_after_persist(&FlushFails) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("engine ckpt sync failed") && message.contains("disk flush refused"),
                "sync_tree_after_persist returned {message}"
            );
        }
        Ok(()) => panic!("checkpoint sync accepted a disk that refuses to flush"),
    }
}
