//! Local network (LAN) Bitcoin node discovery
//!
//! Automatically discovers sibling Bitcoin nodes on the local network by:
//! 1. Detecting local network interfaces
//! 2. Scanning the local subnet for port 8333 (Bitcoin P2P)
//! 3. Returning discovered nodes for priority connection
//!
//! This enables massive IBD speedups when a local Bitcoin Core or other
//! Bitcoin node is available on the LAN (e.g., Start9, Umbrel, RaspiBlitz).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Default Bitcoin P2P port
const BITCOIN_P2P_PORT: u16 = 8333;

/// Connection timeout for port scanning (milliseconds)
const SCAN_TIMEOUT_MS: u64 = 100;

/// Maximum number of concurrent scans
const MAX_CONCURRENT_SCANS: usize = 64;

/// Get local network interfaces and their IPv4 addresses
/// 
/// Returns a list of (interface_ip, subnet_mask) tuples for private networks only
fn get_local_interfaces() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut interfaces = Vec::new();
    
    // Use the local-ip-address crate to get interface IPs
    if let Ok(local_ip) = local_ip_address::local_ip() {
        if let IpAddr::V4(ipv4) = local_ip {
            if ipv4.is_private() {
                // Assume /24 subnet (most common for home networks)
                let mask = Ipv4Addr::new(255, 255, 255, 0);
                interfaces.push((ipv4, mask));
                info!("Detected local interface: {}/24", ipv4);
            }
        }
    }
    
    // Also check for common Docker/VM bridge networks
    // These might have sibling nodes running in containers
    for prefix in &[
        Ipv4Addr::new(172, 17, 0, 1),   // Docker default bridge
        Ipv4Addr::new(10, 0, 0, 1),     // Common VM network
    ] {
        if !interfaces.iter().any(|(ip, _)| ip == prefix) {
            // Only add if we can actually bind to this range
            // (indicates we're on this network)
            if let Ok(stream) = TcpStream::connect_timeout(
                &SocketAddr::new(IpAddr::V4(*prefix), 1),
                Duration::from_millis(10),
            ) {
                drop(stream);
                let mask = Ipv4Addr::new(255, 255, 255, 0);
                interfaces.push((*prefix, mask));
            }
        }
    }
    
    interfaces
}

/// Generate all IPs in a /24 subnet
fn generate_subnet_ips(base_ip: Ipv4Addr, _mask: Ipv4Addr) -> Vec<Ipv4Addr> {
    let octets = base_ip.octets();
    let mut ips = Vec::with_capacity(254);
    
    // Generate .1 to .254 (skip .0 network and .255 broadcast)
    for last_octet in 1..=254 {
        let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], last_octet);
        // Skip our own IP
        if ip != base_ip {
            ips.push(ip);
        }
    }
    
    ips
}

/// Check if a host has Bitcoin P2P port open
fn check_bitcoin_port(ip: Ipv4Addr, port: u16, timeout: Duration) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(ip), port);
    TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Discover Bitcoin nodes on the local network
/// 
/// Scans the local subnet for hosts with port 8333 open.
/// Returns a list of discovered Bitcoin node addresses.
/// 
/// This is called during IBD startup to find local sibling nodes.
pub async fn discover_lan_bitcoin_nodes() -> Vec<SocketAddr> {
    discover_lan_bitcoin_nodes_with_port(BITCOIN_P2P_PORT).await
}

