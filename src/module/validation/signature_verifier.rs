//! Module manifest/binary signature verification (shared by loader and manifest validator).

use std::path::Path;

use tracing::debug;

use crate::module::registry::manifest::ModuleManifest;
use crate::module::security::signing::ModuleSigner;
use crate::module::traits::ModuleError;

/// Verify module signatures when the manifest declares a `[signatures]` section.
pub fn verify_module_signatures(
    manifest: &ModuleManifest,
    binary_path: &Path,
) -> Result<(), ModuleError> {
    let signer = ModuleSigner::new();

    if let Some(_sig_section) = &manifest.signatures {
        let manifest_path = binary_path
            .parent()
            .ok_or_else(|| ModuleError::CryptoError("Binary path has no parent".to_string()))?
            .join("module.toml");

        let manifest_content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| ModuleError::CryptoError(format!("Failed to read manifest file: {e}")))?;

        let content_for_verification = manifest_content_without_signatures(&manifest_content)?;

        let signatures = manifest.get_signatures();
        let public_keys = manifest.get_public_keys();
        let threshold = manifest.get_threshold().ok_or_else(|| {
            ModuleError::CryptoError("Signature threshold not specified".to_string())
        })?;

        let valid = signer.verify_manifest(
            content_for_verification.as_bytes(),
            &signatures,
            &public_keys,
            threshold,
        )?;

        if !valid {
            return Err(ModuleError::CryptoError(format!(
                "Manifest signature verification failed for module {} (required {}-of-{})",
                manifest.name, threshold.0, threshold.1
            )));
        }

        debug!("Manifest signatures verified for module {}", manifest.name);
    }

    if let Some(binary_section) = &manifest.binary {
        if binary_path.exists() {
            let binary_content = std::fs::read(binary_path)
                .map_err(|e| ModuleError::CryptoError(format!("Failed to read binary: {e}")))?;

            if let Some(expected_hash) = &binary_section.hash {
                verify_stored_binary_hash(manifest, binary_path, expected_hash)?;
            }

            if manifest.signatures.is_some() {
                let signatures = manifest.get_signatures();
                let public_keys = manifest.get_public_keys();
                let threshold = manifest.get_threshold().ok_or_else(|| {
                    ModuleError::CryptoError("Signature threshold not specified".to_string())
                })?;

                let valid =
                    signer.verify_binary(&binary_content, &signatures, &public_keys, threshold)?;

                if !valid {
                    return Err(ModuleError::CryptoError(format!(
                        "Binary signature verification failed for module {} (required {}-of-{})",
                        manifest.name, threshold.0, threshold.1
                    )));
                }

                debug!("Binary signatures verified for module {}", manifest.name);
            }
        }
    }

    Ok(())
}

pub fn verify_stored_binary_hash(
    manifest: &ModuleManifest,
    binary_path: &Path,
    expected_hex: &str,
) -> Result<(), ModuleError> {
    let binary_content = std::fs::read(binary_path)
        .map_err(|e| ModuleError::CryptoError(format!("Failed to read binary: {e}")))?;
    use sha2::{Digest, Sha256};
    let actual_hash = hex::encode(Sha256::digest(&binary_content));
    let expected_hash = expected_hex.trim_start_matches("sha256:").to_lowercase();
    if actual_hash != expected_hash {
        return Err(ModuleError::CryptoError(format!(
            "Binary hash mismatch for module {}: expected {}, got {}",
            manifest.name, expected_hex, actual_hash
        )));
    }
    debug!("Binary hash verified for module {}", manifest.name);
    Ok(())
}

/// Strip the top-level `[signatures]` table for manifest signature verification.
///
/// Signatures are computed over canonical TOML without the `signatures` key.
pub fn manifest_content_without_signatures(content: &str) -> Result<String, ModuleError> {
    let mut value: toml::Value = toml::from_str(content)
        .map_err(|e| ModuleError::CryptoError(format!("Invalid manifest TOML: {e}")))?;
    if let toml::Value::Table(table) = &mut value {
        table.remove("signatures");
    }
    toml::to_string(&value).map_err(|e| {
        ModuleError::CryptoError(format!(
            "Failed to serialize manifest without signatures: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::manifest_content_without_signatures;

    #[test]
    fn strips_signatures_table_and_preserves_other_sections() {
        let input = r#"
name = "demo"
version = "1.0.0"
entry_point = "demo.so"

[signatures]
threshold = "1-of-1"

[[signatures.maintainers]]
name = "alice"
public_key = "02aa"
signature = "deadbeef"

[payment]
required = false
"#;
        let stripped = manifest_content_without_signatures(input).unwrap();
        let parsed: toml::Value = toml::from_str(&stripped).unwrap();
        let table = parsed.as_table().unwrap();
        assert!(!table.contains_key("signatures"));
        assert_eq!(table.get("name").unwrap().as_str(), Some("demo"));
        assert!(table.contains_key("payment"));
    }

    #[test]
    fn ignores_signatures_in_comments_not_top_level_table() {
        let input = r##"
name = "demo"
version = "1.0.0"
entry_point = "demo.so"
description = "# [signatures] not a section header"
"##;
        let stripped = manifest_content_without_signatures(input).unwrap();
        let parsed: toml::Value = toml::from_str(&stripped).unwrap();
        let table = parsed.as_table().unwrap();
        assert!(!table.contains_key("signatures"));
        assert_eq!(
            table.get("description").unwrap().as_str(),
            Some("# [signatures] not a section header")
        );
    }

    #[test]
    fn rejects_invalid_toml() {
        let err = manifest_content_without_signatures("name = [").unwrap_err();
        assert!(err.to_string().contains("Invalid manifest TOML"));
    }
}
