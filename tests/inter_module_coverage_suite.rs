//! Module API registry and inter-module router smoke.

use async_trait::async_trait;
use blvm_node::module::inter_module::api::ModuleAPI;
use blvm_node::module::inter_module::registry::ModuleApiRegistry;
use blvm_node::module::inter_module::router::ModuleRouter;
use blvm_node::module::traits::ModuleError;
use std::sync::Arc;

struct EchoApi;

#[async_trait]
impl ModuleAPI for EchoApi {
    async fn handle_request(
        &self,
        method: &str,
        params: &[u8],
        caller_module_id: &str,
    ) -> Result<Vec<u8>, ModuleError> {
        let base = method.rsplit("::").next().unwrap_or(method);
        if base != "echo" {
            return Err(ModuleError::OperationError(format!(
                "unknown method {method}"
            )));
        }
        Ok(format!("{caller_module_id}:{}", String::from_utf8_lossy(params)).into_bytes())
    }

    fn list_methods(&self) -> Vec<String> {
        vec!["echo".into()]
    }

    fn api_version(&self) -> u32 {
        1
    }
}

#[tokio::test]
async fn registry_register_route_and_call() {
    let registry = Arc::new(ModuleApiRegistry::new());
    registry
        .register_api("svc_abc".into(), Arc::new(EchoApi))
        .await
        .unwrap();

    assert_eq!(registry.list_modules().await, vec!["svc_abc".to_string()]);
    assert_eq!(
        registry.get_module_methods("svc_abc").await,
        Some(vec!["echo".to_string()])
    );
    assert_eq!(
        registry.route_method("echo").await,
        Some(("svc_abc".to_string(), "echo".to_string()))
    );
    assert_eq!(
        registry.resolve_module_id("svc").await,
        Some("svc_abc".into())
    );

    let api = registry.get_api("svc_abc").await.unwrap();
    let out = api.handle_request("echo", b"hi", "caller").await.unwrap();
    assert_eq!(out, b"caller:hi");

    registry.unregister_api("svc_abc").await.unwrap();
    assert!(registry.get_api("svc_abc").await.is_none());
}

#[tokio::test]
async fn router_direct_call_and_method_routing() {
    let registry = Arc::new(ModuleApiRegistry::new());
    registry
        .register_api("svc_xyz".into(), Arc::new(EchoApi))
        .await
        .unwrap();
    let router = ModuleRouter::new(Arc::clone(&registry));

    let direct = router
        .route_call("caller", Some("svc_xyz"), "echo", b"one")
        .await
        .unwrap();
    assert_eq!(direct, b"caller:one");

    let routed = router
        .route_call("caller", None, "echo", b"two")
        .await
        .unwrap();
    assert_eq!(routed, b"caller:two");

    let missing = router.route_call("caller", None, "missing", b"").await;
    assert!(missing.is_err());
}
