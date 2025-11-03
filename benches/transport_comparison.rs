use criterion::{black_box, criterion_group, criterion_main, Criterion};
use reference_node::network::compact_blocks::{create_compact_block};
use consensus_proof::{Block, BlockHeader, Transaction, TransactionOutput};
use std::collections::HashSet;

fn create_test_block_with_txs(tx_count: usize) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 0,
            bits: 0x1d00ffff,
            nonce: 12345,
        },
        transactions: (0..tx_count).map(|i| Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![TransactionOutput {
                value: 5000000000 + i as i64,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }).collect(),
    }
}

fn benchmark_compact_block_creation(c: &mut Criterion) {
    let block = create_test_block_with_txs(100);
    let prefilled_indices = HashSet::new();
    
    c.bench_function("create_compact_block_100_tx", |b| {
        b.iter(|| {
            black_box(create_compact_block(
                black_box(&block),
                black_box(12345),
                black_box(&prefilled_indices),
            ));
        })
    });
}

fn benchmark_compact_block_vs_full_block_size(c: &mut Criterion) {
    let block = create_test_block_with_txs(1000);
    let prefilled_indices = HashSet::new();
    
    c.bench_function("compact_block_size_1000_tx", |b| {
        b.iter(|| {
            let compact = create_compact_block(&block, 12345, &prefilled_indices);
            // Estimate size (simplified)
            black_box(compact.short_ids.len() * 6 + compact.prefilled_txs.len());
        })
    });
}

criterion_group!(
    benches,
    benchmark_compact_block_creation,
    benchmark_compact_block_vs_full_block_size
);
criterion_main!(benches);

