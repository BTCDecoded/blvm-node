//! AssumeUTXO background validation
//!
//! When a snapshot is loaded, a second chainstate validates blocks from genesis
//! to the snapshot base in the background. When it reaches the base block, the
//! UTXO hash is compared against chainparams. This achieves full-node security
//! once validation completes.

use crate::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};
use crate::storage::Storage;
use crate::storage::assumeutxo::{
    AssumeUtxoManager, assumeutxo_data_for_blockhash, persist_commitment_failed,
    write_background_validated_marker,
};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tracing::{error, info, warn};

const CHAINSTATE_BACKGROUND_DIR: &str = "chainstate_background";

/// Live surfaces a historical connect must not notify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoricalNotify {
    Mempool,
    Mining,
    Rpc,
    Zmq,
    Index,
}

/// Historical connect records nothing for mempool, mining, RPC, ZMQ, or indexes.
#[derive(Default)]
pub struct HistoricalConnectEvents {
    notified: Vec<HistoricalNotify>,
}

impl HistoricalConnectEvents {
    pub fn notified(&self) -> &[HistoricalNotify] {
        &self.notified
    }
}

/// Apply one historical block with the consensus connect path. `events` is not a live subscriber.
pub fn connect_historical_block(
    block: &blvm_protocol::Block,
    witnesses: &[Vec<blvm_protocol::segwit::Witness>],
    utxo_set: blvm_protocol::UtxoSet,
    height: u64,
    context: &blvm_protocol::block::BlockValidationContext,
    events: &HistoricalConnectEvents,
) -> Result<(blvm_protocol::ValidationResult, blvm_protocol::UtxoSet)> {
    debug_assert!(events.notified().is_empty());
    let (result, utxo_set, _undo) =
        blvm_consensus::block::connect_block(block, witnesses, utxo_set, height, context)?;
    Ok((result, utxo_set))
}

/// Spawn background validation when assumeutxo is active.
///
/// Runs IBD from genesis to base_height in a separate chainstate. When complete,
/// hashes the UTXO set and compares to chainparams. Writes a validated marker
/// on success.
#[cfg(feature = "production")]
pub fn spawn_assumeutxo_background_validation(
    data_dir: &Path,
    base_blockhash: [u8; 32],
    base_height: u64,
    network: &str,
    protocol: Arc<blvm_protocol::BitcoinProtocolEngine>,
    network_manager: Arc<crate::network::NetworkManager>,
    peer_addresses: Vec<String>,
) {
    let min_peers = if std::env::var("BLVM_IBD_PEERS")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        1
    } else {
        2
    };
    if peer_addresses.len() < min_peers {
        warn!(
            "AssumeUTXO background validation: not enough peers (have {}, need {}), skipping",
            peer_addresses.len(),
            min_peers
        );
        return;
    }

    let data_dir = data_dir.to_path_buf();
    let network = network.to_string();

    tokio::spawn(async move {
        if let Err(e) = run_background_validation(
            &data_dir,
            base_blockhash,
            base_height,
            &network,
            protocol,
            network_manager,
            peer_addresses,
        )
        .await
        {
            error!("AssumeUTXO background validation failed: {}", e);
        }
    });
}

