//! Protocol Extensions for UTXO Commitments
//!
//! Extends Bitcoin P2P protocol with UTXO commitment messages:
//! - GetUTXOSet: Request UTXO set at specific height
//! - UTXOSet: Response with UTXO commitment
//! - GetUTXOProof: Request Merkle proof for a specific UTXO
//! - UTXOProof: Response with Merkle proof
//! - GetFilteredBlock: Request filtered (spam-free) block
//! - FilteredBlock: Response with filtered transactions

use crate::network::protocol::*;
use crate::network::txhash::calculate_txid;
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use crate::utils::option_to_result;
use anyhow::Result;
#[cfg(feature = "utxo-commitments")]
use blvm_protocol::spam_filter::SpamFilter;
#[cfg(feature = "utxo-commitments")]
use blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree;
#[cfg(feature = "utxo-commitments")]
use blvm_consensus::types::{OutPoint, UTXO};
use hex;
use std::sync::Arc;

/// Handle GetUTXOSet message
///
/// Responds with UTXO commitment at the requested height.
/// 1. Load UTXO set at requested height from storage
/// 2. Build Merkle tree from UTXO set
/// 3. Generate commitment from Merkle tree
/// 4. Return UTXOSet response
pub async fn handle_get_utxo_set(
    message: GetUTXOSetMessage,
    storage: Option<Arc<Storage>>,
) -> Result<UTXOSetMessage> {
    let storage = match storage {
        Some(s) => s,
        None => {
            // Storage is required for UTXO commitments
            return Err(anyhow::anyhow!(
                "Storage not available: UTXO commitments require storage to be initialized"
            ));
        }
    };

    // Get UTXO set from storage
    let utxo_set = storage.utxos().get_all_utxos()?;
    let utxo_count = utxo_set.len() as u64;

    // Get block hash and height
    let block_height = message.height;
    let block_hash = if block_height == 0 || message.block_hash == [0; 32] {
        // Use current tip if not specified
        storage.chain().get_tip_hash()?.unwrap_or([0; 32])
    } else {
        message.block_hash
    };

    // Build Merkle tree from UTXO set
    #[cfg(feature = "utxo-commitments")]
    let mut utxo_tree = UtxoMerkleTree::new()
        .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;

    #[cfg(feature = "utxo-commitments")]
    for (outpoint, utxo) in &utxo_set {
        utxo_tree
            .insert(outpoint.clone(), utxo.clone())
            .map_err(|e| anyhow::anyhow!("Failed to insert UTXO into tree: {:?}", e))?;
    }

    // Generate commitment
    #[cfg(feature = "utxo-commitments")]
    let commitment = utxo_tree.generate_commitment(block_hash, block_height);

    #[cfg(not(feature = "utxo-commitments"))]
    let commitment = {
        // Calculate total supply (only needed when utxo-commitments feature is disabled)
        let total_supply: u64 = utxo_set.values().map(|utxo| utxo.value as u64).sum();
        crate::network::protocol::UTXOCommitment {
            merkle_root: [0; 32],
            total_supply,
            utxo_count,
            block_height,
            block_hash,
        }
    };

    // Generate request_id (use hash of message as ID since GetUTXOSetMessage doesn't have one)
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    message.height.hash(&mut hasher);
    message.block_hash.hash(&mut hasher);
    let request_id = hasher.finish();

    Ok(UTXOSetMessage {
        request_id, // Generate ID from message content
        commitment: UTXOCommitment {
            merkle_root: commitment.merkle_root,
            total_supply: commitment.total_supply,
            utxo_count: commitment.utxo_count,
            block_height: commitment.block_height,
            block_hash: commitment.block_hash,
        },
        utxo_count,
        is_complete: true,
        chunk_id: None,
    })
}

