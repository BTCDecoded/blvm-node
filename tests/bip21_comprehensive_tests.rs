//! BIP21 Comprehensive Tests
//!
//! Tests for BIP21 URI parsing and OS-level registration.

use bllvm_node::bip21::{Bip21Error, BitcoinUri};

#[test]
fn test_bip21_uri_parsing_basic() {
    // Basic URI with just address
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
    assert_eq!(parsed.amount, None);
    assert_eq!(parsed.label, None);
    assert_eq!(parsed.message, None);
}

#[test]
fn test_bip21_uri_parsing_with_amount() {
    // URI with amount
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
    assert_eq!(parsed.amount, Some(0.01));
}

#[test]
fn test_bip21_uri_parsing_with_label() {
    // URI with label
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?label=Test%20Payment";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
    assert_eq!(parsed.label, Some("Test Payment".to_string()));
}

#[test]
fn test_bip21_uri_parsing_with_message() {
    // URI with message
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?message=Payment%20for%20services";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
    assert_eq!(parsed.message, Some("Payment for services".to_string()));
}

#[test]
fn test_bip21_uri_parsing_all_parameters() {
    // URI with all parameters
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01&label=Test&message=Payment";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
    assert_eq!(parsed.amount, Some(0.01));
    assert_eq!(parsed.label, Some("Test".to_string()));
    assert_eq!(parsed.message, Some("Payment".to_string()));
}

#[test]
fn test_bip21_uri_parsing_invalid_scheme() {
    // Invalid scheme
    let uri = "http://example.com";
    let result = BitcoinUri::parse(uri);

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), Bip21Error::InvalidScheme));
}

#[test]
fn test_bip21_uri_parsing_missing_address() {
    // Missing address
    let uri = "bitcoin:";
    let result = BitcoinUri::parse(uri);

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), Bip21Error::MissingAddress));
}

#[test]
fn test_bip21_uri_parsing_invalid_amount() {
    // Invalid amount
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=invalid";
    let result = BitcoinUri::parse(uri);

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), Bip21Error::InvalidAmount));
}

#[test]
fn test_bip21_uri_parsing_extra_parameters() {
    // URI with extra parameters
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01&custom=value";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
    assert_eq!(parsed.amount, Some(0.01));
    assert_eq!(parsed.params.get("custom"), Some(&"value".to_string()));
}

#[test]
fn test_bip21_uri_parsing_url_encoding() {
    // Test URL encoding/decoding
    let uri =
        "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?label=Hello%20World&message=Test%2FMessage";
    let parsed = BitcoinUri::parse(uri).unwrap();

    assert_eq!(parsed.label, Some("Hello World".to_string()));
    assert_eq!(parsed.message, Some("Test/Message".to_string()));
}

#[test]
fn test_bip21_uri_serialization() {
    // Test serialization
    let uri = BitcoinUri {
        address: "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa".to_string(),
        amount: Some(0.01),
        label: Some("Test".to_string()),
        message: None,
        params: std::collections::HashMap::new(),
    };

    let json = serde_json::to_string(&uri).unwrap();
    let deserialized: BitcoinUri = serde_json::from_str(&json).unwrap();

    assert_eq!(uri.address, deserialized.address);
    assert_eq!(uri.amount, deserialized.amount);
    assert_eq!(uri.label, deserialized.label);
}

#[test]
fn test_bip21_uri_edge_cases() {
    // Empty query string
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?";
    let parsed = BitcoinUri::parse(uri).unwrap();
    assert_eq!(parsed.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");

    // Multiple question marks (should use first one)
    let uri = "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01?invalid=param";
    let parsed = BitcoinUri::parse(uri).unwrap();
    assert_eq!(parsed.amount, Some(0.01));
}
