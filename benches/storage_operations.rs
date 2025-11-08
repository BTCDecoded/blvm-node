use bllvm_consensus::{Block, BlockHeader, Transaction, TransactionOutput};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use reference_node::storage::Storage;
use tempfile::TempDir;

fn create_test_block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 0,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![TransactionOutput {
                value: 5000000000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }],
    }
}

fn benchmark_block_store_insert(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();

    let block = create_test_block();

    c.bench_function("block_store_insert", |b| {
        b.iter(|| {
            black_box(storage.blocks().store_block(black_box(&block))).unwrap();
        })
    });
}

fn benchmark_block_store_get(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();

    let block = create_test_block();
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();

    let _hash = hash; // Use hash to avoid unused variable warning

    c.bench_function("block_store_get", |b| {
        b.iter(|| {
            black_box(storage.blocks().get_block(black_box(&hash))).unwrap();
        })
    });
}

fn benchmark_chainstate_update(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();

    let block = create_test_block();
    let hash = storage.blocks().get_block_hash(&block);

    c.bench_function("chainstate_update_height", |b| {
        b.iter(|| {
            black_box(
                storage
                    .blocks()
                    .store_height(black_box(0), black_box(&hash)),
            )
            .unwrap();
        })
    });
}

fn benchmark_tx_index_insert(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();

    let block = create_test_block();
    let block_hash = storage.blocks().get_block_hash(&block);
    let tx = &block.transactions[0];

    c.bench_function("tx_index_insert", |b| {
        b.iter(|| {
            black_box(storage.transactions().index_transaction(
                black_box(&tx),
                black_box(&block_hash),
                black_box(0),
                black_box(0),
            ))
            .unwrap();
        })
    });
}

criterion_group!(
    benches,
    benchmark_block_store_insert,
    benchmark_block_store_get,
    benchmark_chainstate_update,
    benchmark_tx_index_insert
);
criterion_main!(benches);
