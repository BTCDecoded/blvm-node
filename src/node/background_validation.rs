//! AssumeUTXO background validation
//!
//! When a snapshot is loaded, a second chainstate validates blocks from genesis
//! to the snapshot base in the background. When it reaches the base block, the
//! UTXO hash is compared against chainparams. This achieves full-node security
//! once validation completes.

use crate::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};
use crate::storage::assumeutxo::{
    assumeutxo_data_for_blockhash, write_background_validated_marker, AssumeUtxoManager,
};
use crate::storage::Storage;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tracing::{error, info, warn};

const CHAINSTATE_BACKGROUND_DIR: &str = "chainstate_background";

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

    let config = ParallelIBDConfig::default();
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
        )
        .await?;

    info!(
        "AssumeUTXO background validation: IBD complete at height {}, verifying UTXO hash",
        base_height
    );

    let utxo_set = storage_arc.utxos().get_all_utxos()?;
    let computed_hash = AssumeUtxoManager::calculate_utxo_hash(&utxo_set)?;

    let expected =
        assumeutxo_data_for_blockhash(network, &base_blockhash).and_then(|d| d.hash_serialized);

    match expected {
        Some(expected_hash) => {
            if computed_hash == expected_hash {
                info!(
                    "AssumeUTXO background validation: hash verified at height {}",
                    base_height
                );
                write_background_validated_marker(data_dir, &base_blockhash)?;
            } else {
                error!(
                    "AssumeUTXO background validation: hash mismatch at height {}! \
                     Expected {}, got {}. Snapshot may be invalid.",
                    base_height,
                    hex::encode(expected_hash),
                    hex::encode(computed_hash)
                );
                return Err(anyhow::anyhow!("UTXO hash mismatch"));
            }
        }
        None => {
            warn!(
                "AssumeUTXO background validation: no expected hash in chainparams for block {}, \
                 skipping verification. Add hash_serialized to assumeutxo_data for full security.",
                hex::encode(base_blockhash)
            );
        }
    }

    Ok(())
}