/// Discover Bitcoin nodes on the local network with custom port
pub async fn discover_lan_bitcoin_nodes_with_port(port: u16) -> Vec<SocketAddr> {
    let interfaces = get_local_interfaces();
    
    if interfaces.is_empty() {
        debug!("No local network interfaces found for LAN discovery");
        return vec![];
    }
    
    info!("Starting LAN Bitcoin node discovery on {} interface(s)", interfaces.len());
    
    let mut discovered = Vec::new();
    let timeout = Duration::from_millis(SCAN_TIMEOUT_MS);
    
    for (base_ip, mask) in interfaces {
        let subnet_ips = generate_subnet_ips(base_ip, mask);
        info!("Scanning {} IPs on {}/24 subnet for port {}", subnet_ips.len(), base_ip, port);
        
        // Parallel scanning using tokio spawn_blocking for TCP connects
        let mut handles = Vec::new();
        
        for chunk in subnet_ips.chunks(MAX_CONCURRENT_SCANS) {
            let chunk_ips: Vec<Ipv4Addr> = chunk.to_vec();
            let port = port;
            let timeout = timeout;
            
            let handle = tokio::task::spawn_blocking(move || {
                let mut found = Vec::new();
                for ip in chunk_ips {
                    if check_bitcoin_port(ip, port, timeout) {
                        found.push(SocketAddr::new(IpAddr::V4(ip), port));
                    }
                }
                found
            });
            
            handles.push(handle);
        }
        
        // Collect results
        for handle in handles {
            if let Ok(found) = handle.await {
                discovered.extend(found);
            }
        }
    }
    
    if discovered.is_empty() {
        info!("No LAN Bitcoin nodes discovered");
    } else {
        info!(
            "Discovered {} LAN Bitcoin node(s): {:?}",
            discovered.len(),
            discovered
        );
    }
    
    discovered
}

