//! Parse `commons_get_coinbase_outputs` for GBT.
//!
//! Shared by NodeAPI and `MiningRpc` so hold / script rules cannot drift.

use crate::module::traits::ModuleError;
use serde_json::Value;

/// Parse `commons_get_coinbase_outputs`. `None` = do not use Commons.
/// Holding is an error so GBT does not issue a node-default coinbase.
pub fn commons_gbt_outputs(v: &Value) -> Result<Option<Vec<(i64, Vec<u8>)>>, ModuleError> {
    if v.get("issue_work").and_then(|x| x.as_bool()) == Some(false) {
        return Err(ModuleError::OperationError(
            "commons pool holding; not issuing work".into(),
        ));
    }
    let Some(arr) = v.get("outputs").and_then(|x| x.as_array()) else {
        return Ok(None);
    };
    if arr.is_empty() {
        return Ok(None);
    }
    let mut outs = Vec::with_capacity(arr.len());
    for o in arr {
        let value = o
            .get("value_sats")
            .or_else(|| o.get("value"))
            .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|n| n.max(0) as u64)))
            .ok_or_else(|| ModuleError::OperationError("commons output missing value".into()))?;
        let script = hex::decode(
            o.get("script")
                .or_else(|| o.get("script_pubkey"))
                .and_then(|x| x.as_str())
                .ok_or_else(|| {
                    ModuleError::OperationError("commons output missing script".into())
                })?,
        )
        .map_err(|e| ModuleError::OperationError(format!("commons script: {e}")))?;
        outs.push((value as i64, script));
    }
    Ok(Some(outs))
}
