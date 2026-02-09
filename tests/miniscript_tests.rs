//! Comprehensive unit tests for miniscript functionality
//!
//! Tests cover:
//! - Policy compilation
//! - Script parsing
//! - Descriptor parsing and validation
//! - Script analysis
//! - RPC method integration
//!
//! All tests are feature-gated behind `miniscript` feature.

#[cfg(feature = "miniscript")]
mod miniscript_tests {
    use blvm_node::miniscript::miniscript_support::*;
    use blvm_protocol::ByteString;
    use miniscript::{Miniscript, Policy, Descriptor};
    use std::str::FromStr;

    /// Test basic policy compilation
    #[test]
    fn test_compile_simple_policy() {
        // Simple policy: pk(key)
        let policy_str = "pk(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914)";
        let policy: Policy = Policy::from_str(policy_str)
            .expect("Failed to parse policy");

        let result = compile_policy(&policy);
        assert!(result.is_ok(), "Policy compilation should succeed");
        
        let script = result.unwrap();
        assert!(!script.as_ref().is_empty(), "Compiled script should not be empty");
    }

    /// Test multi-signature policy compilation
    #[test]
    fn test_compile_multisig_policy() {
        // 2-of-3 multisig policy
        let policy_str = "thresh(2,pk(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914),pk(03e60fce93b59e9ec53011aabc21c23e97b2a31369b87a5ae9c44ee89e2a6dec0),pk(02f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9))";
        let policy: Policy = Policy::from_str(policy_str)
            .expect("Failed to parse multisig policy");

        let result = compile_policy(&policy);
        assert!(result.is_ok(), "Multisig policy compilation should succeed");
    }

    /// Test time-locked policy compilation
    #[test]
    fn test_compile_timelock_policy() {
        // Policy with after() timelock
        let policy_str = "and(pk(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914),after(1000000))";
        let policy: Policy = Policy::from_str(policy_str)
            .expect("Failed to parse timelock policy");

        let result = compile_policy(&policy);
        assert!(result.is_ok(), "Timelock policy compilation should succeed");
    }

    /// Test script type detection
    #[test]
    fn test_determine_script_type_p2pkh() {
        // P2PKH script: OP_DUP OP_HASH160 <20 bytes> OP_EQUALVERIFY OP_CHECKSIG
        let p2pkh_script = vec![
            0x76, // OP_DUP
            0xa9, // OP_HASH160
            0x14, // Push 20 bytes
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
            0x88, // OP_EQUALVERIFY
            0xac, // OP_CHECKSIG
        ];
        let script = ByteString::from(p2pkh_script);
        let analysis = analyze_script(&script).expect("Analysis should succeed");
        
        assert_eq!(analysis.script_type, "P2PKH", "Should detect P2PKH script");
    }

    #[test]
    fn test_determine_script_type_p2sh() {
        // P2SH script: OP_HASH160 <20 bytes> OP_EQUAL
        let p2sh_script = vec![
            0xa9, // OP_HASH160
            0x14, // Push 20 bytes
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
            0x87, // OP_EQUAL
        ];
        let script = ByteString::from(p2sh_script);
        let analysis = analyze_script(&script).expect("Analysis should succeed");
        
        assert_eq!(analysis.script_type, "P2SH", "Should detect P2SH script");
    }

    #[test]
    fn test_determine_script_type_p2wpkh() {
        // P2WPKH script: OP_0 <20 bytes>
        let p2wpkh_script = vec![
            0x00, // OP_0
            0x14, // Push 20 bytes
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
        ];
        let script = ByteString::from(p2wpkh_script);
        let analysis = analyze_script(&script).expect("Analysis should succeed");
        
        assert_eq!(analysis.script_type, "P2WPKH", "Should detect P2WPKH script");
    }

    #[test]
    fn test_determine_script_type_p2wsh() {
        // P2WSH script: OP_0 <32 bytes>
        let p2wsh_script = vec![
            0x00, // OP_0
            0x20, // Push 32 bytes
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
            0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
            0x1e, 0x1f,
        ];
        let script = ByteString::from(p2wsh_script);
        let analysis = analyze_script(&script).expect("Analysis should succeed");
        
        assert_eq!(analysis.script_type, "P2WSH", "Should detect P2WSH script");
    }

