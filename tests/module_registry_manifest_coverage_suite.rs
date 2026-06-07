//! Module manifest signature/threshold helpers.

use blvm_node::module::registry::manifest::{
    MaintainerSignature, ModuleManifest, SignatureSection,
};
use std::collections::HashMap;

fn manifest_with_threshold(threshold: &str) -> ModuleManifest {
    ModuleManifest {
        name: "sig-mod".into(),
        version: "1.0.0".into(),
        description: None,
        author: None,
        capabilities: vec![],
        rpc_overrides: vec![],
        dependencies: HashMap::new(),
        optional_dependencies: HashMap::new(),
        entry_point: "sig-mod.so".into(),
        config_schema: HashMap::new(),
        binary: None,
        downloads: HashMap::new(),
        signatures: Some(SignatureSection {
            threshold: Some(threshold.into()),
            maintainers: vec![MaintainerSignature {
                name: "alice".into(),
                public_key: "02abc".into(),
                signature: "deadbeef".into(),
            }],
        }),
        payment: None,
    }
}

#[test]
fn manifest_parse_threshold_valid_and_invalid() {
    assert_eq!(ModuleManifest::parse_threshold("2-of-3"), Some((2, 3)));
    assert_eq!(ModuleManifest::parse_threshold("0-of-3"), None);
    assert_eq!(ModuleManifest::parse_threshold("4-of-3"), None);
    assert_eq!(ModuleManifest::parse_threshold("bad"), None);
}

#[test]
fn manifest_get_threshold_and_signature_helpers() {
    let m = manifest_with_threshold("2-of-3");
    assert_eq!(m.get_threshold(), Some((2, 3)));
    assert!(m.has_signatures());
    let sigs = m.get_signatures();
    assert_eq!(sigs.len(), 1);
    assert_eq!(sigs[0].0, "alice");
    let keys = m.get_public_keys();
    assert_eq!(keys.get("alice").map(String::as_str), Some("02abc"));
}

#[test]
fn manifest_without_signatures_returns_none() {
    let mut m = manifest_with_threshold("1-of-1");
    m.signatures = None;
    assert!(!m.has_signatures());
    assert!(m.get_threshold().is_none());
    assert!(m.get_signatures().is_empty());
}
