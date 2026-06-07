//! RPC auth log redaction helpers.

use blvm_node::rpc::auth::{redact_auth_header, redact_tokens_from_log};

#[test]
fn test_redact_tokens_from_log() {
    let msg = "RPC failed: token=abc123secret end";
    let out = redact_tokens_from_log(msg, &["abc123secret".to_string()]);
    assert!(!out.contains("abc123secret"));
    assert!(out.contains("[REDACTED]"));
}

#[test]
fn test_redact_tokens_bearer_format() {
    let msg = "Header: Bearer mysecrettoken";
    let out = redact_tokens_from_log(msg, &["mysecrettoken".to_string()]);
    assert!(out.contains("Bearer [REDACTED]"));
}

#[test]
fn test_redact_auth_header() {
    let msg = "Authorization: Bearer deadbeef\nContent-Type: json";
    let out = redact_auth_header(msg);
    assert!(out.contains("Authorization: Bearer [REDACTED]"));
    assert!(!out.contains("deadbeef"));
}
