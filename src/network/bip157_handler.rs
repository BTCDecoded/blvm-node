//! BIP157 Message Handler
//!
//! Handles incoming BIP157 filter requests and generates appropriate responses
//! using the BlockFilterService.

use crate::network::filter_service::BlockFilterService;
use crate::network::protocol::{
    GetCfiltersMessage, CfilterMessage, GetCfheadersMessage, CfheadersMessage,
    GetCfcheckptMessage, CfcheckptMessage, ProtocolMessage, FilterHeaderData,
};
use protocol_engine::Hash;
use anyhow::{Result, anyhow};

/// Handle GetCfilters request
pub fn handle_getcfilters(
    request: &GetCfiltersMessage,
    _filter_service: &BlockFilterService,
) -> Result<Vec<ProtocolMessage>> {
    // BIP157: Validate filter type
    if request.filter_type != 0 {
        return Err(anyhow!("Unsupported filter type: {}", request.filter_type));
    }

    // Find block hashes in range [start_height, stop_hash]
    // This is simplified - in production, would query block index
    // For now, return empty response
    let mut responses = Vec::new();

    // In production, would iterate through block hashes and generate filters
    // For now, this is a placeholder that shows the structure
    // TODO: Query block index to get hashes in range [start_height, stop_hash]
    // For each block hash, get or generate filter and create Cfilter response
    // Example:
    // for block_hash in block_hashes_in_range {
    //     let filter = filter_service.get_filter(&block_hash)
    //         .or_else(|| generate_and_cache_filter(block, utxo_scripts))?;
    //     responses.push(ProtocolMessage::Cfilter(CfilterMessage {
    //         filter_type: 0,
    //         block_hash,
    //         filter_data: filter.filter_data,
    //         num_elements: filter.num_elements,
    //     }));
    // }

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
    let filter_headers = filter_service.get_filter_headers_range(
        request.start_height,
        request.stop_hash,
    )?;

    // Get previous filter header
    let prev_header = filter_service.get_prev_filter_header(request.start_height)
        .unwrap_or_else(|| {
            // Genesis filter header (all zeros)
            crate::bip157::FilterHeader {
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

    let filter = filter_service.get_filter(&block_hash)
        .ok_or_else(|| anyhow!("Filter not found for block hash"))?;

    Ok(ProtocolMessage::Cfilter(CfilterMessage {
        filter_type,
        block_hash,
        filter_data: filter.filter_data,
        num_elements: filter.num_elements,
    }))
}

