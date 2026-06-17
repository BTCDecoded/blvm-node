//! Regression tests for `NodeConfig::validate_security` warning strings.

use std::net::SocketAddr;

use blvm_node::config::{NodeConfig, RpcAuthConfig};

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("socket addr")
}

#[test]
fn validate_security_warns_rpc_non_loopback_without_auth() {
    let cfg = NodeConfig::default();
    let warns = cfg.validate_security(addr("0.0.0.0:8332"), None);
    assert_eq!(warns.len(), 1);
    assert!(warns[0].contains("RPC server is binding"));
    assert!(warns[0].contains("without authentication"));
}

#[test]
fn validate_security_warns_rpc_required_but_no_tokens_or_certs() {
    let mut cfg = NodeConfig::default();
    cfg.rpc_auth = Some(RpcAuthConfig {
        required: true,
        ..Default::default()
    });
    let warns = cfg.validate_security(addr("127.0.0.1:8332"), None);
    assert_eq!(warns.len(), 1);
    assert!(warns[0].contains("authentication is required"));
    assert!(warns[0].contains("no tokens or certificates"));
}

#[test]
fn validate_security_warns_rest_non_loopback_without_auth() {
    let cfg = NodeConfig::default();
    let warns = cfg.validate_security(addr("127.0.0.1:8332"), Some(addr("0.0.0.0:8080")));
    assert_eq!(warns.len(), 1);
    assert!(warns[0].contains("REST API is binding"));
    assert!(warns[0].contains("without authentication"));
}

#[test]
fn validate_security_warns_rest_non_loopback_when_auth_not_required() {
    let mut cfg = NodeConfig::default();
    cfg.rpc_auth = Some(RpcAuthConfig {
        required: false,
        ..Default::default()
    });
    let warns = cfg.validate_security(addr("127.0.0.1:8332"), Some(addr("0.0.0.0:8080")));
    assert_eq!(warns.len(), 1);
    assert!(warns[0].contains("REST API is binding"));
    assert!(warns[0].contains("authentication is not required"));
}

#[test]
fn validate_security_no_warnings_loopback_rpc_only() {
    let cfg = NodeConfig::default();
    assert!(
        cfg.validate_security(addr("127.0.0.1:8332"), None)
            .is_empty()
    );
}

#[test]
fn validate_security_no_warnings_ipv6_loopback_rpc_only() {
    let cfg = NodeConfig::default();
    assert!(cfg.validate_security(addr("[::1]:8332"), None).is_empty());
}
