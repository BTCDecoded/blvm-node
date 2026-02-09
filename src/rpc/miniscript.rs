//! Miniscript RPC methods
//!
//! Provides RPC methods for miniscript functionality:
//! - getdescriptorinfo: Parse and validate descriptors
//! - analyzepsbt: Analyze PSBT scripts using miniscript
//!
//! Feature-gated: Only available when `miniscript` feature is enabled.

#[cfg(feature = "miniscript")]
pub mod miniscript_rpc {
    use super::super::errors::RpcError;
    use super::super::types::RpcResult;
    use crate::miniscript::miniscript_support;
    use serde_json::{json, Value};
    use std::str::FromStr;

    /// RPC method: getdescriptorinfo
    ///
    /// Analyzes a descriptor and returns information about it.
    ///
    /// # Arguments
    /// * `params` - JSON-RPC parameters containing descriptor string
    ///
    /// # Returns
    /// Descriptor information (isvalid, descriptor, checksum, etc.)
    pub async fn get_descriptor_info(
        params: &Value,
    ) -> Result<Value, RpcError> {
        // Extract descriptor from params
        let descriptor_str = params
            .get(0)
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError::invalid_params("descriptor string required"))?;

        // Parse descriptor using miniscript
        match miniscript_support::parse_descriptor(descriptor_str) {
            Ok(descriptor) => {
                // Get script from descriptor
                let script = descriptor.script_pubkey();
                let script_bytes: Vec<u8> = script.into();
                
                // Analyze script
                let analysis = miniscript_support::analyze_script(
                    &blvm_protocol::ByteString::from(script_bytes)
                ).map_err(|e| RpcError::internal_error(e.to_string()))?;

                // Calculate checksum and detect range
                let checksum = miniscript_support::calculate_descriptor_checksum(descriptor_str);
                let is_range = miniscript_support::is_range_descriptor(descriptor_str);
                
                // Return descriptor info (Bitcoin Core compatible format)
                Ok(json!({
                    "descriptor": descriptor_str,
                    "checksum": checksum,
                    "isrange": is_range,
                    "issolvable": analysis.is_miniscript,
                    "hasprivatekeys": false, // Descriptors don't contain private keys
                }))
            }
            Err(e) => {
                // Invalid descriptor
                Ok(json!({
                    "descriptor": descriptor_str,
                    "checksum": "",
                    "isrange": false,
                    "issolvable": false,
                    "hasprivatekeys": false,
                    "error": e.to_string(),
                }))
            }
        }
    }

    /// RPC method: analyzepsbt
    ///
    /// Analyzes a PSBT and provides information about it, including script analysis.
    ///
    /// # Arguments
    /// * `params` - JSON-RPC parameters containing PSBT string
    ///
    /// # Returns
    /// PSBT analysis including script information
    pub async fn analyze_psbt(
        params: &Value,
    ) -> Result<Value, RpcError> {
        // Extract PSBT from params
        let psbt_str = params
            .get(0)
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError::invalid_params("PSBT string required"))?;

        // Decode base64 PSBT
        use base64::{Engine as _, engine::general_purpose};
        let psbt_bytes = general_purpose::STANDARD.decode(psbt_str)
            .map_err(|e| RpcError::invalid_params(&format!("Invalid PSBT base64: {}", e)))?;

        // Parse PSBT using bitcoin crate's built-in PSBT support
        // miniscript depends on bitcoin crate which includes PSBT support
        use bitcoin::psbt::Psbt;
        let psbt = Psbt::deserialize(&psbt_bytes)
            .map_err(|e| RpcError::internal_error(&format!("Failed to parse PSBT: {}", e)))?;

        // Analyze inputs
        let mut input_analyses = Vec::new();
        for (_idx, input) in psbt.inputs.iter().enumerate() {
            let mut input_info = json!({
                "has_utxo": input.non_witness_utxo.is_some() || input.witness_utxo.is_some(),
                "has_final_script_sig": input.final_script_sig.is_some(),
                "has_final_script_witness": input.final_script_witness.is_some(),
            });

            // Analyze scripts if available
            if let Some(ref utxo) = input.witness_utxo {
                let script_bytes = utxo.script_pubkey.as_bytes();
                let analysis = miniscript_support::analyze_script(
                    &blvm_protocol::ByteString::from(script_bytes.to_vec())
                ).ok();
                
                if let Some(analysis) = analysis {
                    input_info["script_type"] = json!(analysis.script_type);
                    input_info["is_miniscript"] = json!(analysis.is_miniscript);
                }
            }

            input_analyses.push(input_info);
        }

        // Calculate estimated vsize (simplified)
        let estimated_vsize = psbt.unsigned_tx.weight().to_wu() as u64 / 4;

        // Determine next step
        let next = if psbt.inputs.iter().all(|i| i.final_script_sig.is_some() || i.final_script_witness.is_some()) {
            "finalizer"
        } else if psbt.inputs.iter().any(|i| !i.partial_sigs.is_empty()) {
            "signer"
        } else {
            "updater"
        };

        Ok(json!({
            "inputs": input_analyses,
            "estimated_vsize": estimated_vsize,
            "estimated_feerate": 0, // Would need fee calculation
            "fee": 0, // Would need fee calculation
            "next": next,
        }))
    }
}


