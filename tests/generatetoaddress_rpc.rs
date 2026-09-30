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
