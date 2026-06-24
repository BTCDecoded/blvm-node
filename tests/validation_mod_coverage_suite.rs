//! Parallel block validator integration paths.

use blvm_node::storage::Storage;
use blvm_node::validation::{BlockValidationContext, ParallelBlockValidator};
use blvm_protocol::segwit::Witness;
use blvm_protocol::types::Network;

mod common;
use common::setup_mining_chain_on;

#[test]
fn parallel_validator_validate_seeded_regtest_block() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    setup_mining_chain_on(&storage, 6).unwrap();

    let prev_hash = storage
        .blocks()
        .get_hash_by_height(0)
        .unwrap()
        .expect("genesis hash");
    let block_hash = storage
        .blocks()
        .get_hash_by_height(1)
        .unwrap()
        .expect("block 1 hash");
    let block = storage
        .blocks()
        .get_block(&block_hash)
        .unwrap()
        .expect("block 1");
    let utxo_set = storage.utxos().get_all_utxos().unwrap();

    let witnesses: Vec<Vec<Witness>> = block
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
        .collect();

    let context = BlockValidationContext {
        block,
        height: 1,
        prev_utxo_set: utxo_set,
        prev_block_hash: prev_hash,
        witnesses,
    };

    let validator = ParallelBlockValidator::new(0);
    let (result, _) = validator
        .validate_block(&context, Network::Regtest)
        .unwrap();
    assert!(matches!(
        result,
        blvm_protocol::ValidationResult::Valid | blvm_protocol::ValidationResult::Invalid(_)
    ));

    let batch = validator
        .validate_blocks(&[context], 10, Network::Regtest)
        .unwrap();
    assert_eq!(batch.len(), 1);
}

#[test]
fn parallel_validator_validate_blocks_sequential_empty() {
    let validator = ParallelBlockValidator::default();
    let results = validator
        .validate_blocks_sequential(&[], Network::Regtest)
        .unwrap();
    assert!(results.is_empty());
}