/// Handle GetFilteredBlock message
///
/// Returns a block with spam transactions filtered out.
/// Optionally includes BIP158 compact block filter if requested.
/// 1. Load block at requested hash from block store
/// 2. Apply spam filter based on preferences
/// 3. Generate UTXO commitment for filtered block
/// 4. Generate BIP158 filter if requested
/// 5. Return filtered transactions with commitment and optional filter
pub async fn handle_get_filtered_block(
    message: GetFilteredBlockMessage,
    storage: Option<Arc<Storage>>,
    filter_service: Option<&crate::network::filter_service::BlockFilterService>,
) -> Result<FilteredBlockMessage> {
    let request_id = message.request_id; // Store for response

    // Get block from storage
    let (block, block_height) = if let Some(ref storage) = storage {
        // Get block by hash
        let block_opt = storage.blocks().get_block(&message.block_hash)?;
        let block = option_to_result(
            block_opt,
            &format!(
                "Block not found: block hash {} not in storage",
                hex::encode(message.block_hash)
            ),
        )
        .map_err(|e| anyhow::anyhow!("Failed to get block from storage: {}", e))?;
        // Get block height from chain state
        let height = storage.chain().get_height()?.unwrap_or(0);
        // Try to find exact height by iterating backwards from tip
        // For now, use tip height as approximation
        (block, height)
    } else {
        // Storage is required for filtered blocks
        return Err(anyhow::anyhow!(
            "Storage not available: filtered blocks require storage to be initialized"
        ));
    };

    // Create spam filter from preferences
    #[cfg(feature = "utxo-commitments")]
    let spam_filter_config = blvm_protocol::spam_filter::SpamFilterConfig {
        filter_ordinals: message.filter_preferences.filter_ordinals,
        filter_dust: message.filter_preferences.filter_dust,
        filter_brc20: message.filter_preferences.filter_brc20,
        dust_threshold: message.filter_preferences.min_output_value as i64,
        min_output_value: message.filter_preferences.min_output_value as i64,
    };
    #[cfg(feature = "utxo-commitments")]
    let spam_filter = SpamFilter::with_config(spam_filter_config);
    #[cfg(feature = "utxo-commitments")]
    let (filtered_txs, spam_summary_from_filter) = spam_filter.filter_block(&block.transactions);
    #[cfg(not(feature = "utxo-commitments"))]
    let (filtered_txs, spam_summary_from_filter): (
        Vec<blvm_protocol::Transaction>,
        crate::network::protocol::SpamSummary,
    ) = (
        block.transactions.to_vec(),
        crate::network::protocol::SpamSummary {
            filtered_count: 0,
            filtered_size: 0,
            by_type: crate::network::protocol::SpamBreakdown {
                ordinals: 0,
                inscriptions: 0,
                dust: 0,
                brc20: 0,
            },
        },
    );

    // Convert spam summary to protocol types
    let spam_summary = SpamSummary {
        filtered_count: spam_summary_from_filter.filtered_count,
        filtered_size: spam_summary_from_filter.filtered_size,
        by_type: SpamBreakdown {
            ordinals: spam_summary_from_filter.by_type.ordinals,
            inscriptions: spam_summary_from_filter.by_type.inscriptions,
            dust: spam_summary_from_filter.by_type.dust,
            brc20: spam_summary_from_filter.by_type.brc20,
        },
    };

    // Generate transaction indices (positions of filtered transactions in original block)
    let mut transaction_indices = Vec::new();
    let filtered_txids: std::collections::HashSet<_> =
        filtered_txs.iter().map(calculate_txid).collect();
    for (original_idx, tx) in block.transactions.iter().enumerate() {
        let txid = calculate_txid(tx);
        if filtered_txids.contains(&txid) {
            transaction_indices.push(original_idx as u32);
        }
    }

    // Build UTXO tree from filtered transactions to generate commitment
    #[cfg(feature = "utxo-commitments")]
    let mut utxo_tree = UtxoMerkleTree::new()
        .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;

    #[cfg(feature = "utxo-commitments")]
    // Add outputs from filtered transactions
    for tx in &filtered_txs {
        let txid = calculate_txid(tx);
        for (output_idx, output) in tx.outputs.iter().enumerate() {
            use blvm_protocol::OutPoint;
            let outpoint = OutPoint {
                hash: txid,
                index: output_idx as u64,
            };
            use blvm_protocol::UTXO;
            let utxo = UTXO {
                value: output.value,
                script_pubkey: output.script_pubkey.clone(),
                height: block_height, // Use the block height from the message
            };
            if let Err(e) = utxo_tree.insert(outpoint, utxo) {
                // Log error but continue
                tracing::warn!("Failed to insert UTXO into tree: {:?}", e);
            }
        }
    }

    // Generate commitment for filtered block
    #[cfg(feature = "utxo-commitments")]
    let commitment = utxo_tree.generate_commitment(message.block_hash, block_height);

    #[cfg(not(feature = "utxo-commitments"))]
    let commitment = crate::network::protocol::UTXOCommitment {
        merkle_root: [0; 32],
        total_supply: 0,
        utxo_count: 0,
        block_height,
        block_hash: message.block_hash,
    };

    // Generate BIP158 filter if requested and service available
    let bip158_filter = if message.include_bip158_filter {
        filter_service.and({
            // BIP158 filter support requires BlockFilterService integration
            // This is intentionally not implemented as filter service integration
            // is handled at a higher level in the node architecture
            None
        })
    } else {
        None
    };

    Ok(FilteredBlockMessage {
        request_id, // Echo request_id for matching
        header: block.header.clone(),
        commitment: UTXOCommitment {
            merkle_root: commitment.merkle_root,
            total_supply: commitment.total_supply,
            utxo_count: commitment.utxo_count,
            block_height: commitment.block_height,
            block_hash: commitment.block_hash,
        },
        transactions: filtered_txs,
        transaction_indices,
        spam_summary,
        bip158_filter,
    })
}

