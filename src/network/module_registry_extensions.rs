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
//!
//! Payment Integration:
//! - If module requires payment, returns PaymentRequest instead of module
//! - Verifies payment before serving module
//! - Supports both P2P and HTTP transports

use crate::module::registry::client::ModuleRegistry;
use crate::module::registry::manifest::ModuleManifest;
use crate::network::protocol::*;
use crate::payment::processor::PaymentProcessor;
use crate::payment::state_machine::PaymentStateMachine;
use anyhow::Result;
use std::sync::Arc;

/// Handle GetModule message
///
/// Responds with module data for the requested module name.
/// If module requires payment:
/// 1. Check if payment_id provided
/// 2. If not, return PaymentRequest (via error or special response)
/// 3. If yes, verify payment before serving module
///
/// For free modules:
/// 1. Look up module in registry
/// 2. Load manifest and binary from CAS
/// 3. Return Module response
pub async fn handle_get_module(
    message: GetModuleMessage,
    registry: Option<Arc<ModuleRegistry>>,
    payment_processor: Option<Arc<PaymentProcessor>>,
    payment_state_machine: Option<Arc<PaymentStateMachine>>,
    encryption: Option<std::sync::Arc<crate::module::encryption::ModuleEncryption>>,
    modules_dir: Option<std::path::PathBuf>,
    _node_script: Option<Vec<u8>>, // Node's payment address (10% of payment) - placeholder for future use
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

    // Check if module requires payment
    let mut module_binary = entry.binary;
    if let Some(ref payment_section) = entry.manifest.payment {
        if payment_section.required {
            // Module requires payment
            if let Some(ref payment_id) = message.payment_id {
                // Payment ID provided - verify payment and decrypt if confirmed
                let payment_confirmed = if let Some(ref state_machine) = payment_state_machine {
                    // Use state machine to check payment state
                    match state_machine.get_payment_state(payment_id).await {
                        Ok(state) => {
                            // Check if payment is settled or in mempool
                            match state {
                                crate::payment::state_machine::PaymentState::Settled { .. }
                                | crate::payment::state_machine::PaymentState::InMempool {
                                    ..
                                } => {
                                    tracing::info!(
                                        "Payment verified for module {} (payment_id: {}, state: {:?})",
                                        message.name,
                                        payment_id,
                                        state
                                    );
                                    true
                                }
                                _ => {
                                    // Payment pending - serve encrypted version
                                    false
                                }
                            }
                        }
                        Err(_) => {
                            // Fallback: check if payment request exists
                            if let Some(ref processor) = payment_processor {
                                processor.get_payment_request(payment_id).await.is_ok()
                            } else {
                                false
                            }
                        }
                    }
                } else if let Some(ref processor) = payment_processor {
                    // Fallback to payment processor if state machine not available
                    processor.get_payment_request(payment_id).await.is_ok()
                } else {
                    return Err(anyhow::anyhow!(
                        "Payment processor not available: cannot verify payment for module {}",
                        message.name
                    ));
                };

                // If payment confirmed, decrypt module; otherwise serve encrypted
                if payment_confirmed {
                    // Payment confirmed - decrypt module
                    if let (Some(ref enc), Some(ref modules_dir_path)) = (encryption, modules_dir) {
                        match crate::module::encryption::load_encrypted_module(
                            modules_dir_path,
                            &message.name,
                        )
                        .await
                        {
                            Ok((encrypted_binary, metadata)) => {
                                // Decrypt
                                let module_hash: [u8; 32] =
                                    metadata.module_hash[..32].try_into().map_err(|_| {
                                        anyhow::anyhow!("Invalid module hash length in metadata")
                                    })?;

                                match enc.decrypt_module(
                                    &encrypted_binary,
                                    &metadata.nonce,
                                    payment_id,
                                    &module_hash,
                                ) {
                                    Ok(decrypted) => {
                                        tracing::info!(
                                            "Module {} decrypted for payment {}",
                                            message.name,
                                            payment_id
                                        );
                                        module_binary = Some(decrypted);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Failed to decrypt module {}: {}",
                                            message.name,
                                            e
                                        );
                                        // Fall back to encrypted version
                                    }
                                }
                            }
                            Err(_) => {
                                // Encrypted module not found - may not be encrypted yet
                                // Use original binary if available
                            }
                        }
                    }
                } else {
                    // Payment pending - serve encrypted version
                    if let Some(ref modules_dir_path) = modules_dir {
                        match crate::module::encryption::load_encrypted_module(
                            modules_dir_path,
                            &message.name,
                        )
                        .await
                        {
                            Ok((encrypted_binary, _metadata)) => {
                                tracing::info!(
                                    "Serving encrypted module {} (payment pending)",
                                    message.name
                                );
                                module_binary = Some(encrypted_binary);
                            }
                            Err(_) => {
                                // Encrypted module not found - may not be encrypted yet
                                // Return error or serve nothing
                                return Err(anyhow::anyhow!(
                                    "Module {} payment pending but encrypted module not found",
                                    message.name
                                ));
                            }
                        }
                    } else {
                        return Err(anyhow::anyhow!(
                            "Payment pending for module {} but modules directory not configured",
                            message.name
                        ));
                    }
                }
            } else {
                // No payment ID provided - return error indicating payment required
                return Err(anyhow::anyhow!(
                    "Module {} requires payment. Request payment with GetPaymentRequest message (module_name: {})",
                    message.name,
                    message.name
                ));
            }
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
        binary: module_binary,
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
    let manifest: ModuleManifest = toml::from_str(&manifest_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse manifest: {}", e))?;

    // Get binary if requested
    let binary = if message.include_binary {
        // Get binary hash from manifest and fetch binary from CAS
        if let Some(binary_section) = &manifest.binary {
            if let Some(ref hash_str) = binary_section.hash {
                // Convert hash to ModuleHash format (hex-encoded SHA256)
                if hash_str.len() == 64 {
                    // Hex-encoded SHA256 (64 hex chars = 32 bytes)
                    let hash_bytes = hex::decode(hash_str)
                        .map_err(|e| anyhow::anyhow!("Invalid binary hash hex: {}", e))?;
                    if hash_bytes.len() == 32 {
                        let mut hash_array = [0u8; 32];
                        hash_array.copy_from_slice(&hash_bytes);

                        // Fetch binary from CAS
                        let cas = cas_arc.read().await;
                        cas.get(&hash_array)
                            .ok()
                            .map(|bin_data| {
                                // Verify size matches if specified
                                if let Some(expected_size) = binary_section.size {
                                    if bin_data.len() != expected_size as usize {
                                        tracing::warn!(
                                            "Binary size mismatch: expected {}, got {}",
                                            expected_size,
                                            bin_data.len()
                                        );
                                    }
                                }
                                bin_data
                            })
                    } else {
                        return Err(anyhow::anyhow!("Binary hash must be 32 bytes"));
                    }
                } else {
                    return Err(anyhow::anyhow!("Binary hash must be 64 hex characters (32 bytes)"));
                }
            } else {
                None
            }
        } else {
            None
        }
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
