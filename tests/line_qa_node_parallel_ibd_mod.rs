//! Test-line QA: parallel IBD refuses an empty peer list.

use std::sync::Arc;

use blvm_node::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};
use blvm_node::storage::blockstore::BlockStore;
use blvm_node::storage::database::{create_database, default_backend};
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion, UtxoSet};

#[tokio::test]
async fn sync_parallel_rejects_an_empty_peer_list() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = create_database(dir.path(), default_backend(), None).expect("database");
    let blockstore = Arc::new(BlockStore::new(Arc::from(db)).expect("blockstore"));
    let protocol =
        Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).expect("protocol"));
    let ibd = Arc::new(ParallelIBD::new(ParallelIBDConfig::default()));
    let mut utxos = UtxoSet::default();

    match ibd
        .sync_parallel(
            0,
            1,
            &[],
            blockstore,
            None,
            protocol,
            &mut utxos,
            None,
            None,
        )
        .await
    {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("No peers available for parallel IBD"),
                "sync_parallel returned {message}"
            );
        }
        Ok(()) => panic!("parallel IBD started with no peers"),
    }
}