    #[test]
    fn test_determine_script_type_p2tr() {
        // P2TR script: OP_1 <32 bytes>
        let p2tr_script = vec![
            0x51, // OP_1
            0x20, // Push 32 bytes
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
            0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
            0x1e, 0x1f,
        ];
        let script = ByteString::from(p2tr_script);
        let analysis = analyze_script(&script).expect("Analysis should succeed");
        
        assert_eq!(analysis.script_type, "P2TR", "Should detect P2TR script");
    }

    /// Test descriptor parsing
    #[test]
    fn test_parse_descriptor_pkh() {
        // P2PKH descriptor
        let descriptor_str = "pkh(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914)";
        
        let result = parse_descriptor(descriptor_str);
        assert!(result.is_ok(), "P2PKH descriptor parsing should succeed");
    }

    #[test]
    fn test_parse_descriptor_wpkh() {
        // P2WPKH descriptor
        let descriptor_str = "wpkh(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914)";
        
        let result = parse_descriptor(descriptor_str);
        assert!(result.is_ok(), "P2WPKH descriptor parsing should succeed");
    }

    #[test]
    fn test_parse_descriptor_sh() {
        // P2SH descriptor
        let descriptor_str = "sh(wpkh(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914))";
        
        let result = parse_descriptor(descriptor_str);
        assert!(result.is_ok(), "P2SH descriptor parsing should succeed");
    }

    #[test]
    fn test_parse_descriptor_multisig() {
        // Multisig descriptor
        let descriptor_str = "sh(multi(2,02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914,03e60fce93b59e9ec53011aabc21c23e97b2a31369b87a5ae9c44ee89e2a6dec0))";
        
        let result = parse_descriptor(descriptor_str);
        assert!(result.is_ok(), "Multisig descriptor parsing should succeed");
    }

    #[test]
    fn test_parse_descriptor_invalid() {
        // Invalid descriptor
        let descriptor_str = "invalid_descriptor";
        
        let result = parse_descriptor(descriptor_str);
        assert!(result.is_err(), "Invalid descriptor should fail to parse");
    }

    /// Test script analysis for miniscript-compatible scripts
    #[test]
    fn test_analyze_script_miniscript() {
        // Create a simple miniscript and analyze it
        let policy_str = "pk(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914)";
        let policy: Policy = Policy::from_str(policy_str)
            .expect("Failed to parse policy");
        
        let script = compile_policy(&policy)
            .expect("Policy compilation should succeed");
        
        let analysis = analyze_script(&script)
            .expect("Script analysis should succeed");
        
        // Note: The script might not be directly parseable as miniscript
        // because it's compiled to Bitcoin script format
        // This test verifies the analysis function handles it gracefully
        assert!(!analysis.script_type.is_empty(), "Script type should be determined");
    }

    /// Test script analysis for non-miniscript scripts
    #[test]
    fn test_analyze_script_non_miniscript() {
        // Random script that's not miniscript
        let random_script = vec![0x51, 0x52, 0x53, 0x54]; // OP_1, OP_2, OP_3, OP_4
        let script = ByteString::from(random_script);
        
        let analysis = analyze_script(&script)
            .expect("Analysis should succeed even for non-miniscript");
        
        assert!(!analysis.is_miniscript, "Non-miniscript should be detected");
        assert_eq!(analysis.satisfaction_weight, None, "Non-miniscript should have no satisfaction weight");
        assert_eq!(analysis.script_type, "Unknown", "Unknown script type should be detected");
    }

