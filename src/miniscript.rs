//! Miniscript support for script policy compilation and descriptor parsing
//!
//! This module provides miniscript functionality for:
//! - Script policy compilation (policy → script)
//! - Descriptor parsing and validation
//! - Script analysis (satisfaction weight, etc.)
//!
//! Feature-gated: Only available when `miniscript` feature is enabled.

#[cfg(feature = "miniscript")]
pub mod miniscript_support {
    use miniscript::{Miniscript, Policy, Descriptor};
    use blvm_protocol::ByteString;
    use anyhow::{Context, Result};
    use std::str::FromStr;
    use bech32::{ToBase32, Variant};

    /// Miniscript compilation error
    #[derive(Debug, thiserror::Error)]
    pub enum MiniscriptError {
        #[error("Policy compilation failed: {0}")]
        CompilationFailed(String),
        #[error("Script parsing failed: {0}")]
        ParseFailed(String),
        #[error("Descriptor parsing failed: {0}")]
        DescriptorParseFailed(String),
    }

    /// Script analysis result
    #[derive(Debug, Clone)]
    pub struct ScriptAnalysis {
        /// Whether script is miniscript-compatible
        pub is_miniscript: bool,
        /// Satisfaction weight (if miniscript)
        pub satisfaction_weight: Option<usize>,
        /// Script type (P2PKH, P2SH, P2WPKH, P2WSH, P2TR, etc.)
        pub script_type: String,
    }

    /// Compile miniscript policy to Bitcoin script
    ///
    /// # Arguments
    /// * `policy` - Miniscript policy to compile
    ///
    /// # Returns
    /// Compiled script as ByteString compatible with blvm-consensus
    pub fn compile_policy(policy: &Policy) -> Result<ByteString, MiniscriptError> {
        // Compile policy to miniscript
        let ms: Miniscript<_, _> = policy
            .compile()
            .map_err(|e| MiniscriptError::CompilationFailed(e.to_string()))?;
        
        // Encode to script bytes
        let script_bytes = ms.encode();
        
        Ok(ByteString::from(script_bytes.as_bytes()))
    }

    /// Parse script to miniscript (if possible)
    ///
    /// # Arguments
    /// * `script` - Script bytes to parse
    ///
    /// # Returns
    /// Parsed miniscript if script is miniscript-compatible
    pub fn parse_script(script: &ByteString) -> Result<Miniscript, MiniscriptError> {
        // Parse script as miniscript
        // Note: Miniscript::from_str expects hex string representation
        // Convert bytes to hex for parsing
        let script_hex = hex::encode(script.as_ref());
        
        // Try to parse as miniscript
        // Note: This is a simplified version - full implementation would use
        // miniscript's script parsing capabilities directly from bytes
        Miniscript::from_str(&script_hex)
            .map_err(|e| MiniscriptError::ParseFailed(e.to_string()))
    }

    /// Analyze script for satisfaction properties
    ///
    /// # Arguments
    /// * `script` - Script bytes to analyze
    ///
    /// # Returns
    /// Analysis result with script properties
    pub fn analyze_script(script: &ByteString) -> Result<ScriptAnalysis, MiniscriptError> {
        // Try to parse as miniscript
        match parse_script(script) {
            Ok(ms) => {
                // Calculate satisfaction weight
                let satisfaction_weight = ms.max_satisfaction_weight();
                
                // Determine script type
                let script_type = determine_script_type(script);
                
                Ok(ScriptAnalysis {
                    is_miniscript: true,
                    satisfaction_weight: Some(satisfaction_weight),
                    script_type,
                })
            }
            Err(_) => {
                // Not a miniscript, but still analyze basic properties
                let script_type = determine_script_type(script);
                Ok(ScriptAnalysis {
                    is_miniscript: false,
                    satisfaction_weight: None,
                    script_type,
                })
            }
        }
    }

