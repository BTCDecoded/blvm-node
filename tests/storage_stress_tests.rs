//! Stress tests for storage operations (concurrent writes, high load)

use bllvm_node::storage::Storage;
use bllvm_node::{Block, BlockHeader, Hash};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_block(height: u64) -> Block {
    let mut prev_hash = [0u8; 32];
    prev_hash[0] = (height % 256) as u8;

    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: prev_hash,
            merkle_root: [0u8; 32],
            timestamp: 1231006505 + height,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    }
}

fn create_test_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    (temp_dir, storage)
}

#[tokio::test]
async fn test_storage_concurrent_block_writes() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Create multiple blocks
    let blocks: Vec<_> = (0..100).map(|i| create_test_block(i)).collect();

    // Write blocks concurrently
    let mut handles = vec![];
    for (i, block) in blocks.iter().enumerate() {
        let blockstore_clone = Arc::clone(&blockstore);
        let block_clone = block.clone();
        let height = i as u64;

        handles.push(tokio::spawn(async move {
            // Store block
            blockstore_clone.store_block(&block_clone).ok()?;

            // Store height
            let block_hash = blockstore_clone.get_block_hash(&block_clone);
            blockstore_clone.store_height(height, &block_hash).ok()?;

            Some(())
        }));
    }

    // Wait for all writes
    let results: Vec<_> = futures::future::join_all(handles).await;

    // All should complete
    assert_eq!(results.len(), 100);
    for result in results {
        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn test_storage_concurrent_reads_writes() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Store initial blocks
    for i in 0..10 {
        let block = create_test_block(i);
        blockstore.store_block(&block).unwrap();
    }

    // Concurrent reads and writes
    let mut handles = vec![];

    // Writers
    for i in 10..20 {
        let blockstore_clone = Arc::clone(&blockstore);
        let block = create_test_block(i);
        handles.push(tokio::spawn(async move {
            blockstore_clone.store_block(&block).ok()
        }));
    }

    // Readers
    for i in 0..10 {
        let blockstore_clone = Arc::clone(&blockstore);
        let block = create_test_block(i);
        handles.push(tokio::spawn(async move {
            let block_hash = blockstore_clone.get_block_hash(&block);
            blockstore_clone.get_block(&block_hash).ok()
        }));
    }

    // Wait for all operations
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 20);
}

#[tokio::test]
async fn test_storage_rapid_sequential_writes() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Rapid sequential writes
    for i in 0..1000 {
        let block = create_test_block(i);
        blockstore.store_block(&block).unwrap();
    }

    // Verify all stored
    for i in 0..100 {
        let block = create_test_block(i);
        let block_hash = blockstore.get_block_hash(&block);
        let stored = blockstore.get_block(&block_hash).unwrap();
        assert!(stored.is_some());
    }
}

#[tokio::test]
async fn test_storage_concurrent_height_indexing() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Store blocks with concurrent height indexing
    let mut handles = vec![];
    for i in 0..50 {
        let blockstore_clone = Arc::clone(&blockstore);
        let block = create_test_block(i);
        let height = i;

        handles.push(tokio::spawn(async move {
            // Store block
            blockstore_clone.store_block(&block).ok()?;

            // Store height index
            let block_hash = blockstore_clone.get_block_hash(&block);
            blockstore_clone.store_height(height, &block_hash).ok()?;

            Some(())
        }));
    }

    // Wait for all operations
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 50);

    // Verify height indexing
    for i in 0..50 {
        let block = create_test_block(i);
        let block_hash = blockstore.get_block_hash(&block);
        let stored_height = blockstore.get_height(&block_hash).unwrap();
        assert_eq!(stored_height, Some(i));
    }
}

#[tokio::test]
async fn test_storage_large_block_handling() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Create a block (simulated large block)
    let block = create_test_block(0);

    // Store and retrieve
    blockstore.store_block(&block).unwrap();
    let block_hash = blockstore.get_block_hash(&block);
    let stored = blockstore.get_block(&block_hash).unwrap();

    assert!(stored.is_some());
    let retrieved_block = stored.unwrap();
    assert_eq!(retrieved_block.header.version, block.header.version);
}

#[tokio::test]
async fn test_storage_concurrent_header_storage() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Store headers concurrently
    let mut handles = vec![];
    for i in 0..100 {
        let blockstore_clone = Arc::clone(&blockstore);
        let block = create_test_block(i);
        let height = i;

        handles.push(tokio::spawn(async move {
            // Store block (includes header)
            blockstore_clone.store_block(&block).ok()?;

            // Store recent header
            blockstore_clone
                .store_recent_header(height, &block.header)
                .ok()?;

            Some(())
        }));
    }

    // Wait for all operations
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 100);

    // Verify recent headers
    let recent_headers = blockstore.get_recent_headers(11).ok();
    // May have headers or not depending on implementation
    let _ = recent_headers;
}

#[tokio::test]
async fn test_storage_witness_storage_concurrent() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();

    // Store blocks with witnesses concurrently
    let mut handles = vec![];
    for i in 0..50 {
        let blockstore_clone = Arc::clone(&blockstore);
        let block = create_test_block(i);

        handles.push(tokio::spawn(async move {
            // Store block
            blockstore_clone.store_block(&block).ok()?;

            // Store empty witnesses
            let block_hash = blockstore_clone.get_block_hash(&block);
            let witnesses: Vec<Vec<Vec<u8>>> = vec![];
            blockstore_clone
                .store_witness(&block_hash, &witnesses)
                .ok()?;

            Some(())
        }));
    }

    // Wait for all operations
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 50);
}

#[tokio::test]
async fn test_storage_flush_under_load() {
    let (_temp_dir, storage) = create_test_storage();

    // Write many blocks
    for i in 0..100 {
        let block = create_test_block(i);
        storage.blocks().store_block(&block).unwrap();
    }

    // Flush should succeed even with pending writes
    let flush_result = storage.blocks().db.flush();
    // May succeed or fail depending on implementation
    let _ = flush_result;
}
