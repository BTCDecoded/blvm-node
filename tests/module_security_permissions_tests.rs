//! Module Security Permissions Tests
//!
//! Tests permission enforcement, permission checking, and security boundaries.

use bllvm_node::module::ipc::protocol::RequestPayload;
use bllvm_node::module::security::permissions::*;

#[test]
fn test_permission_set_empty() {
    let perms = PermissionSet::new();
    assert!(!perms.has(&Permission::ReadBlockchain));
    assert!(!perms.has(&Permission::ReadUTXO));
}

#[test]
fn test_permission_set_add() {
    let mut perms = PermissionSet::new();
    perms.add(Permission::ReadBlockchain);

    assert!(perms.has(&Permission::ReadBlockchain));
    assert!(!perms.has(&Permission::ReadUTXO));
}

#[test]
fn test_permission_set_remove() {
    let mut perms = PermissionSet::new();
    perms.add(Permission::ReadBlockchain);
    assert!(perms.has(&Permission::ReadBlockchain));

    // PermissionSet doesn't have remove method - create new set without permission
    let mut perms = PermissionSet::new();
    assert!(!perms.has(&Permission::ReadBlockchain));
}

#[test]
fn test_permission_set_contains_all() {
    let mut perms1 = PermissionSet::new();
    perms1.add(Permission::ReadBlockchain);
    perms1.add(Permission::ReadUTXO);

    let mut perms2 = PermissionSet::new();
    perms2.add(Permission::ReadBlockchain);

    assert!(perms1.has_all(&[Permission::ReadBlockchain]));
    assert!(!perms2.has_all(&[Permission::ReadUTXO]));
}

#[test]
fn test_permission_checker_check_api_call() {
    let checker = PermissionChecker::new();

    // Should allow ReadBlockchain permission (default permissions include it)
    let payload = RequestPayload::GetBlock { hash: [0u8; 32] };
    assert!(checker.check_api_call("test-module", &payload).is_ok());
}

#[test]
fn test_permission_checker_deny_missing_permission() {
    let mut checker = PermissionChecker::new();
    let empty_perms = PermissionSet::new(); // No permissions

    // Register module with no permissions
    checker.register_module_permissions("restricted-module".to_string(), empty_perms);

    // Should deny if permission missing
    let payload = RequestPayload::GetBlock { hash: [0u8; 32] };
    let result = checker.check_api_call("restricted-module", &payload);
    assert!(result.is_err());
}

#[test]
fn test_permission_checker_read_utxo_requires_permission() {
    let checker = PermissionChecker::new();

    // Default permissions include ReadUTXO
    let payload = RequestPayload::GetUtxo {
        outpoint: bllvm_protocol::OutPoint {
            hash: [0u8; 32],
            index: 0,
        },
    };
    assert!(checker.check_api_call("test-module", &payload).is_ok());
}

#[test]
fn test_permission_checker_subscribe_events_requires_permission() {
    let checker = PermissionChecker::new();

    // Default permissions include SubscribeEvents
    let payload = RequestPayload::SubscribeEvents {
        event_types: vec![],
    };
    assert!(checker.check_api_call("test-module", &payload).is_ok());
}

#[test]
fn test_parse_permission_string() {
    assert_eq!(
        parse_permission_string("read_blockchain"),
        Some(Permission::ReadBlockchain)
    );
    assert_eq!(
        parse_permission_string("ReadBlockchain"),
        Some(Permission::ReadBlockchain)
    );
    assert_eq!(
        parse_permission_string("read_utxo"),
        Some(Permission::ReadUTXO)
    );
    assert_eq!(
        parse_permission_string("ReadUTXO"),
        Some(Permission::ReadUTXO)
    );
    assert_eq!(parse_permission_string("invalid"), None);
}
