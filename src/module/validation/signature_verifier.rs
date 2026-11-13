//! Module manifest/binary signature verification (shared by loader and manifest validator).

use std::path::Path;

use tracing::debug;

use crate::module::registry::manifest::ModuleManifest;
use crate::module::traits::ModuleError;

/// Verify a declared binary hash. A `[signatures]` table is not a publisher.
///
/// The public keys live in the same manifest. Accepting them would let the
/// module satisfy the signed-module gate with a key it just generated.
/// `modules.allow_unsigned_modules` remains the operator switch for a module
/// that has no signature section.
pub fn verify_module_signatures(
    manifest: &ModuleManifest,
    binary_path: &Path,
) -> Result<(), ModuleError> {
    if let Some(expected_hash) = manifest
        .binary
        .as_ref()
        .and_then(|section| section.hash.as_deref())
    {
        if binary_path.exists() {
            verify_stored_binary_hash(manifest, binary_path, expected_hash)?;
        }
    }
    if manifest.signatures.is_some() {
        return Err(ModuleError::CryptoError(
            "module signature keys in the manifest are not a trust anchor".to_string(),
        ));
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
    use super::{manifest_content_without_signatures, verify_module_signatures};
    use std::path::Path;

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

    #[test]
    fn embedded_signature_key_is_not_a_trust_anchor() {
        use crate::module::registry::manifest::ModuleManifest;

        let manifest: ModuleManifest = toml::from_str(
            r#"
name = "evil"
version = "0.0.1"
entry_point = "evil"

[signatures]
threshold = "1-of-1"

[[signatures.maintainers]]
name = "attacker"
public_key = "02"
signature = "00"
"#,
        )
        .unwrap();
        let err = verify_module_signatures(&manifest, Path::new("missing-binary")).unwrap_err();
        assert!(err.to_string().contains("not a trust anchor"), "{err}");
    }
}
