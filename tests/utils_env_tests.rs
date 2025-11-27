//! Environment variable utility tests
//!
//! Tests for environment variable reading helpers.

use bllvm_node::utils::env::{env_bool, env_int, env_opt, env_or_default, env_or_else};

#[test]
fn test_env_or_default_set() {
    std::env::set_var("TEST_ENV_VAR", "test_value");

    let value = env_or_default("TEST_ENV_VAR", "default");

    assert_eq!(value, "test_value");

    std::env::remove_var("TEST_ENV_VAR");
}

#[test]
fn test_env_or_default_unset() {
    std::env::remove_var("TEST_ENV_VAR_UNSET");

    let value = env_or_default("TEST_ENV_VAR_UNSET", "default_value");

    assert_eq!(value, "default_value");
}

#[test]
fn test_env_or_else_set() {
    std::env::set_var("TEST_ENV_VAR2", "test_value");

    let value = env_or_else("TEST_ENV_VAR2", || "computed_default".to_string());

    assert_eq!(value, "test_value");

    std::env::remove_var("TEST_ENV_VAR2");
}

#[test]
fn test_env_or_else_unset() {
    std::env::remove_var("TEST_ENV_VAR2_UNSET");

    let value = env_or_else("TEST_ENV_VAR2_UNSET", || "computed_default".to_string());

    assert_eq!(value, "computed_default");
}

#[test]
fn test_env_opt_set() {
    std::env::set_var("TEST_ENV_VAR3", "test_value");

    let value = env_opt("TEST_ENV_VAR3");

    assert_eq!(value, Some("test_value".to_string()));

    std::env::remove_var("TEST_ENV_VAR3");
}

#[test]
fn test_env_opt_unset() {
    std::env::remove_var("TEST_ENV_VAR3_UNSET");

    let value = env_opt("TEST_ENV_VAR3_UNSET");

    assert_eq!(value, None);
}

#[test]
fn test_env_bool_true() {
    for true_value in &["true", "TRUE", "True", "1", "yes", "YES", "on", "ON"] {
        std::env::set_var("TEST_ENV_BOOL", true_value);
        assert!(
            env_bool("TEST_ENV_BOOL"),
            "Should be true for '{}'",
            true_value
        );
    }
    std::env::remove_var("TEST_ENV_BOOL");
}

#[test]
fn test_env_bool_false() {
    for false_value in &["false", "FALSE", "0", "no", "NO", "off", "OFF", "invalid"] {
        std::env::set_var("TEST_ENV_BOOL", false_value);
        assert!(
            !env_bool("TEST_ENV_BOOL"),
            "Should be false for '{}'",
            false_value
        );
    }
    std::env::remove_var("TEST_ENV_BOOL");
}

#[test]
fn test_env_bool_unset() {
    std::env::remove_var("TEST_ENV_BOOL_UNSET");

    assert!(!env_bool("TEST_ENV_BOOL_UNSET"));
}

#[test]
fn test_env_int_valid() {
    std::env::set_var("TEST_ENV_INT", "42");

    let value: Option<i32> = env_int("TEST_ENV_INT");

    assert_eq!(value, Some(42));

    std::env::remove_var("TEST_ENV_INT");
}

#[test]
fn test_env_int_invalid() {
    std::env::set_var("TEST_ENV_INT", "not_a_number");

    let value: Option<i32> = env_int("TEST_ENV_INT");

    assert_eq!(value, None);

    std::env::remove_var("TEST_ENV_INT");
}

#[test]
fn test_env_int_unset() {
    std::env::remove_var("TEST_ENV_INT_UNSET");

    let value: Option<i32> = env_int("TEST_ENV_INT_UNSET");

    assert_eq!(value, None);
}

#[test]
fn test_env_int_different_types() {
    std::env::set_var("TEST_ENV_INT", "100");

    let i32_val: Option<i32> = env_int("TEST_ENV_INT");
    let u64_val: Option<u64> = env_int("TEST_ENV_INT");
    let i64_val: Option<i64> = env_int("TEST_ENV_INT");

    assert_eq!(i32_val, Some(100));
    assert_eq!(u64_val, Some(100));
    assert_eq!(i64_val, Some(100));

    std::env::remove_var("TEST_ENV_INT");
}