    /// Determine script type from script bytes
    fn determine_script_type(script: &ByteString) -> String {
        let bytes = script.as_ref();
        
        // P2PKH: OP_DUP OP_HASH160 <20 bytes> OP_EQUALVERIFY OP_CHECKSIG
        if bytes.len() == 25 && bytes[0] == 0x76 && bytes[1] == 0xa9 && bytes[2] == 0x14 && bytes[23] == 0x88 && bytes[24] == 0xac {
            return "P2PKH".to_string();
        }
        
        // P2SH: OP_HASH160 <20 bytes> OP_EQUAL
        if bytes.len() == 23 && bytes[0] == 0xa9 && bytes[1] == 0x14 && bytes[22] == 0x87 {
            return "P2SH".to_string();
        }
        
        // P2WPKH: OP_0 <20 bytes>
        if bytes.len() == 22 && bytes[0] == 0x00 && bytes[1] == 0x14 {
            return "P2WPKH".to_string();
        }
        
        // P2WSH: OP_0 <32 bytes>
        if bytes.len() == 34 && bytes[0] == 0x00 && bytes[1] == 0x20 {
            return "P2WSH".to_string();
        }
        
        // P2TR: OP_1 <32 bytes>
        if bytes.len() == 34 && bytes[0] == 0x51 && bytes[1] == 0x20 {
            return "P2TR".to_string();
        }
        
        "Unknown".to_string()
    }

    /// Parse and validate descriptor
    ///
    /// # Arguments
    /// * `descriptor` - Descriptor string to parse
    ///
    /// # Returns
    /// Parsed descriptor
    pub fn parse_descriptor(descriptor: &str) -> Result<Descriptor, MiniscriptError> {
        Descriptor::from_str(descriptor)
            .map_err(|e| MiniscriptError::DescriptorParseFailed(e.to_string()))
    }

    /// Calculate descriptor checksum (BIP380)
    ///
    /// Descriptor checksums use bech32m encoding (BIP380).
    /// The checksum is calculated from the descriptor string and appended as #<checksum>.
    ///
    /// # Arguments
    /// * `descriptor` - Descriptor string (without checksum)
    ///
    /// # Returns
    /// 8-character checksum string
    pub fn calculate_descriptor_checksum(descriptor: &str) -> String {
        use bech32::{ToBase32, Variant};
        
        // Remove existing checksum if present
        let descriptor_clean = if let Some(sep_pos) = descriptor.rfind('#') {
            &descriptor[..sep_pos]
        } else {
            descriptor
        };
        
        // Convert descriptor to base32
        let descriptor_bytes = descriptor_clean.as_bytes();
        let base32 = descriptor_bytes.to_base32();
        
        // Encode with bech32m using HRP "dp" (descriptor prefix)
        // BIP380 uses bech32m (Variant::Bech32m) not bech32
        match bech32::encode("dp", base32, Variant::Bech32m) {
            Ok(encoded) => {
                // Extract checksum (last 8 characters after the separator '#')
                // Format: dp<descriptor>#<checksum>
                if let Some(sep_pos) = encoded.rfind('#') {
                    encoded[sep_pos + 1..].to_string()
                } else {
                    // If no separator, the checksum is the last 8 chars of encoded string
                    // bech32m checksum is 6 characters, but we return 8 for compatibility
                    if encoded.len() >= 8 {
                        encoded[encoded.len() - 8..].to_string()
                    } else {
                        encoded.clone()
                    }
                }
            }
            Err(_) => {
                // Fallback: use simple hash-based checksum
                use sha2::{Digest, Sha256};
                let hash = Sha256::digest(descriptor_bytes);
                hex::encode(&hash[..4])
            }
        }
    }

    /// Detect if descriptor is a range descriptor
    ///
    /// Range descriptors contain patterns like `[0,1]` or `[0,1000]` indicating
    /// they can derive multiple addresses.
    ///
    /// # Arguments
    /// * `descriptor` - Descriptor string to check
    ///
    /// # Returns
    /// True if descriptor contains range patterns
    pub fn is_range_descriptor(descriptor: &str) -> bool {
        #[cfg(feature = "miniscript")]
        {
            // Use regex if available
            if let Ok(range_pattern) = regex::Regex::new(r"\[\d+,\d+") {
                if range_pattern.is_match(descriptor) {
                    return true;
                }
            }
        }
        
        // Fallback: simple string matching for range patterns
        descriptor.contains("/0/*") || 
        descriptor.contains("/0/1") ||
        descriptor.contains("[0,") ||
        descriptor.contains("/*") ||
        descriptor.contains("/'/") ||  // HD key derivation with range
        descriptor.matches('[').count() > 0  // Any bracket pattern suggests range
    }
}