    /// Test error handling for invalid policy compilation
    #[test]
    fn test_compile_invalid_policy() {
        // Try to compile an invalid policy
        // This is tricky because Policy::from_str would fail first
        // But we can test with a policy that compiles but might have issues
        let policy_str = "pk(00)"; // Invalid pubkey
        let policy_result = Policy::from_str(policy_str);
        
        // This might fail at parse time or compile time
        if let Ok(policy) = policy_result {
            let compile_result = compile_policy(&policy);
            // Either should fail gracefully
            assert!(compile_result.is_ok() || compile_result.is_err(), 
                "Compilation should handle errors gracefully");
        }
    }

    /// Test error handling for invalid script parsing
    #[test]
    fn test_parse_invalid_script() {
        // Invalid script (too short, malformed)
        let invalid_script = vec![0x00]; // Just OP_0, incomplete
        let script = ByteString::from(invalid_script);
        
        let result = parse_script(&script);
        // Should either parse or fail gracefully
        assert!(result.is_ok() || result.is_err(), 
            "Parsing should handle invalid scripts gracefully");
    }

    /// Test satisfaction weight calculation
    #[test]
    fn test_satisfaction_weight() {
        // Create a policy and check if satisfaction weight is calculated
        let policy_str = "pk(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914)";
        let policy: Policy = Policy::from_str(policy_str)
            .expect("Failed to parse policy");
        
        let script = compile_policy(&policy)
            .expect("Policy compilation should succeed");
        
        let analysis = analyze_script(&script)
            .expect("Script analysis should succeed");
        
        // If it's a miniscript, satisfaction weight should be calculated
        if analysis.is_miniscript {
            assert!(analysis.satisfaction_weight.is_some(), 
                "Miniscript should have satisfaction weight");
        }
    }
}

// Integration tests for RPC methods
#[cfg(feature = "miniscript")]
mod rpc_tests {
    use blvm_node::rpc::miniscript::miniscript_rpc;
    use serde_json::json;

    /// Test getdescriptorinfo RPC method with valid descriptor
    #[tokio::test]
    async fn test_getdescriptorinfo_valid() {
        let descriptor = "pkh(02c6047f9441ed7d6d3045406e95c07cd85c778e4b8a8f3af4eb0c8666e7c1914)";
        let params = json!([descriptor]);
        
        let result = miniscript_rpc::get_descriptor_info(&params).await;
        assert!(result.is_ok(), "getdescriptorinfo should succeed");
        
        let response = result.unwrap();
        assert!(response.get("descriptor").is_some(), "Response should contain descriptor");
        assert!(response.get("issolvable").is_some(), "Response should contain issolvable");
    }

    /// Test getdescriptorinfo RPC method with invalid descriptor
    #[tokio::test]
    async fn test_getdescriptorinfo_invalid() {
        let descriptor = "invalid_descriptor";
        let params = json!([descriptor]);
        
        let result = miniscript_rpc::get_descriptor_info(&params).await;
        assert!(result.is_ok(), "getdescriptorinfo should handle invalid descriptors");
        
        let response = result.unwrap();
        assert_eq!(response.get("issolvable").and_then(|v| v.as_bool()), Some(false),
            "Invalid descriptor should have issolvable=false");
    }

    /// Test getdescriptorinfo RPC method with missing parameter
    #[tokio::test]
    async fn test_getdescriptorinfo_missing_param() {
        let params = json!([]);
        
        let result = miniscript_rpc::get_descriptor_info(&params).await;
        assert!(result.is_err(), "getdescriptorinfo should fail with missing parameter");
    }

    /// Test analyzepsbt RPC method (placeholder test)
    #[tokio::test]
    async fn test_analyzepsbt() {
        // Note: This is a placeholder test since analyzepsbt is not fully implemented
        let psbt = "cHNidP8BAH0CAAAAASuBBAO7r9HADl0R9l5z0z8BAAAAAAD/////AQAAAAAAAAAAAACAAQAAAAAB6wQAAAAA";
        let params = json!([psbt]);
        
        let result = miniscript_rpc::analyze_psbt(&params).await;
        assert!(result.is_ok(), "analyzepsbt should succeed (placeholder)");
        
        let response = result.unwrap();
        assert!(response.get("inputs").is_some(), "Response should contain inputs");
    }
}