#[cfg(feature = "production")]
async fn run_background_validation(
    data_dir: &Path,
    base_blockhash: [u8; 32],
    base_height: u64,
    network: &str,
    protocol: Arc<blvm_protocol::BitcoinProtocolEngine>,
    network_manager: Arc<crate::network::NetworkManager>,
    peer_addresses: Vec<String>,
) -> Result<()> {
    info!(
        "AssumeUTXO background validation: starting genesis -> height {} (block {})",
        base_height,
        hex::encode(base_blockhash)
    );

    let bg_dir = data_dir.join(CHAINSTATE_BACKGROUND_DIR);
    let backend = crate::storage::database::default_backend();
    let storage = Storage::with_backend(&bg_dir, backend)?;
    let storage_arc = Arc::new(storage);
    let blockstore = storage_arc.blocks();

    let mut config = ParallelIBDConfig::default();
    config.network = protocol.get_protocol_version().consensus_network();
    let mut parallel_ibd = ParallelIBD::new(config);
    parallel_ibd.initialize_peers(&peer_addresses);
    let parallel_ibd = Arc::new(parallel_ibd);

    let mut utxo_set = blvm_protocol::UtxoSet::default();
    parallel_ibd
        .sync_parallel(
            0,
            base_height,
            &peer_addresses,
            blockstore,
            Some(&storage_arc),
            protocol,
            &mut utxo_set,
            Some(network_manager),
            None, // EventPublisher not available in background validation
        )
        .await?;

    info!(
        "AssumeUTXO background validation: IBD complete at height {}, verifying UTXO hash",
        base_height
    );

    let utxo_set = storage_arc.load_chain_utxo_set()?;
    let computed_hash = AssumeUtxoManager::calculate_utxo_hash(&utxo_set)?;

    let expected =
        assumeutxo_data_for_blockhash(network, &base_blockhash).and_then(|d| d.hash_serialized);
    settle_historical_commitment(
        data_dir,
        base_height,
        &base_blockhash,
        computed_hash,
        expected,
    )
}

pub(crate) fn settle_historical_commitment(
    data_dir: &Path,
    base_height: u64,
    base_blockhash: &[u8; 32],
    computed_hash: blvm_protocol::Hash,
    expected_hash: Option<[u8; 32]>,
) -> Result<()> {
    match expected_hash {
        Some(expected) if computed_hash == expected => {
            info!(
                "AssumeUTXO background validation: hash verified at height {}",
                base_height
            );
            write_background_validated_marker(data_dir, base_blockhash)
        }
        Some(expected) => {
            error!(
                "AssumeUTXO background validation: hash mismatch at height {}! \
                 Expected {}, got {}. Snapshot may be invalid.",
                base_height,
                hex::encode(expected),
                hex::encode(computed_hash)
            );
            persist_commitment_failed(data_dir)?;
            Err(anyhow::anyhow!("UTXO hash mismatch"))
        }
        None => {
            error!(
                "AssumeUTXO background validation: no expected hash in chainparams for block {}. \
                 Refusing to mark snapshot validated without hash_serialized.",
                hex::encode(base_blockhash)
            );
            Err(anyhow::anyhow!(
                "AssumeUTXO background validation: missing hash_serialized in chainparams"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HistoricalConnectEvents, connect_historical_block, settle_historical_commitment};
    use crate::storage::assumeutxo::{COMMITMENT_FAILED, active_chainstate_refused};
    use blvm_protocol::block::BlockValidationContext;
    use blvm_protocol::types::Network;
    use blvm_protocol::{UtxoSet, ValidationResult};

    #[test]
    fn a_commitment_mismatch_persists_failed_and_refuses_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let err = settle_historical_commitment(
            dir.path(),
            100,
            &[0x11; 32],
            [0x22; 32],
            Some([0x33; 32]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("UTXO hash mismatch"));
        let status =
            std::fs::read_to_string(dir.path().join("chainstate_snapshot/commitment")).unwrap();
        assert_eq!(status, COMMITMENT_FAILED);
        assert!(active_chainstate_refused(dir.path()));
    }

    #[test]
    fn a_matching_commitment_does_not_refuse_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        settle_historical_commitment(dir.path(), 100, &[0x11; 32], [0x22; 32], Some([0x22; 32]))
            .unwrap();
        assert!(!active_chainstate_refused(dir.path()));
        assert!(!dir.path().join("chainstate_snapshot/commitment").exists());
    }

    #[test]
    fn historical_connect_uses_consensus_and_does_not_notify_live_surfaces() {
        let block = blvm_protocol::genesis::regtest_genesis();
        let witnesses: Vec<Vec<blvm_protocol::segwit::Witness>> = block
            .transactions
            .iter()
            .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
            .collect();
        let events = HistoricalConnectEvents::default();
        let (result, utxo) = connect_historical_block(
            &block,
            &witnesses,
            UtxoSet::default(),
            0,
            &BlockValidationContext::for_network(Network::Regtest),
            &events,
        )
        .unwrap();
        assert!(matches!(result, ValidationResult::Valid));
        assert_eq!(utxo.len(), 0);
        assert!(events.notified().is_empty());
    }
}
