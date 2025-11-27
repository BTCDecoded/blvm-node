//! Protocol Extensions for Module Registry
//!
//! Extends Bitcoin P2P protocol with module registry messages:
//! - GetModule: Request module by name
//! - Module: Response with module data
//! - GetModuleByHash: Request module by hash (content-addressable)
//! - ModuleByHash: Response with module by hash
//! - ModuleInv: Announce available modules
//! - GetModuleList: Request list of available modules
//! - ModuleList: Response with module list

use crate::module::registry::client::ModuleRegistry;
use crate::module::registry::manifest::ModuleManifest;
use crate::network::protocol::*;
use anyhow::Result;
use std::sync::Arc;

/// Handle GetModule message
///
/// Responds with module data for the requested module name.
/// 1. Look up module in registry
/// 2. Load manifest and binary from CAS
/// 3. Return Module response
pub async fn handle_get_module(
    message: GetModuleMessage,
    registry: Option<Arc<ModuleRegistry>>,
) -> Result<ModuleMessage> {
    let request_id = message.request_id; // Store for response
    let registry = match registry {
        Some(r) => r,
        None => {
            return Err(anyhow::anyhow!(
                "Module registry not available: cannot serve module requests"
            ));
        }
    };

    // Fetch module from registry
    let entry = registry
        .fetch_module(&message.name)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch module {}: {}", message.name, e))?;

    // Check version if specified
    if let Some(ref requested_version) = message.version {
        if entry.version != *requested_version {
            return Err(anyhow::anyhow!(
                "Module {} version mismatch: requested {}, available {}",
                message.name,
                requested_version,
                entry.version
            ));
        }
    }

    // Serialize manifest to TOML
    let manifest_toml = toml::to_string(&entry.manifest)
        .map_err(|e| anyhow::anyhow!("Failed to serialize manifest: {}", e))?;

    Ok(ModuleMessage {
        request_id, // Echo request_id for matching
        name: entry.name,
        version: entry.version,
        hash: entry.hash,
        manifest_hash: entry.manifest_hash,
        binary_hash: entry.binary_hash,
        manifest: manifest_toml.into_bytes(),
        binary: entry.binary,
    })
}

/// Handle GetModuleByHash message
///
/// Responds with module data for the requested hash (content-addressable).
/// 1. Look up module by hash in CAS
/// 2. Load manifest and binary
/// 3. Return ModuleByHash response
pub async fn handle_get_module_by_hash(
    message: GetModuleByHashMessage,
    registry: Option<Arc<ModuleRegistry>>,
) -> Result<ModuleByHashMessage> {
    let request_id = message.request_id; // Store for response
    let registry = match registry {
        Some(r) => r,
        None => {
            return Err(anyhow::anyhow!(
                "Module registry not available: cannot serve module requests"
            ));
        }
    };

    // Get manifest from CAS
    let cas_arc = registry.cas();
    let manifest_data = {
        let cas = cas_arc.read().await;
        cas.get(&message.hash)
            .map_err(|e| anyhow::anyhow!("Failed to get module manifest from CAS: {}", e))?
    };

    // Parse manifest
    let manifest_str = String::from_utf8(manifest_data.clone())
        .map_err(|e| anyhow::anyhow!("Failed to decode manifest: {}", e))?;
    let _manifest: ModuleManifest = toml::from_str(&manifest_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse manifest: {}", e))?;

    // Get binary if requested
    let binary = if message.include_binary {
        // TODO: Get binary hash from manifest and fetch binary
        // For now, return None (will be implemented when we have binary hash lookup)
        None
    } else {
        None
    };

    Ok(ModuleByHashMessage {
        request_id, // Echo request_id for matching
        hash: message.hash,
        manifest: manifest_data,
        binary,
    })
}

/// Handle GetModuleList message
///
/// Responds with list of available modules.
/// 1. Get list from registry cache
/// 2. Apply filters (name prefix, max count)
/// 3. Return ModuleList response
pub async fn handle_get_module_list(
    message: GetModuleListMessage,
    registry: Option<Arc<ModuleRegistry>>,
) -> Result<ModuleListMessage> {
    let registry = match registry {
        Some(r) => r,
        None => {
            return Err(anyhow::anyhow!(
                "Module registry not available: cannot serve module list"
            ));
        }
    };

    // Get cached modules
    let name_prefix = message.name_prefix.clone();
    let max_count = message.max_count;
    let cache_arc = registry.local_cache();
    let mut modules: Vec<ModuleInventoryItem> = {
        let cache = cache_arc.read().await;
        cache
            .list_modules()
            .iter()
            .filter_map(|name| {
                // Apply name prefix filter if specified
                if let Some(ref prefix) = &name_prefix {
                    if !name.starts_with(prefix) {
                        return None;
                    }
                }

                // Get cached module entry
                cache.get(name).map(|cached| ModuleInventoryItem {
                    name: cached.name.clone(),
                    version: cached.version.clone(),
                    hash: cached.hash,
                })
            })
            .collect()
    };

    // Apply max count limit
    if let Some(max) = max_count {
        modules.truncate(max as usize);
    }

    Ok(ModuleListMessage { modules })
}

/// Serialize GetModule message to protocol format
pub fn serialize_get_module(message: &GetModuleMessage) -> Result<Vec<u8>> {
    use crate::network::protocol::ProtocolParser;
    ProtocolParser::serialize_message(&ProtocolMessage::GetModule(message.clone()))
}

/// Deserialize Module message from protocol format
pub fn deserialize_module(data: &[u8]) -> Result<ModuleMessage> {
    use crate::network::protocol::ProtocolParser;
    match ProtocolParser::parse_message(data)? {
        ProtocolMessage::Module(msg) => Ok(msg),
        _ => Err(anyhow::anyhow!("Expected Module message")),
    }
}

/// Serialize GetModuleByHash message to protocol format
pub fn serialize_get_module_by_hash(message: &GetModuleByHashMessage) -> Result<Vec<u8>> {
    use crate::network::protocol::ProtocolParser;
    ProtocolParser::serialize_message(&ProtocolMessage::GetModuleByHash(message.clone()))
}

/// Deserialize ModuleByHash message from protocol format
pub fn deserialize_module_by_hash(data: &[u8]) -> Result<ModuleByHashMessage> {
    use crate::network::protocol::ProtocolParser;
    match ProtocolParser::parse_message(data)? {
        ProtocolMessage::ModuleByHash(msg) => Ok(msg),
        _ => Err(anyhow::anyhow!("Expected ModuleByHash message")),
    }
}
