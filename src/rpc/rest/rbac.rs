//! REST RBAC — map REST routes to JSON-RPC method names for admin checks.

use crate::rpc::server::admin_rpc_methods;
use hyper::Method;

/// JSON-RPC method equivalent for a REST path, when known.
pub(crate) fn rest_equivalent_rpc_method(method: Method, path: &str) -> Option<&'static str> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 3 || parts[0] != "api" || parts[1] != "v1" {
        return None;
    }

    match parts[2] {
        "chain" => match (method, parts.get(3).copied()) {
            (Method::POST, Some("verify")) => Some("verifychain"),
            (Method::POST, Some("prune")) => Some("pruneblockchain"),
            _ => None,
        },
        "blocks" => {
            if method == Method::POST && parts.len() >= 5 {
                match parts.get(4).copied() {
                    Some("invalidate") => Some("invalidateblock"),
                    Some("reconsider") => Some("reconsiderblock"),
                    _ => None,
                }
            } else {
                None
            }
        }
        "transactions" if method == Method::POST => match parts.get(3).copied() {
            None if parts.len() == 3 => Some("sendrawtransaction"),
            Some("test") => Some("testmempoolaccept"),
            Some("decode") => Some("decoderawtransaction"),
            Some("create") => Some("createrawtransaction"),
            _ => None,
        },
        "mempool" if method == Method::POST => match parts.get(3).copied() {
            Some("save") => Some("savemempool"),
            Some("transactions") if parts.get(5) == Some(&"priority") => {
                Some("prioritisetransaction")
            }
            _ => None,
        },
        "network" => match method {
            Method::POST => match parts.get(3).copied() {
                Some("ping") => Some("ping"),
                Some("nodes") => Some("addnode"),
                Some("active") => Some("setnetworkactive"),
                Some("bans") => Some("setban"),
                _ => None,
            },
            Method::DELETE => match parts.get(3).copied() {
                Some("nodes") => Some("disconnectnode"),
                Some("bans") => Some("clearbanned"),
                _ => None,
            },
            _ => None,
        },
        "node" => match (method, parts.get(3).copied()) {
            (Method::POST, Some("stop")) => Some("stop"),
            (Method::POST, Some("logging")) => Some("logging"),
            _ => None,
        },
        "mining" => match (method, parts.get(3).copied()) {
            (Method::GET, Some("block-template")) => Some("getblocktemplate"),
            (Method::POST, Some("blocks")) => Some("submitblock"),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the REST route requires an admin token (aligned with JSON-RPC RBAC).
///
/// Unmapped POST/DELETE paths fail closed (admin required). Unmapped GET paths are
/// allowed for authenticated non-admin callers.
pub(crate) fn rest_requires_admin(method: &Method, path: &str) -> bool {
    if let Some(rpc_method) = rest_equivalent_rpc_method(method.clone(), path) {
        return admin_rpc_methods().contains(rpc_method);
    }
    matches!(*method, Method::POST | Method::DELETE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getblocktemplate_requires_admin() {
        assert!(rest_requires_admin(
            &Method::GET,
            "/api/v1/mining/block-template"
        ));
        assert_eq!(
            rest_equivalent_rpc_method(Method::GET, "/api/v1/mining/block-template"),
            Some("getblocktemplate")
        );
    }

    #[test]
    fn decode_and_test_mempool_do_not_require_admin() {
        assert!(!rest_requires_admin(
            &Method::POST,
            "/api/v1/transactions/decode"
        ));
        assert!(!rest_requires_admin(
            &Method::POST,
            "/api/v1/transactions/test"
        ));
    }

    #[test]
    fn destructive_writes_require_admin() {
        assert!(rest_requires_admin(&Method::POST, "/api/v1/transactions"));
        assert!(rest_requires_admin(&Method::POST, "/api/v1/node/stop"));
        assert!(rest_requires_admin(
            &Method::DELETE,
            "/api/v1/network/nodes/127.0.0.1:8333"
        ));
    }

    #[test]
    fn verifychain_does_not_require_admin() {
        assert!(!rest_requires_admin(&Method::POST, "/api/v1/chain/verify"));
    }

    #[test]
    fn prioritisetransaction_requires_admin() {
        assert!(rest_requires_admin(
            &Method::POST,
            "/api/v1/mempool/transactions/0000000000000000000000000000000000000000000000000000000000000000/priority"
        ));
    }

    #[test]
    fn unmapped_payment_post_fails_closed() {
        assert!(rest_requires_admin(&Method::POST, "/api/v1/payments"));
    }
}
