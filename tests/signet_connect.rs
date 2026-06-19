//! Signet network integration: genesis connect and custom challenge plumbing.

use blvm_node::node::block_processor::{store_block_with_context, validate_block_with_context};
use blvm_node::storage::Storage;
use blvm_node::{UtxoSet, ValidationResult};
use blvm_protocol::segwit::Witness;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::sync::Arc;
use tempfile::TempDir;

fn signet_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    (temp_dir, storage)
}

#[test]
fn signet_genesis_connects_at_height_zero() {
    let (_dir, storage) = signet_storage();
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Signet).unwrap();
    let genesis = protocol.get_network_params().genesis_block.clone();
    let witnesses: Vec<Vec<Witness>> = genesis
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
        .collect();

    store_block_with_context(&storage.blocks(), &genesis, &witnesses, 0).unwrap();

    let mut utxo_set = UtxoSet::default();
    let result = validate_block_with_context(
        &storage.blocks(),
        &protocol,
        &genesis,
        &witnesses,
        &mut utxo_set,
        0,
    )
    .unwrap();

    assert!(
        matches!(result, ValidationResult::Valid),
        "signet genesis must connect at height 0: {result:?}"
    );
    assert!(!utxo_set.is_empty(), "genesis coinbase must create a UTXO");
}

#[test]
fn signet_custom_challenge_reaches_connect_context() {
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Signet)
        .unwrap()
        .with_signet_challenge(Some(vec![0x51]));
    let ctx = protocol.connect_block_validation_context(None::<&[blvm_protocol::BlockHeader]>, 0);
    assert_eq!(
        ctx.signet_challenge.as_ref().map(|b| b.as_ref()),
        Some([0x51].as_slice())
    );
}

#[test]
fn parse_signet_challenge_hex_from_config() {
    use blvm_node::config::parse_signet_challenge_hex;
    assert_eq!(parse_signet_challenge_hex("51").unwrap(), vec![0x51]);
    assert!(parse_signet_challenge_hex("not-hex").is_err());
}
