//! Option utility tests
//!
//! Tests for Option helper functions.

use bllvm_node::utils::option::{
    map_or_default, option_to_result, or_else, unwrap_or_default_with,
};

#[test]
fn test_unwrap_or_default_with_some() {
    let opt = Some(42);

    let value = unwrap_or_default_with(opt, || 0);

    assert_eq!(value, 42);
}

#[test]
fn test_unwrap_or_default_with_none() {
    let opt: Option<i32> = None;

    let value = unwrap_or_default_with(opt, || 100);

    assert_eq!(value, 100);
}

#[test]
fn test_option_to_result_some() {
    let opt = Some(42);

    let result: Result<i32, String> = option_to_result(opt, "Value not found");

    assert_eq!(result.unwrap(), 42);
}

#[test]
fn test_option_to_result_none() {
    let opt: Option<i32> = None;

    let result: Result<i32, String> = option_to_result(opt, "Value not found");

    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "Value not found");
}

#[test]
fn test_map_or_default_some() {
    let opt = Some(10);

    let result = map_or_default(opt, |v| v * 2, || 0);

    assert_eq!(result, 20);
}

#[test]
fn test_map_or_default_none() {
    let opt: Option<i32> = None;

    let result = map_or_default(opt, |v| v * 2, || 0);

    assert_eq!(result, 0);
}

#[test]
fn test_or_else_some() {
    let opt1 = Some(42);

    let result = or_else(opt1, || Some(100));

    assert_eq!(result, Some(42));
}

#[test]
fn test_or_else_none() {
    let opt1: Option<i32> = None;

    let result = or_else(opt1, || Some(100));

    assert_eq!(result, Some(100));
}

#[test]
fn test_or_else_both_none() {
    let opt1: Option<i32> = None;

    let result = or_else(opt1, || None::<i32>);

    assert_eq!(result, None);
}
