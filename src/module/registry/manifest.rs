//! Module manifest parsing and validation
//!
//! Handles parsing module.toml manifests and validating module metadata.

use crate::module::traits::{ModuleError, ModuleMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Maintainer signature information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintainerSignature {
    /// Maintainer name/identifier
    pub name: String,
    /// Public key in hex format
    pub public_key: String,
    /// Signature in hex format
    pub signature: String,
}

/// Signature section in manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureSection {
    /// List of maintainer signatures
    #[serde(default)]
    pub maintainers: Vec<MaintainerSignature>,
    /// Signature threshold (e.g., "2-of-3" means 2 out of 3 maintainers must sign)
    #[serde(default)]
    pub threshold: Option<String>,
}

/// Binary information section
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinarySection {
    /// SHA256 hash of the binary in hex format
    #[serde(default)]
    pub hash: Option<String>,
    /// Binary size in bytes
    #[serde(default)]
    pub size: Option<u64>,
}

/// Payment configuration section (cryptographically signed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentSection {
    /// Whether payment is required for this module
    #[serde(default)]
    pub required: bool,
    /// Price in satoshis (if payment required)
    #[serde(default)]
    pub price_sats: Option<u64>,
    /// Module author's payment code (BIP47) or address
    /// If payment_code is provided, generates unique address per payment (privacy-preserving)
    /// If address is provided, uses fixed address (legacy, less private)
    /// This receives 75% of payment
    #[serde(default)]
    pub author_payment_code: Option<String>,
    /// Module author's payment address (legacy, for backward compatibility)
    /// DEPRECATED: Use author_payment_code instead to avoid address reuse
    #[serde(default)]
    pub author_address: Option<String>,
    /// Commons governance payment code (BIP47) or address
    /// If payment_code is provided, generates unique address per payment (privacy-preserving)
    /// If address is provided, uses fixed address (legacy, less private)
    /// This receives 15% of payment
    #[serde(default)]
    pub commons_payment_code: Option<String>,
    /// Commons governance payment address (legacy, for backward compatibility)
    /// DEPRECATED: Use commons_payment_code instead to avoid address reuse
    #[serde(default)]
    pub commons_address: Option<String>,
    /// Signature over payment section (payment_codes/addresses + price_sats)
    /// Signed by module maintainers using the same keys as manifest signatures
    #[serde(default)]
    pub payment_signature: Option<String>,
}

/// Module manifest (module.toml structure)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleManifest {
    /// Module name
    pub name: String,
    /// Module version (semantic versioning)
    pub version: String,
    /// Human-readable description
    pub description: Option<String>,
    /// Module author
    pub author: Option<String>,
    /// Capabilities this module declares it can use
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Required dependencies (module names with versions)
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    /// Module entry point (binary name or path)
    pub entry_point: String,
    /// Module configuration schema (optional)
    #[serde(default)]
    pub config_schema: HashMap<String, String>,
    /// Signature section (optional - for signed modules)
    #[serde(default)]
    pub signatures: Option<SignatureSection>,
    /// Binary information (optional - for integrity verification)
    #[serde(default)]
    pub binary: Option<BinarySection>,
    /// Payment configuration (optional - for paid modules)
    /// Contains cryptographically signed payment addresses
    #[serde(default)]
    pub payment: Option<PaymentSection>,
}

impl ModuleManifest {
    /// Load manifest from file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ModuleError> {
        let contents = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            ModuleError::InvalidManifest(format!("Failed to read manifest file: {e}"))
        })?;

        let manifest: ModuleManifest = toml::from_str(&contents).map_err(|e| {
            ModuleError::InvalidManifest(format!("Failed to parse manifest TOML: {e}"))
        })?;

        // Validate required fields
        if manifest.name.is_empty() {
            return Err(ModuleError::InvalidManifest(
                "Module name cannot be empty".to_string(),
            ));
        }
        if manifest.entry_point.is_empty() {
            return Err(ModuleError::InvalidManifest(
                "Entry point cannot be empty".to_string(),
            ));
        }

        Ok(manifest)
    }

    /// Convert to ModuleMetadata
    pub fn to_metadata(&self) -> ModuleMetadata {
        ModuleMetadata {
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone().unwrap_or_default(),
            author: self.author.clone().unwrap_or_default(),
            capabilities: self.capabilities.clone(),
            dependencies: self.dependencies.clone(),
            entry_point: self.entry_point.clone(),
        }
    }

    /// Get signature threshold as (required, total) tuple
    ///
    /// Parses threshold string like "2-of-3" into (2, 3).
    /// Returns None if threshold is not set or cannot be parsed.
    pub fn get_threshold(&self) -> Option<(usize, usize)> {
        let threshold_str = self.signatures.as_ref()?.threshold.as_ref()?;
        Self::parse_threshold(threshold_str)
    }

    /// Parse threshold string like "2-of-3" into (2, 3)
    pub fn parse_threshold(threshold_str: &str) -> Option<(usize, usize)> {
        let parts: Vec<&str> = threshold_str.split("-of-").collect();
        if parts.len() != 2 {
            return None;
        }
        let required = parts[0].parse().ok()?;
        let total = parts[1].parse().ok()?;
        if required > total || required == 0 {
            return None;
        }
        Some((required, total))
    }

    /// Get signatures as (maintainer, signature_hex) pairs
    pub fn get_signatures(&self) -> Vec<(String, String)> {
        self.signatures
            .as_ref()
            .map(|s| {
                s.maintainers
                    .iter()
                    .map(|m| (m.name.clone(), m.signature.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get public keys as (maintainer, pubkey_hex) map
    pub fn get_public_keys(&self) -> HashMap<String, String> {
        self.signatures
            .as_ref()
            .map(|s| {
                s.maintainers
                    .iter()
                    .map(|m| (m.name.clone(), m.public_key.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Check if manifest has signatures
    pub fn has_signatures(&self) -> bool {
        self.signatures.is_some()
            && self
                .signatures
                .as_ref()
                .map(|s| !s.maintainers.is_empty())
                .unwrap_or(false)
    }
}

impl TryFrom<ModuleManifest> for ModuleMetadata {
    type Error = ModuleError;

    fn try_from(manifest: ModuleManifest) -> Result<Self, Self::Error> {
        Ok(manifest.to_metadata())
    }
}
