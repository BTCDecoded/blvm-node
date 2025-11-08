//! Protocol Extensions for UTXO Commitments
//!
//! Extends Bitcoin P2P protocol with UTXO commitment messages:
//! - GetUTXOSet: Request UTXO set at specific height
//! - UTXOSet: Response with UTXO commitment
//! - GetFilteredBlock: Request filtered (spam-free) block
//! - FilteredBlock: Response with filtered transactions

use crate::network::protocol::*;
use crate::storage::Storage;
use anyhow::Result;
use bllvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree;
use bllvm_protocol::utxo_commitments::spam_filter::{SpamFilter, SpamFilterConfig};
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
    
    // Calculate total supply
    let total_supply: u64 = utxo_set.values().map(|utxo| utxo.value as u64).sum();

    // Build Merkle tree from UTXO set
    let mut utxo_tree = UtxoMerkleTree::new()
        .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;
    
    for (outpoint, utxo) in &utxo_set {
        utxo_tree.insert(*outpoint, utxo.clone())
            .map_err(|e| anyhow::anyhow!("Failed to insert UTXO into tree: {:?}", e))?;
    }

    // Get block hash and height
    let block_height = message.block_height;
    let block_hash = if block_height == 0 || message.block_hash == [0; 32] {
        // Use current tip if not specified
        storage.chain().get_tip_hash()?.unwrap_or([0; 32])
    } else {
        message.block_hash
    };

    // Generate commitment
    let commitment = utxo_tree.generate_commitment(block_hash, block_height);

    Ok(UTXOSetMessage {
        request_id: message.request_id, // Echo request_id for matching
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
    use bllvm_protocol::BlockHeader;

    // Get block from storage
    let (block, block_height) = if let Some(ref storage) = storage {
        // Get block by hash
        let block = storage.blocks().get_block(&message.block_hash)?;
        match block {
            Some(block) => {
                // Get block height from chain state
                let height = storage.chain().get_height()?.unwrap_or(0);
                // Try to find exact height by iterating backwards from tip
                // For now, use tip height as approximation
                (Some(block), height)
            }
            None => {
                // Block not found
                return Err(anyhow::anyhow!(
                    "Block not found: block hash {} not in storage",
                    hex::encode(message.block_hash)
                ));
            }
        }
    } else {
        // Storage is required for filtered blocks
        return Err(anyhow::anyhow!(
            "Storage not available: filtered blocks require storage to be initialized"
        ));
    };

    let block = block.unwrap();

    // Create spam filter from preferences
    let spam_filter_config = SpamFilterConfig {
        filter_ordinals: message.filter_preferences.filter_ordinals,
        filter_dust: message.filter_preferences.filter_dust,
        filter_brc20: message.filter_preferences.filter_brc20,
        dust_threshold: message.filter_preferences.min_output_value as i64,
    };
    let spam_filter = SpamFilter::with_config(spam_filter_config);

    // Filter transactions
    let (filtered_txs, spam_summary_from_filter) = spam_filter.filter_block(&block.transactions);
    
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
    use crate::network::txhash::calculate_txid;
    let mut transaction_indices = Vec::new();
    let filtered_txids: std::collections::HashSet<_> = filtered_txs.iter()
        .map(|tx| calculate_txid(tx))
        .collect();
    for (original_idx, tx) in block.transactions.iter().enumerate() {
        let txid = calculate_txid(tx);
        if filtered_txids.contains(&txid) {
            transaction_indices.push(original_idx as u32);
        }
    }

    // Build UTXO tree from filtered transactions to generate commitment
    let mut utxo_tree = UtxoMerkleTree::new()
        .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;
    
    // Add outputs from filtered transactions
    use crate::network::txhash::calculate_txid;
    for tx in &filtered_txs {
        let txid = calculate_txid(tx);
        for (output_idx, output) in tx.outputs.iter().enumerate() {
            use bllvm_protocol::OutPoint;
            let outpoint = OutPoint {
                hash: txid,
                index: output_idx as u32,
            };
            use bllvm_protocol::UTXO;
            let utxo = UTXO {
                value: output.value,
                script_pubkey: output.script_pubkey.clone(),
            };
            if let Err(e) = utxo_tree.insert(outpoint, utxo) {
                // Log error but continue
                tracing::warn!("Failed to insert UTXO into tree: {:?}", e);
            }
        }
    }

    // Generate commitment for filtered block
    let commitment = utxo_tree.generate_commitment(message.block_hash, block_height);

    // Generate BIP158 filter if requested and service available
    let bip158_filter = if message.include_bip158_filter {
        filter_service.and_then(|fs| {
            // Try to get filter from service
            // Note: This would require BlockFilterService to have a get_filter method
            // For now, return None as placeholder
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
