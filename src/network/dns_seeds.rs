//! DNS Seed Resolution for Peer Discovery
//!
//! Resolves Bitcoin DNS seeds to discover initial peer addresses.
//! Based on Bitcoin Core's DNS seed mechanism.

use crate::network::protocol::NetworkAddress;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::lookup_host;
use tracing::{info, warn};

/// Bitcoin DNS seeds (mainnet)
/// These are well-known DNS servers that return Bitcoin node addresses
pub const MAINNET_DNS_SEEDS: &[&str] = &[
    "seed.bitcoin.sipa.be",
    "dnsseed.bluematt.me",
    "dnsseed.bitcoin.dashjr.org",
    "seed.bitcoinstats.com",
    "seed.bitcoin.jonasschnelli.ch",
    "seed.btc.petertodd.org",
    "seed.bitcoin.sprovoost.nl",
    "dnsseed.emzy.de",
    "seed.bitcoin.wiz.biz",
];

/// Testnet DNS seeds
pub const TESTNET_DNS_SEEDS: &[&str] = &[
    "testnet-seed.bitcoin.jonasschnelli.ch",
    "seed.tbtc.petertodd.org",
    "seed.testnet.bitcoin.sprovoost.nl",
    "testnet-seed.bluematt.me",
];

/// Resolve DNS seeds to peer addresses
pub async fn resolve_dns_seeds(
    seeds: &[&str],
    port: u16,
    max_addresses: usize,
) -> Vec<NetworkAddress> {
    let mut addresses = Vec::new();

    for seed in seeds {
        match resolve_dns_seed(seed, port).await {
            Ok(mut addrs) => {
                info!("Resolved {} addresses from DNS seed: {}", addrs.len(), seed);
                addresses.append(&mut addrs);
                if addresses.len() >= max_addresses {
                    break;
                }
            }
            Err(e) => {
                warn!("Failed to resolve DNS seed {}: {}", seed, e);
            }
        }
    }

    // Limit to max_addresses
    addresses.truncate(max_addresses);
    addresses
}

/// Resolve a single DNS seed
async fn resolve_dns_seed(seed: &str, port: u16) -> Result<Vec<NetworkAddress>, String> {
    // Create hostname:port string for DNS lookup
    let hostname = format!("{}:{}", seed, port);

    // Perform DNS lookup with timeout
    let timeout = Duration::from_secs(5);
    let lookup_result = tokio::time::timeout(timeout, lookup_host(&hostname))
        .await
        .map_err(|_| format!("DNS lookup timeout for {}", seed))?;

    let socket_addrs =
        lookup_result.map_err(|e| format!("DNS lookup failed for {}: {}", seed, e))?;

    // Convert SocketAddr to NetworkAddress
    let mut addresses = Vec::new();
    for socket_addr in socket_addrs {
        addresses.push(socket_addr_to_network_address(socket_addr));
    }

    Ok(addresses)
}

/// Convert SocketAddr to NetworkAddress
fn socket_addr_to_network_address(socket_addr: SocketAddr) -> NetworkAddress {
    use std::net::IpAddr;

    let ip_bytes = match socket_addr.ip() {
        IpAddr::V4(ipv4) => {
            // IPv4-mapped IPv6 format
            let mut bytes = [0u8; 16];
            bytes[10] = 0xff;
            bytes[11] = 0xff;
            bytes[12..16].copy_from_slice(&ipv4.octets());
            bytes
        }
        IpAddr::V6(ipv6) => ipv6.octets(),
    };

    NetworkAddress {
        services: 0, // Will be updated when we connect
        ip: ip_bytes,
        port: socket_addr.port(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_socket_addr_to_network_address() {
        let socket = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            8333,
        );
        let addr = socket_addr_to_network_address(socket);
        assert_eq!(addr.port, 8333);
        // Check IPv4-mapped format
        assert_eq!(addr.ip[10], 0xff);
        assert_eq!(addr.ip[11], 0xff);
        assert_eq!(addr.ip[12], 127);
        assert_eq!(addr.ip[13], 0);
        assert_eq!(addr.ip[14], 0);
        assert_eq!(addr.ip[15], 1);
    }
}
