//! RPC Input Validation Utilities
//!
//! Provides helper functions for validating RPC method parameters,
//! including string length limits, numeric bounds, and format validation.

use crate::rpc::errors::RpcError;
use serde_json::Value;

/// Maximum string length for hex-encoded data (e.g., transaction hex)
pub const MAX_HEX_STRING_LENGTH: usize = 2_000_000; // ~1MB transaction max

/// Maximum string length for hash strings (64 hex chars for 32-byte hash)
pub const MAX_HASH_STRING_LENGTH: usize = 64;

/// Maximum string length for addresses
pub const MAX_ADDRESS_STRING_LENGTH: usize = 200;

/// Maximum string length for general string parameters
pub const MAX_STRING_LENGTH: usize = 10_000;

/// Maximum numeric value for block height
pub const MAX_BLOCK_HEIGHT: u64 = 2_000_000_000; // Far beyond current Bitcoin height

/// Maximum numeric value for confirmation count
pub const MAX_CONFIRMATIONS: u64 = 1_000_000;

/// Maximum numeric value for fee rate (satoshis per byte)
pub const MAX_FEE_RATE: u64 = 1_000_000_000; // 10 BTC per byte (extremely high)

/// Validate and extract a string parameter
pub fn validate_string_param(
    params: &Value,
    index: usize,
    param_name: &str,
    max_length: Option<usize>,
) -> Result<String, RpcError> {
    let value = params
        .get(index)
        .and_then(|p| p.as_str())
        .ok_or_else(|| RpcError::invalid_params(format!("Missing {param_name} parameter")))?;

    let max_len = max_length.unwrap_or(MAX_STRING_LENGTH);
    if value.len() > max_len {
        return Err(RpcError::invalid_params(format!(
            "{} parameter too long: {} bytes (max: {})",
            param_name,
            value.len(),
            max_len
        )));
    }

    Ok(value.to_string())
}

/// Validate and extract a hex string parameter
pub fn validate_hex_string_param(
    params: &Value,
    index: usize,
    param_name: &str,
    max_length: Option<usize>,
) -> Result<String, RpcError> {
    let hex_string = validate_string_param(params, index, param_name, max_length)?;

    // Validate hex format (even length, valid hex chars)
    if hex_string.len() % 2 != 0 {
        return Err(RpcError::invalid_params(format!(
            "{} must be even-length hex string",
            param_name
        )));
    }

    if !hex_string.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(RpcError::invalid_params(format!(
            "{} contains invalid hex characters",
            param_name
        )));
    }

    Ok(hex_string)
}

/// Validate and extract a hash string parameter (64 hex chars)
pub fn validate_hash_param(
    params: &Value,
    index: usize,
    param_name: &str,
) -> Result<String, RpcError> {
    let hash = validate_hex_string_param(params, index, param_name, Some(MAX_HASH_STRING_LENGTH))?;

    if hash.len() != 64 {
        return Err(RpcError::invalid_params(format!(
            "{} must be 64 hex characters (32 bytes), got {}",
            param_name,
            hash.len()
        )));
    }

    Ok(hash)
}

/// Validate and extract a numeric parameter with bounds
pub fn validate_numeric_param<T>(
    params: &Value,
    index: usize,
    param_name: &str,
    min: Option<T>,
    max: Option<T>,
) -> Result<T, RpcError>
where
    T: TryFrom<u64> + PartialOrd + std::fmt::Display,
    <T as TryFrom<u64>>::Error: std::fmt::Display,
{
    let value = params
        .get(index)
        .and_then(|p| p.as_u64())
        .ok_or_else(|| RpcError::invalid_params(format!("Missing {param_name} parameter")))?;

    let typed_value = T::try_from(value).map_err(|e| {
        RpcError::invalid_params(format!("Invalid {param_name} value: {value} ({e})"))
    })?;

    if let Some(min_val) = min {
        if typed_value < min_val {
            return Err(RpcError::invalid_params(format!(
                "{param_name} must be >= {min_val}, got {typed_value}"
            )));
        }
    }

    if let Some(max_val) = max {
        if typed_value > max_val {
            return Err(RpcError::invalid_params(format!(
                "{param_name} must be <= {max_val}, got {typed_value}"
            )));
        }
    }

    Ok(typed_value)
}

/// Validate and extract an optional numeric parameter with bounds
pub fn validate_optional_numeric_param<T>(
    params: &Value,
    index: usize,
    param_name: &str,
    default: T,
    min: Option<T>,
    max: Option<T>,
) -> Result<T, RpcError>
where
    T: TryFrom<u64> + PartialOrd + std::fmt::Display + Copy,
    <T as TryFrom<u64>>::Error: std::fmt::Display,
{
    if let Some(value) = params.get(index).and_then(|p| p.as_u64()) {
        // Validate the extracted value against bounds
        let typed_value = T::try_from(value)
            .map_err(|e| RpcError::invalid_params(format!("Invalid {param_name}: {e}")))?;

        if let Some(min_val) = min {
            if typed_value < min_val {
                return Err(RpcError::invalid_params(format!(
                    "{param_name} must be >= {min_val}"
                )));
            }
        }

        if let Some(max_val) = max {
            if typed_value > max_val {
                return Err(RpcError::invalid_params(format!(
                    "{param_name} must be <= {max_val}"
                )));
            }
        }

        Ok(typed_value)
    } else {
        Ok(default)
    }
}

/// Validate and extract a boolean parameter
pub fn validate_bool_param(
    params: &Value,
    index: usize,
    param_name: &str,
) -> Result<bool, RpcError> {
    params
        .get(index)
        .and_then(|p| p.as_bool())
        .ok_or_else(|| RpcError::invalid_params(format!("Missing {param_name} parameter")))
}

/// Validate and extract an optional boolean parameter
pub fn validate_optional_bool_param(params: &Value, index: usize, default: bool) -> bool {
    params
        .get(index)
        .and_then(|p| p.as_bool())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_validate_string_param() {
        let params = json!(["test"]);
        assert_eq!(
            validate_string_param(&params, 0, "test", None).unwrap(),
            "test"
        );
    }

    #[test]
    fn test_validate_string_param_too_long() {
        let long_string = "a".repeat(MAX_STRING_LENGTH + 1);
        let params = json!([long_string]);
        assert!(validate_string_param(&params, 0, "test", None).is_err());
    }

    #[test]
    fn test_validate_hex_string_param() {
        let params = json!(["deadbeef"]);
        assert_eq!(
            validate_hex_string_param(&params, 0, "hex", None).unwrap(),
            "deadbeef"
        );
    }

    #[test]
    fn test_validate_hex_string_param_invalid() {
        let params = json!(["nothex!"]);
        assert!(validate_hex_string_param(&params, 0, "hex", None).is_err());
    }

    #[test]
    fn test_validate_hash_param() {
        let hash = "0".repeat(64);
        let params = json!([hash]);
        assert_eq!(validate_hash_param(&params, 0, "hash").unwrap(), hash);
    }

    #[test]
    fn test_validate_hash_param_wrong_length() {
        let params = json!(["deadbeef"]);
        assert!(validate_hash_param(&params, 0, "hash").is_err());
    }

    #[test]
    fn test_validate_numeric_param() {
        let params = json!([100]);
        assert_eq!(
            validate_numeric_param::<u64>(&params, 0, "value", Some(0), Some(1000)).unwrap(),
            100
        );
    }

    #[test]
    fn test_validate_numeric_param_out_of_bounds() {
        let params = json!([2000]);
        assert!(validate_numeric_param::<u64>(&params, 0, "value", Some(0), Some(1000)).is_err());
    }
}
