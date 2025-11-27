//! Tests for validation utilities

use bllvm_node::utils::validation::{
    ensure, ensure_fmt, ensure_not_empty, ensure_range, ensure_some,
};

#[test]
fn test_ensure_success() {
    let result = ensure(true, "Should not error");
    assert!(result.is_ok());
}

#[test]
fn test_ensure_failure() {
    let result = ensure(false, "Should error");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "Should error");
}

#[test]
fn test_ensure_fmt_success() {
    let result = ensure_fmt(true, || "Should not error".to_string());
    assert!(result.is_ok());
}

#[test]
fn test_ensure_fmt_failure() {
    let value = 42;
    let result = ensure_fmt(false, || format!("Value {} must be positive", value));
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Value 42"));
}

#[test]
fn test_ensure_range_valid() {
    let result = ensure_range(5, 0, 10, "value");
    assert!(result.is_ok());
}

#[test]
fn test_ensure_range_too_low() {
    let result = ensure_range(-1, 0, 10, "value");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("between 0 and 10"));
}

#[test]
fn test_ensure_range_too_high() {
    let result = ensure_range(11, 0, 10, "value");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("between 0 and 10"));
}

#[test]
fn test_ensure_range_boundary() {
    // Test boundaries
    assert!(ensure_range(0, 0, 10, "value").is_ok());
    assert!(ensure_range(10, 0, 10, "value").is_ok());
}

#[test]
fn test_ensure_not_empty_valid() {
    let vec = vec![1, 2, 3];
    let result = ensure_not_empty(&vec, "items");
    assert!(result.is_ok());
}

#[test]
fn test_ensure_not_empty_invalid() {
    let vec: Vec<i32> = vec![];
    let result = ensure_not_empty(&vec, "items");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("items must not be empty"));
}

#[test]
fn test_ensure_some_valid() {
    let value = Some(42);
    let result = ensure_some(value, "value");
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
}

#[test]
fn test_ensure_some_invalid() {
    let value: Option<i32> = None;
    let result = ensure_some(value, "value");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("value must be set"));
}
