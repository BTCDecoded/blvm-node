//! Control / utility RPC smoke tests.

use blvm_node::rpc::control::ControlRpc;
use serde_json::json;

#[tokio::test]
async fn test_uptime_and_memory_info() {
    let rpc = ControlRpc::new();
    let uptime = rpc.uptime(&json!([])).await.unwrap();
    assert!(uptime.is_number());
    let mem = rpc.getmemoryinfo(&json!(["stats"])).await.unwrap();
    assert!(mem.is_object());
}

#[tokio::test]
async fn test_getrpcinfo_help_and_logging() {
    let rpc = ControlRpc::new();
    let info = rpc.getrpcinfo(&json!([])).await.unwrap();
    assert!(info.get("active_commands").is_some());
    let help = rpc.help(&json!([])).await.unwrap();
    assert!(help.is_string());
    let logging = rpc.logging(&json!([])).await.unwrap();
    assert!(logging.get("active").is_some());
}

#[tokio::test]
async fn test_gethealth_and_getmetrics() {
    let rpc = ControlRpc::new();
    let health = rpc.gethealth(&json!([])).await.unwrap();
    assert_eq!(health.get("status").unwrap().as_str(), Some("healthy"));
    let metrics = rpc.getmetrics(&json!([])).await.unwrap();
    assert!(metrics.get("uptime_seconds").unwrap().is_u64());
}

#[tokio::test]
async fn test_listmodules_without_manager_errors() {
    let rpc = ControlRpc::new();
    assert!(rpc.listmodules(&json!([])).await.is_err());
}

#[tokio::test]
async fn test_loadmodule_missing_params_errors() {
    let rpc = ControlRpc::new();
    assert!(rpc.loadmodule(&json!([])).await.is_err());
}

#[tokio::test]
async fn test_runmodulecli_without_manager_errors() {
    let rpc = ControlRpc::new();
    assert!(rpc.runmodulecli(&json!(["hello", "ping"])).await.is_err());
}

#[tokio::test]
async fn test_help_specific_command_and_unknown() {
    let rpc = ControlRpc::new();
    let stop_help = rpc.help(&json!(["stop"])).await.unwrap();
    assert!(stop_help.as_str().unwrap().contains("Stop Bitcoin node"));
    assert!(rpc.help(&json!(["not-a-real-rpc"])).await.is_err());
    let list = rpc.help(&json!([])).await.unwrap();
    assert!(list.as_str().unwrap().contains("getblockcount"));
}

#[tokio::test]
async fn test_getmemoryinfo_mallocinfo_compat_and_logging_include() {
    let rpc = ControlRpc::new();
    let malloc = rpc.getmemoryinfo(&json!(["mallocinfo"])).await.unwrap();
    assert!(malloc.as_str().is_some());
    let logging = rpc
        .logging(&json!([["net", "rpc"], ["http"]]))
        .await
        .unwrap();
    assert_eq!(logging.get("active").unwrap().as_bool(), Some(true));
}

#[tokio::test]
async fn test_unload_and_reload_module_missing_params() {
    let rpc = ControlRpc::new();
    assert!(rpc.unloadmodule(&json!([])).await.is_err());
    assert!(rpc.reloadmodule(&json!([])).await.is_err());
}

#[tokio::test]
async fn test_getmoduleclispecs_without_manager_errors() {
    let rpc = ControlRpc::new();
    assert!(rpc.getmoduleclispecs(&json!([])).await.is_err());
}