/// Quick check for a specific IP (used for manual testing or known hosts)
pub fn quick_check_bitcoin_node(ip: Ipv4Addr, port: u16) -> bool {
    check_bitcoin_port(ip, port, Duration::from_millis(SCAN_TIMEOUT_MS * 5))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // Subnet IP Generation Tests
    // ============================================================

    #[test]
    fn test_generate_subnet_ips() {
        let base = Ipv4Addr::new(192, 168, 1, 100);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        let ips = generate_subnet_ips(base, mask);
        
        // Should have 253 IPs (1-254 minus our own)
        assert_eq!(ips.len(), 253);
        
        // Should not include our own IP
        assert!(!ips.contains(&base));
        
        // Should include .1 and .254
        assert!(ips.contains(&Ipv4Addr::new(192, 168, 1, 1)));
        assert!(ips.contains(&Ipv4Addr::new(192, 168, 1, 254)));
    }

    #[test]
    fn test_generate_subnet_ips_edge_cases() {
        // Test with .1 as base (common gateway)
        let base = Ipv4Addr::new(192, 168, 2, 1);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        let ips = generate_subnet_ips(base, mask);
        
        assert_eq!(ips.len(), 253);
        assert!(!ips.contains(&base));
        assert!(ips.contains(&Ipv4Addr::new(192, 168, 2, 100))); // Start9 address
        assert!(ips.contains(&Ipv4Addr::new(192, 168, 2, 254)));
        
        // Test with .254 as base
        let base = Ipv4Addr::new(10, 0, 0, 254);
        let ips = generate_subnet_ips(base, mask);
        assert_eq!(ips.len(), 253);
        assert!(!ips.contains(&base));
        assert!(ips.contains(&Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn test_generate_subnet_excludes_network_and_broadcast() {
        let base = Ipv4Addr::new(192, 168, 1, 100);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        let ips = generate_subnet_ips(base, mask);
        
        // .0 (network) and .255 (broadcast) should never be included
        assert!(!ips.contains(&Ipv4Addr::new(192, 168, 1, 0)));
        assert!(!ips.contains(&Ipv4Addr::new(192, 168, 1, 255)));
    }

    // ============================================================
    // Private IP Detection Tests
    // ============================================================

    #[test]
    fn test_is_private_ip() {
        // Private ranges should be detected
        assert!(Ipv4Addr::new(10, 0, 0, 1).is_private());
        assert!(Ipv4Addr::new(172, 16, 0, 1).is_private());
        assert!(Ipv4Addr::new(192, 168, 1, 1).is_private());
        
        // Public IPs should not be
        assert!(!Ipv4Addr::new(8, 8, 8, 8).is_private());
        assert!(!Ipv4Addr::new(1, 1, 1, 1).is_private());
    }

    #[test]
    fn test_private_ip_10_range() {
        // 10.0.0.0/8 - Full Class A private range
        assert!(Ipv4Addr::new(10, 0, 0, 0).is_private());
        assert!(Ipv4Addr::new(10, 0, 0, 1).is_private());
        assert!(Ipv4Addr::new(10, 255, 255, 254).is_private());
        assert!(Ipv4Addr::new(10, 255, 255, 255).is_private());
        assert!(Ipv4Addr::new(10, 123, 45, 67).is_private());
    }

    #[test]
    fn test_private_ip_172_range() {
        // 172.16.0.0/12 - Partial Class B private (172.16-31.x.x only)
        assert!(Ipv4Addr::new(172, 16, 0, 1).is_private());
        assert!(Ipv4Addr::new(172, 31, 255, 254).is_private());
        assert!(Ipv4Addr::new(172, 20, 5, 10).is_private());
        
        // 172.15.x.x and 172.32.x.x should NOT be private
        assert!(!Ipv4Addr::new(172, 15, 255, 255).is_private());
        assert!(!Ipv4Addr::new(172, 32, 0, 1).is_private());
        assert!(!Ipv4Addr::new(172, 0, 0, 1).is_private());
    }

    #[test]
    fn test_private_ip_192_168_range() {
        // 192.168.0.0/16 - Most common home network
        assert!(Ipv4Addr::new(192, 168, 0, 1).is_private());
        assert!(Ipv4Addr::new(192, 168, 1, 1).is_private());
        assert!(Ipv4Addr::new(192, 168, 2, 100).is_private()); // Our Start9!
        assert!(Ipv4Addr::new(192, 168, 255, 254).is_private());
        
        // 192.169.x.x should NOT be private
        assert!(!Ipv4Addr::new(192, 169, 0, 1).is_private());
        assert!(!Ipv4Addr::new(192, 167, 0, 1).is_private());
    }

    #[test]
    fn test_common_public_ips_not_private() {
        // Well-known public IPs
        assert!(!Ipv4Addr::new(8, 8, 8, 8).is_private());      // Google DNS
        assert!(!Ipv4Addr::new(8, 8, 4, 4).is_private());      // Google DNS
        assert!(!Ipv4Addr::new(1, 1, 1, 1).is_private());      // Cloudflare
        assert!(!Ipv4Addr::new(208, 67, 222, 222).is_private()); // OpenDNS
        assert!(!Ipv4Addr::new(45, 33, 20, 159).is_private()); // Random public
        assert!(!Ipv4Addr::new(173, 46, 87, 0).is_private());  // Random public
    }

    // ============================================================
    // Bitcoin Port Check Tests (unit-level, no network)
    // ============================================================

    #[test]
    fn test_bitcoin_p2p_port_constant() {
        assert_eq!(BITCOIN_P2P_PORT, 8333, "Default Bitcoin P2P port should be 8333");
    }

    #[test]
    fn test_scan_timeout_reasonable() {
        // Timeout should be fast for LAN scanning but not instant
        assert!(SCAN_TIMEOUT_MS >= 50, "Scan timeout should be at least 50ms");
        assert!(SCAN_TIMEOUT_MS <= 500, "Scan timeout should be at most 500ms for responsiveness");
    }

    #[test]
    fn test_max_concurrent_scans_reasonable() {
        // Should scan multiple IPs concurrently for speed
        assert!(MAX_CONCURRENT_SCANS >= 16, "Should scan at least 16 IPs concurrently");
        assert!(MAX_CONCURRENT_SCANS <= 256, "Should not overload the system");
    }

    // ============================================================
    // Integration Test Helpers (can be run manually)
    // ============================================================

    #[test]
    fn test_quick_check_function_exists() {
        // Just verify the function compiles and has expected signature
        // Can't actually test without a running Bitcoin node
        // In CI, this will return false (no node running), which is fine
        // We just verify the function exists and doesn't panic
        let result = quick_check_bitcoin_node(Ipv4Addr::new(127, 0, 0, 1), 8333);
        // Result will be false in CI (no Bitcoin node), true if node exists
        // Either way, we just verify the function works without panicking
        let _ = result; // Suppress unused warning
    }

    #[test]
    fn test_socket_addr_construction() {
        // Verify we can construct valid SocketAddrs for discovered nodes
        let ip = Ipv4Addr::new(192, 168, 2, 100);
        let port = 8333u16;
        let addr = SocketAddr::new(IpAddr::V4(ip), port);
        
        assert_eq!(addr.port(), 8333);
        assert_eq!(addr.ip(), IpAddr::V4(ip));
    }
}

