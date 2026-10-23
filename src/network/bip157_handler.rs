//! BIP157 Message Handler
//!
//! Handles incoming BIP157 filter requests and generates appropriate responses
//! using the BlockFilterService.

use crate::network::filter_service::BlockFilterService;
use crate::network::protocol::{
    CfcheckptMessage, CfheadersMessage, CfilterMessage, FilterHeaderData, GetCfcheckptMessage,
    GetCfheadersMessage, GetCfiltersMessage, ProtocolMessage,
};
use crate::storage::Storage;
use anyhow::{Result, anyhow};
use blvm_protocol::Hash;
use std::sync::Arc;

/// Handle GetCfilters request
pub fn handle_getcfilters(
    request: &GetCfiltersMessage,
    filter_service: &BlockFilterService,
    storage: Option<&Arc<Storage>>,
) -> Result<Vec<ProtocolMessage>> {
    // BIP157: Validate filter type
    if request.filter_type != 0 {
        return Err(anyhow!("Unsupported filter type: {}", request.filter_type));
    }

    let mut responses = Vec::new();

    if let Some(storage) = storage {
        // Get current height to determine stop height
        let current_height = storage.chain().get_height()?.unwrap_or(0) as u32;
        let start_height = request.start_height;

        // Find stop height by iterating from start until we find stop_hash
        let mut stop_height = start_height;
        let mut found_stop = false;

        // Iterate through blocks from start_height
        for height in start_height..=current_height.min(start_height + 2000) {
            // Get block hash by height, then get block
            if let Ok(Some(block_hash)) = storage.blocks().get_hash_by_height(height as u64) {
                // The height index stores the wire block id used to save the block.
                let filter = if let Some(cached) = filter_service.get_filter(&block_hash) {
                    let body_present = storage.blocks().has_block(&block_hash).unwrap_or(false);
                    if !body_present
                        && storage
                            .blocks()
                            .get_block(&block_hash)
                            .ok()
                            .flatten()
                            .is_none()
                    {
                        continue;
                    }
                    cached
                } else if let Ok(Some(outputs)) =
                    storage.blocks().output_scripts_and_prev_hash(&block_hash)
                {
                    #[cfg(feature = "production")]
                    let spent = storage
                        .blocks()
                        .spent_script_spans(&block_hash)
                        .unwrap_or_default();
                    #[cfg(not(feature = "production"))]
                    let spent = crate::storage::blockstore::ScriptSpans::default();
                    let scripts = crate::storage::blockstore::output_and_spent_slices(
                        &outputs.scripts,
                        &spent,
                    );

                    filter_service.generate_and_cache_filter_for_scripts(
                        scripts,
                        &[],
                        block_hash,
                        height,
                    )?
                } else {
                    continue;
                };

                if block_hash == request.stop_hash {
                    found_stop = true;
                    stop_height = height;
                }

                responses.push(ProtocolMessage::Cfilter(CfilterMessage {
                    filter_type: 0,
                    block_hash,
                    filter_data: filter.filter_data,
                    num_elements: filter.num_elements,
                }));

                if found_stop {
                    break;
                }
            }
        }

        if !found_stop && stop_height < current_height {
            // Stop hash not found in reasonable range, return what we have
        }
    }

    Ok(responses)
}

/// Handle GetCfheaders request
pub fn handle_getcfheaders(
    request: &GetCfheadersMessage,
    filter_service: &BlockFilterService,
) -> Result<ProtocolMessage> {
    // BIP157: Validate filter type
    if request.filter_type != 0 {
        return Err(anyhow!("Unsupported filter type: {}", request.filter_type));
    }

    // Get filter headers in range
    let filter_headers =
        filter_service.get_filter_headers_range(request.start_height, request.stop_hash)?;

    // Get previous filter header
    let prev_header = filter_service
        .get_prev_filter_header(request.start_height)
        .unwrap_or({
            // Genesis filter header (all zeros)
            blvm_protocol::bip157::FilterHeader {
                filter_hash: [0u8; 32],
                prev_header_hash: [0u8; 32],
            }
        });

    let prev_header_data = FilterHeaderData {
        filter_hash: prev_header.filter_hash,
        prev_header_hash: prev_header.prev_header_hash,
    };

    Ok(ProtocolMessage::Cfheaders(CfheadersMessage {
        filter_type: request.filter_type,
        stop_hash: request.stop_hash,
        prev_header: prev_header_data,
        filter_headers,
    }))
}

/// Handle GetCfcheckpt request
pub fn handle_getcfcheckpt(
    request: &GetCfcheckptMessage,
    filter_service: &BlockFilterService,
) -> Result<ProtocolMessage> {
    // BIP157: Validate filter type
    if request.filter_type != 0 {
        return Err(anyhow!("Unsupported filter type: {}", request.filter_type));
    }

    // Get filter checkpoints
    let filter_header_hashes = filter_service.get_filter_checkpoints(request.stop_hash)?;

    Ok(ProtocolMessage::Cfcheckpt(CfcheckptMessage {
        filter_type: request.filter_type,
        stop_hash: request.stop_hash,
        filter_header_hashes,
    }))
}

/// Generate Cfilter response for a single block
pub fn generate_cfilter_response(
    block_hash: Hash,
    filter_type: u8,
    filter_service: &BlockFilterService,
) -> Result<ProtocolMessage> {
    if filter_type != 0 {
        return Err(anyhow!("Unsupported filter type: {}", filter_type));
    }

    let filter = filter_service
        .get_filter(&block_hash)
        .ok_or_else(|| anyhow!("Filter not found for block hash"))?;

    Ok(ProtocolMessage::Cfilter(CfilterMessage {
        filter_type,
        block_hash,
        filter_data: filter.filter_data,
        num_elements: filter.num_elements,
    }))
}
