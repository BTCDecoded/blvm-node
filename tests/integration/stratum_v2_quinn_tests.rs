//! Integration tests for Stratum V2 with Quinn transport

#[cfg(all(feature = "stratum-v2", feature = "quinn"))]
use anyhow::Result;
#[cfg(all(feature = "stratum-v2", feature = "quinn"))]
use bllvm_node::network::stratum_v2::client::StratumV2Client;

#[cfg(all(feature = "stratum-v2", feature = "quinn"))]
#[tokio::test]
async fn test_stratum_v2_quinn_url_parsing() -> Result<()> {
    // Test Quinn URL parsing
    let quinn_url = "quinn://127.0.0.1:3333";
    let client = StratumV2Client::new(quinn_url.to_string());
    
    // Client should be created with Quinn transport type
    // (actual connection will require a running server)
    
    Ok(())
}

#[cfg(all(feature = "stratum-v2", feature = "quinn"))]
#[tokio::test]
async fn test_stratum_v2_url_formats() {
    // Test different URL formats
    let tcp_url = "tcp://127.0.0.1:3333";
    let quinn_url = "quinn://127.0.0.1:3333";
    let iroh_url = "iroh://0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    
    let _tcp_client = StratumV2Client::new(tcp_url.to_string());
    let _quinn_client = StratumV2Client::new(quinn_url.to_string());
    
    #[cfg(feature = "iroh")]
    let _iroh_client = StratumV2Client::new(iroh_url.to_string());
}

