//! `generatetoaddress` on regtest: mine a few blocks via MiningRpc and check chain height.

use async_trait::async_trait;
use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::errors::RpcResult;
use blvm_node::rpc::mining::{CommonsGbtCaller, CommonsGbtSlot, MiningRpc};
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use serde_json::json;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generatetoaddress_regtest_extends_chain() -> anyhow::Result<()> {
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?);
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();

    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;

    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), mempool)
        .with_protocol_engine(Arc::clone(&protocol));

    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let params = json!([
        3u64,
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
        2_000_000u64
    ]);
    let result = mining.generate_to_address(&params).await?;
    let arr = result.as_array().expect("array of block hashes");
    assert_eq!(arr.len(), 3);
    for h in arr {
        assert_eq!(h.as_str().map(str::len), Some(64));
    }

    let height = storage.chain().get_height()?.expect("height");
    assert_eq!(
        height, 3,
        "genesis at height 0 plus 3 mined blocks => tip height 3"
    );

    // REV-TN-01 / REV-C-32: mined headers must use wall-clock time, not genesis constant.
    let tip_header = storage
        .chain()
        .get_tip_header()?
        .expect("tip header after mining");
    assert!(
        tip_header.timestamp > genesis_header.timestamp,
        "tip timestamp {} must exceed genesis {}",
        tip_header.timestamp,
        genesis_header.timestamp
    );
    assert_ne!(
        tip_header.timestamp, 1_231_006_505,
        "must not use hardcoded genesis timestamp"
    );

    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    assert!(
        tip_header.timestamp >= before.saturating_sub(5),
        "tip timestamp {} should be near wall clock (before={before})",
        tip_header.timestamp
    );
    assert!(
        tip_header.timestamp <= after,
        "tip timestamp {} should not be far in the future",
        tip_header.timestamp
    );

    Ok(())
}

struct HoldCommons;

#[async_trait]
impl CommonsGbtCaller for HoldCommons {
    async fn fetch_commons_gbt_outputs(&self) -> RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
        Err(blvm_node::rpc::errors::RpcError::internal_error(
            "commons pool holding; not issuing work",
        ))
    }
}

struct ScriptCommons;

#[async_trait]
impl CommonsGbtCaller for ScriptCommons {
    async fn fetch_commons_gbt_outputs(&self) -> RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
        Ok(Some(vec![(0, vec![0x51])]))
    }
}

fn generate_params() -> serde_json::Value {
    json!([
        1u64,
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
        2_000_000u64
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generatetoaddress_hold_refuses() -> anyhow::Result<()> {
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?);
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;
    let mempool = Arc::new(MempoolManager::new());
    let slot = CommonsGbtSlot::default();
    slot.set(Arc::new(HoldCommons));
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), mempool)
        .with_protocol_engine(Arc::clone(&protocol))
        .with_commons_gbt(slot);

    let err = mining
        .generate_to_address(&generate_params())
        .await
        .expect_err("hold must refuse generate");
    assert!(
        err.message.contains("holding"),
        "unexpected generate error: {}",
        err.message
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generatetoaddress_pays_commons_script() -> anyhow::Result<()> {
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?);
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;
    let mempool = Arc::new(MempoolManager::new());
    let slot = CommonsGbtSlot::default();
    slot.set(Arc::new(ScriptCommons));
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), mempool)
        .with_protocol_engine(Arc::clone(&protocol))
        .with_commons_gbt(slot);

    mining.generate_to_address(&generate_params()).await?;
    let tip = storage.chain().get_tip_hash()?.expect("tip");
    let block = storage.blocks().get_block(&tip)?.expect("block");
    let cb = &block.transactions[0];
    assert_eq!(cb.outputs[0].script_pubkey, vec![0x51]);
    Ok(())
}

fn funded_spend(
    prev_hash: [u8; 32],
    input_value: i64,
    output_value: i64,
) -> blvm_protocol::Transaction {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: prev_hash,
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: output_value,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    }
}

fn fund(
    storage: &Storage,
    mempool: &MempoolManager,
    prev_hash: [u8; 32],
    value: i64,
    script: Vec<u8>,
) {
    use blvm_protocol::{OutPoint, UTXO, UtxoSet};

    let outpoint = OutPoint {
        hash: prev_hash,
        index: 0,
    };
    let utxo = UTXO {
        value,
        script_pubkey: script.into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(outpoint, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generatetoaddress_adds_the_mempool_fee_and_connects_a_witness_spend() -> anyhow::Result<()>
{
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use sha2::Digest;

    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?);
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = blvm_node::config::mempool::MempoolPolicyConfig::default();
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));

    let prev = [0x11u8; 32];
    fund(&storage, &mempool, prev, 50_000, vec![OP_1]);
    let tx = funded_spend(prev, 50_000, 49_000);
    assert!(mempool.add_transaction(tx.clone()).unwrap());

    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool))
        .with_protocol_engine(Arc::clone(&protocol));
    mining.generate_to_address(&generate_params()).await?;
    let tip = storage.chain().get_tip_hash()?.expect("tip");
    let block = storage.blocks().get_block(&tip)?.expect("block");
    let subsidy = blvm_consensus::economic::get_block_subsidy_for_network(
        1,
        blvm_consensus::types::Network::Regtest,
    );
    assert_eq!(block.transactions[0].outputs[0].value, subsidy + 1_000);
    assert_eq!(
        calculate_tx_id(&block.transactions[1]),
        calculate_tx_id(&tx)
    );

    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = blvm_node::config::mempool::MempoolPolicyConfig::default();
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let witness_script = vec![OP_1];
    let digest = sha2::Sha256::digest(&witness_script);
    let mut program = vec![0x00, 0x20];
    program.extend_from_slice(&digest);
    let prev = [0x22u8; 32];
    fund(&storage, &mempool, prev, 41_000, program);
    let spend = funded_spend(prev, 41_000, 40_000);
    let stacks = vec![vec![witness_script]];
    assert!(
        mempool
            .add_transaction_with_witness(spend.clone(), Some(stacks.clone()))
            .unwrap()
    );
    let slot = CommonsGbtSlot::default();
    slot.set(Arc::new(ScriptCommons));
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool))
        .with_protocol_engine(protocol)
        .with_commons_gbt(slot);
    mining.generate_to_address(&generate_params()).await?;
    let tip = storage.chain().get_tip_hash()?.expect("witness tip");
    let block = storage.blocks().get_block(&tip)?.expect("witness block");
    assert_eq!(
        calculate_tx_id(&block.transactions[1]),
        calculate_tx_id(&spend)
    );
    let stored = storage.blocks().get_witness(&tip)?.expect("witnesses");
    assert_eq!(stored[1], stacks);
    Ok(())
}
