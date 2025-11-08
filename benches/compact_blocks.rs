use bllvm_consensus::{Block, BlockHeader, Transaction, TransactionOutput};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use reference_node::network::compact_blocks::{
    calculate_short_tx_id, calculate_tx_hash, create_compact_block,
    recommended_compact_block_version, should_prefer_compact_blocks,
};
use reference_node::network::transport::TransportType;
use std::collections::HashSet;

fn create_test_block() -> Block {
    let mut transactions = vec![Transaction {
        version: 1,
        inputs: vec![],
        outputs: vec![TransactionOutput {
            value: 5000000000,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    }];

    // Add 10 regular transactions
    for _ in 0..10 {
        transactions.push(Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![TransactionOutput {
                value: 100000000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        });
    }

    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 1234567890,
            bits: 0x1d00ffff,
            nonce: 12345,
        },
        transactions,
    }
}

fn benchmark_compact_block_creation(c: &mut Criterion) {
    let block = create_test_block();
    let nonce = block.header.nonce;
    let prefilled_indices = HashSet::new();

    c.bench_function("create_compact_block", |b| {
        b.iter(|| {
            black_box(create_compact_block(
                black_box(&block),
                black_box(nonce),
                black_box(&prefilled_indices),
            ))
        })
    });
}

fn benchmark_tx_hash_calculation(c: &mut Criterion) {
    let tx = &create_test_block().transactions[1]; // First non-coinbase

    c.bench_function("calculate_tx_hash", |b| {
        b.iter(|| black_box(calculate_tx_hash(black_box(tx))))
    });
}

fn benchmark_short_tx_id_calculation(c: &mut Criterion) {
    let tx_hash = [0x42u8; 32];
    let nonce = 12345u64;

    c.bench_function("calculate_short_tx_id", |b| {
        b.iter(|| black_box(calculate_short_tx_id(black_box(&tx_hash), black_box(nonce))))
    });
}

fn benchmark_transport_aware_functions(c: &mut Criterion) {
    c.bench_function("should_prefer_compact_blocks_tcp", |b| {
        b.iter(|| black_box(should_prefer_compact_blocks(TransportType::Tcp)))
    });

    #[cfg(feature = "iroh")]
    c.bench_function("should_prefer_compact_blocks_iroh", |b| {
        b.iter(|| black_box(should_prefer_compact_blocks(TransportType::Iroh)))
    });

    c.bench_function("recommended_version_tcp", |b| {
        b.iter(|| black_box(recommended_compact_block_version(TransportType::Tcp)))
    });

    #[cfg(feature = "iroh")]
    c.bench_function("recommended_version_iroh", |b| {
        b.iter(|| black_box(recommended_compact_block_version(TransportType::Iroh)))
    });
}

criterion_group!(
    benches,
    benchmark_compact_block_creation,
    benchmark_tx_hash_calculation,
    benchmark_short_tx_id_calculation,
    benchmark_transport_aware_functions
);
criterion_main!(benches);