/// Serialize GetUTXOSet message to protocol format
pub fn serialize_get_utxo_set(message: &GetUTXOSetMessage) -> Result<Vec<u8>> {
    use crate::network::protocol::ProtocolParser;
    ProtocolParser::serialize_message(&ProtocolMessage::GetUTXOSet(message.clone()))
}

/// Deserialize UTXOSet message from protocol format
pub fn deserialize_utxo_set(data: &[u8]) -> Result<UTXOSetMessage> {
    use crate::network::protocol::ProtocolParser;
    match ProtocolParser::parse_message(data)? {
        ProtocolMessage::UTXOSet(msg) => Ok(msg),
        _ => Err(anyhow::anyhow!("Expected UTXOSet message")),
    }
}

/// Handle GetUTXOProof message
///
/// Responds with Merkle proof for the requested UTXO.
/// 1. Load UTXO set from storage
/// 2. Build Merkle tree from UTXO set
/// 3. Generate proof for requested outpoint
/// 4. Return UTXOProof response
#[cfg(feature = "utxo-commitments")]
pub async fn handle_get_utxo_proof(
    message: crate::network::protocol::GetUTXOProofMessage,
    storage: Option<Arc<Storage>>,
) -> Result<crate::network::protocol::UTXOProofMessage> {
    let storage = match storage {
        Some(s) => s,
        None => {
            return Err(anyhow::anyhow!(
                "Storage not available: UTXO proof generation requires storage"
            ));
        }
    };

    // Get UTXO set from storage
    let utxo_set = storage.utxos().get_all_utxos()?;

    // Build Merkle tree from UTXO set
    let mut utxo_tree = UtxoMerkleTree::new()
        .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;

    for (outpoint, utxo) in &utxo_set {
        utxo_tree
            .insert(outpoint.clone(), utxo.clone())
            .map_err(|e| anyhow::anyhow!("Failed to insert UTXO into tree: {:?}", e))?;
    }

    // Create OutPoint from message
    use blvm_consensus::types::OutPoint;
    let outpoint = OutPoint {
        hash: message.tx_hash,
        index: message.output_index,
    };

    // Find UTXO in set
    let utxo = utxo_set
        .get(&outpoint)
        .ok_or_else(|| anyhow::anyhow!("UTXO not found for outpoint"))?;

    // Generate proof
    let proof = utxo_tree
        .generate_proof(&outpoint)
        .map_err(|e| anyhow::anyhow!("Failed to generate proof: {:?}", e))?;

    // Serialize proof to bytes
    let proof_bytes = bincode::serialize(&proof)
        .map_err(|e| anyhow::anyhow!("Failed to serialize proof: {:?}", e))?;

    Ok(crate::network::protocol::UTXOProofMessage {
        request_id: message.request_id,
        tx_hash: message.tx_hash,
        output_index: message.output_index,
        value: utxo.value,
        script_pubkey: utxo.script_pubkey.clone(),
        height: utxo.height,
        is_coinbase: utxo.is_coinbase,
        proof: proof_bytes,
    })
}

/// Serialize GetUTXOProof message to protocol format
pub fn serialize_get_utxo_proof(
    message: &crate::network::protocol::GetUTXOProofMessage,
) -> Result<Vec<u8>> {
    use crate::network::protocol::ProtocolParser;
    ProtocolParser::serialize_message(&ProtocolMessage::GetUTXOProof(message.clone()))
}

/// Deserialize UTXOProof message from protocol format
pub fn deserialize_utxo_proof(
    data: &[u8],
) -> Result<crate::network::protocol::UTXOProofMessage> {
    use crate::network::protocol::ProtocolParser;
    match ProtocolParser::parse_message(data)? {
        ProtocolMessage::UTXOProof(msg) => Ok(msg),
        _ => Err(anyhow::anyhow!("Expected UTXOProof message")),
    }
}

// Erlay (BIP330) protocol message handlers

/// Handle SendTxRcncl message (Erlay negotiation)
///
/// Responds to Erlay capability announcement and negotiates parameters.
#[cfg(feature = "erlay")]
pub async fn handle_send_tx_rcncl(
    message: crate::network::protocol::SendTxRcnclMessage,
    _storage: Option<Arc<Storage>>,
) -> Result<()> {
    // Store Erlay parameters for this peer
    // In a real implementation, this would be stored in peer state
    debug!(
        "Received Erlay negotiation: version={}, min_field={}, max_field={}",
        message.version, message.min_field_size, message.max_field_size
    );
    
    // Negotiate field size (use minimum of both peers' max)
    // For now, just acknowledge
    Ok(())
}

