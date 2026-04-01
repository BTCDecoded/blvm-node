//! `generatetoaddress` on regtest: mine a few blocks via MiningRpc and check chain height.

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use serde_json::json;
use std::sync::Arc;
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
    assert_eq!(height, 2, "genesis at 0 plus 3 blocks => tip height 2");

    Ok(())
}
