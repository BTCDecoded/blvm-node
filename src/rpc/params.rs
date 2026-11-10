//! RPC parameter parsing helpers
//!
//! Single place for reading JSON-RPC params by index and type, so error messages
//! and bounds checks stay consistent.

use crate::rpc::errors::RpcError;
use serde_json::Value;

/// Get string at `params[index]`, or `None` if missing/wrong type.
#[inline]
pub fn param_str(params: &Value, index: usize) -> Option<&str> {
    params.get(index).and_then(|p| p.as_str())
}

/// Get string at `params[index]`, or error with consistent message for the method.
pub fn param_str_required(
    params: &Value,
    index: usize,
    method_name: &str,
) -> Result<String, RpcError> {
    param_str(params, index).map(String::from).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "Expected string at params[{index}] for {method_name}"
        ))
    })
}

/// Get u64 at `params[index]`, or `None` if missing/wrong type.
#[inline]
pub fn param_u64(params: &Value, index: usize) -> Option<u64> {
    params.get(index).and_then(|p| p.as_u64())
}

/// Get u64 at `params[index]`, or `default` if missing/wrong type.
#[inline]
pub fn param_u64_default(params: &Value, index: usize, default: u64) -> u64 {
    param_u64(params, index).unwrap_or(default)
}

/// Get u64 at `params[index]`, or error with a consistent message for the method.
pub fn param_u64_required(
    params: &Value,
    index: usize,
    method_name: &str,
) -> Result<u64, RpcError> {
    param_u64(params, index).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "Expected non-negative integer at params[{index}] for {method_name}"
        ))
    })
}

/// Get bool at `params[index]`, or `None` if missing/wrong type.
#[inline]
pub fn param_bool(params: &Value, index: usize) -> Option<bool> {
    params.get(index).and_then(|p| p.as_bool())
}

/// Get bool at `params[index]`, or `default` if missing/wrong type.
#[inline]
pub fn param_bool_default(params: &Value, index: usize, default: bool) -> bool {
    param_bool(params, index).unwrap_or(default)
}

/// Get f64 at `params[index]`, or `None` if missing/wrong type.
#[inline]
pub fn param_f64(params: &Value, index: usize) -> Option<f64> {
    params.get(index).and_then(|p| p.as_f64())
}

/// Get array at `params[index]`, or `None` if missing/wrong type.
#[inline]
pub fn param_array(params: &Value, index: usize) -> Option<&Vec<Value>> {
    params.get(index).and_then(|p| p.as_array())
}