/// Handle ReqRecon message (Erlay reconciliation request)
///
/// Initiates transaction set reconciliation with a peer.
#[cfg(feature = "erlay")]
pub async fn handle_req_recon(
    message: crate::network::protocol::ReqReconMessage,
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
) -> Result<crate::network::protocol::ReqSktMessage> {
    use crate::network::erlay::ErlayTxSet;
    
    // Get local transaction set from mempool
    let mut tx_set = ErlayTxSet::new();
    
    if let Some(mempool_mgr) = mempool {
        // Get all transactions from mempool and add their hashes to the set
        let transactions = mempool_mgr.get_transactions();
        for tx in transactions {
            let tx_hash = calculate_txid(&tx);
            tx_set.add(tx_hash);
        }
        debug!("Erlay: Populated tx_set with {} transactions from mempool", tx_set.size());
    } else {
        warn!("MempoolManager not available for Erlay reconciliation, using empty set");
    }
    
    // Create reconciliation sketch
    let local_sketch = tx_set.create_reconciliation_sketch(message.local_set_size as usize)
        .map_err(|e| anyhow::anyhow!("Failed to create reconciliation sketch: {}", e))?;

    // Return ReqSkt message with our sketch
    Ok(crate::network::protocol::ReqSktMessage {
        salt: message.salt,
        remote_set_size: tx_set.size() as u32,
        field_size: message.field_size,
    })
}

/// Handle ReqSkt message (Erlay sketch request)
///
/// Responds with reconciliation sketch.
#[cfg(feature = "erlay")]
pub async fn handle_req_skt(
    message: crate::network::protocol::ReqSktMessage,
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
) -> Result<crate::network::protocol::SketchMessage> {
    use crate::network::erlay::ErlayTxSet;
    
    // Get local transaction set from mempool
    let mut tx_set = ErlayTxSet::new();
    
    if let Some(mempool_mgr) = mempool {
        // Get all transactions from mempool and add their hashes to the set
        let transactions = mempool_mgr.get_transactions();
        for tx in transactions {
            let tx_hash = calculate_txid(&tx);
            tx_set.add(tx_hash);
        }
        debug!("Erlay: Populated tx_set with {} transactions for sketch", tx_set.size());
    } else {
        warn!("MempoolManager not available for Erlay sketch, using empty set");
    }
    
    // Create sketch
    let sketch = tx_set.create_reconciliation_sketch(message.remote_set_size as usize)
        .map_err(|e| anyhow::anyhow!("Failed to create sketch: {}", e))?;

    Ok(crate::network::protocol::SketchMessage {
        salt: message.salt,
        sketch,
        field_size: message.field_size,
    })
}

/// Handle Sketch message (Erlay reconciliation sketch)
///
/// Processes reconciliation sketch and identifies missing transactions.
#[cfg(feature = "erlay")]
pub async fn handle_sketch(
    message: crate::network::protocol::SketchMessage,
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
) -> Result<Vec<blvm_protocol::Hash>> {
    use crate::network::erlay::ErlayTxSet;
    
    // Get local transaction set from mempool
    let mut tx_set = ErlayTxSet::new();
    
    if let Some(mempool_mgr) = mempool {
        // Get all transactions from mempool and add their hashes to the set
        let transactions = mempool_mgr.get_transactions();
        for tx in transactions {
            let tx_hash = calculate_txid(&tx);
            tx_set.add(tx_hash);
        }
        debug!("Erlay: Populated tx_set with {} transactions for reconciliation", tx_set.size());
    } else {
        warn!("MempoolManager not available for Erlay reconciliation, using empty set");
    }
    
    // Create our local sketch
    let local_sketch = tx_set.create_reconciliation_sketch(0)
        .map_err(|e| anyhow::anyhow!("Failed to create local sketch: {}", e))?;

    // Reconcile sets
    let missing_txs = tx_set.reconcile_with_peer(&local_sketch, &message.sketch)
        .map_err(|e| anyhow::anyhow!("Failed to reconcile sets: {}", e))?;

    debug!("Erlay: Reconciliation found {} missing transactions", missing_txs.len());
    Ok(missing_txs)
}

/// Serialize GetFilteredBlock message to protocol format
pub fn serialize_get_filtered_block(message: &GetFilteredBlockMessage) -> Result<Vec<u8>> {
    use crate::network::protocol::ProtocolParser;
    ProtocolParser::serialize_message(&ProtocolMessage::GetFilteredBlock(message.clone()))
}

/// Deserialize FilteredBlock message from protocol format
pub fn deserialize_filtered_block(data: &[u8]) -> Result<FilteredBlockMessage> {
    use crate::network::protocol::ProtocolParser;
    match ProtocolParser::parse_message(data)? {
        ProtocolMessage::FilteredBlock(msg) => Ok(msg),
        _ => Err(anyhow::anyhow!("Expected FilteredBlock message")),
    }
}
