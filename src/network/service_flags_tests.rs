//! Tests for service flags and peer capability detection

#[cfg(test)]
mod tests {
    use super::super::protocol::VersionMessage;
    use super::super::protocol::{NODE_UTXO_COMMITMENTS, NODE_BAN_LIST_SHARING, NODE_PACKAGE_RELAY, NODE_FIBRE};
    use bllvm_protocol::bip157::NODE_COMPACT_FILTERS;
    use crate::network::protocol::NetworkAddress;

    fn create_version_message(services: u64) -> VersionMessage {
        VersionMessage {
            version: 70015,
            services,
            timestamp: 1234567890,
            addr_recv: NetworkAddress {
                services: 0,
                ip: [0; 16],
                port: 8333,
            },
            addr_from: NetworkAddress {
                services: 0,
                ip: [0; 16],
                port: 8333,
            },
            nonce: 12345,
            user_agent: "/test-node:0.1.0/".to_string(),
            start_height: 0,
            relay: true,
        }
    }

    #[test]
    fn test_utxo_commitments_flag() {
        #[cfg(feature = "utxo-commitments")]
        {
            // Peer with UTXO commitments support
            let services = NODE_UTXO_COMMITMENTS;
            let version = create_version_message(services);
            assert!(version.supports_utxo_commitments(), "Should support UTXO commitments");

            // Peer without UTXO commitments support
            let services = 0;
            let version = create_version_message(services);
            assert!(!version.supports_utxo_commitments(), "Should not support UTXO commitments");

            // Peer with multiple flags including UTXO commitments
            let services = NODE_UTXO_COMMITMENTS | NODE_COMPACT_FILTERS | NODE_PACKAGE_RELAY;
            let version = create_version_message(services);
            assert!(version.supports_utxo_commitments(), "Should support UTXO commitments with other flags");
            assert!(version.supports_compact_filters(), "Should also support compact filters");
            assert!(version.supports_package_relay(), "Should also support package relay");
        }
    }

    #[test]
    fn test_ban_list_sharing_flag() {
        // Peer with ban list sharing support
        let services = NODE_BAN_LIST_SHARING;
        let version = create_version_message(services);
        assert!(version.supports_ban_list_sharing(), "Should support ban list sharing");

        // Peer without ban list sharing support
        let services = 0;
        let version = create_version_message(services);
        assert!(!version.supports_ban_list_sharing(), "Should not support ban list sharing");

        // Peer with multiple flags including ban list sharing
        let services = NODE_BAN_LIST_SHARING | NODE_COMPACT_FILTERS | NODE_FIBRE;
        let version = create_version_message(services);
        assert!(version.supports_ban_list_sharing(), "Should support ban list sharing with other flags");
        assert!(version.supports_compact_filters(), "Should also support compact filters");
        assert!(version.supports_fibre(), "Should also support FIBRE");
    }

    #[test]
    fn test_compact_filters_flag() {
        // Peer with compact filters support
        let services = NODE_COMPACT_FILTERS;
        let version = create_version_message(services);
        assert!(version.supports_compact_filters(), "Should support compact filters");

        // Peer without compact filters support
        let services = 0;
        let version = create_version_message(services);
        assert!(!version.supports_compact_filters(), "Should not support compact filters");
    }

    #[test]
    fn test_package_relay_flag() {
        // Peer with package relay support
        let services = NODE_PACKAGE_RELAY;
        let version = create_version_message(services);
        assert!(version.supports_package_relay(), "Should support package relay");

        // Peer without package relay support
        let services = 0;
        let version = create_version_message(services);
        assert!(!version.supports_package_relay(), "Should not support package relay");
    }

    #[test]
    fn test_fibre_flag() {
        // Peer with FIBRE support
        let services = NODE_FIBRE;
        let version = create_version_message(services);
        assert!(version.supports_fibre(), "Should support FIBRE");

        // Peer without FIBRE support
        let services = 0;
        let version = create_version_message(services);
        assert!(!version.supports_fibre(), "Should not support FIBRE");
    }

    #[test]
    fn test_multiple_flags() {
        // Peer with all flags
        let services = NODE_UTXO_COMMITMENTS | NODE_BAN_LIST_SHARING | NODE_COMPACT_FILTERS | 
                      NODE_PACKAGE_RELAY | NODE_FIBRE;
        let version = create_version_message(services);
        
        #[cfg(feature = "utxo-commitments")]
        assert!(version.supports_utxo_commitments(), "Should support UTXO commitments");
        assert!(version.supports_ban_list_sharing(), "Should support ban list sharing");
        assert!(version.supports_compact_filters(), "Should support compact filters");
        assert!(version.supports_package_relay(), "Should support package relay");
        assert!(version.supports_fibre(), "Should support FIBRE");
    }

    #[test]
    fn test_flag_independence() {
        // Test that flags are independent - setting one doesn't affect others
        let services = NODE_UTXO_COMMITMENTS;
        let version = create_version_message(services);
        
        #[cfg(feature = "utxo-commitments")]
        assert!(version.supports_utxo_commitments(), "Should support UTXO commitments");
        assert!(!version.supports_ban_list_sharing(), "Should not support ban list sharing");
        assert!(!version.supports_compact_filters(), "Should not support compact filters");
    }

    #[test]
    fn test_flag_bit_positions() {
        // Verify bit positions don't overlap
        #[cfg(feature = "utxo-commitments")]
        {
            assert_eq!(NODE_UTXO_COMMITMENTS, 1 << 27, "UTXO commitments should be bit 27");
        }
        assert_eq!(NODE_BAN_LIST_SHARING, 1 << 28, "Ban list sharing should be bit 28");
        assert_eq!(NODE_PACKAGE_RELAY, 1 << 25, "Package relay should be bit 25");
        assert_eq!(NODE_FIBRE, 1 << 26, "FIBRE should be bit 26");
        
        // Verify no overlap
        #[cfg(feature = "utxo-commitments")]
        {
            assert_eq!(NODE_UTXO_COMMITMENTS & NODE_BAN_LIST_SHARING, 0, "Flags should not overlap");
            assert_eq!(NODE_UTXO_COMMITMENTS & NODE_PACKAGE_RELAY, 0, "Flags should not overlap");
            assert_eq!(NODE_UTXO_COMMITMENTS & NODE_FIBRE, 0, "Flags should not overlap");
        }
        assert_eq!(NODE_BAN_LIST_SHARING & NODE_PACKAGE_RELAY, 0, "Flags should not overlap");
        assert_eq!(NODE_BAN_LIST_SHARING & NODE_FIBRE, 0, "Flags should not overlap");
        assert_eq!(NODE_PACKAGE_RELAY & NODE_FIBRE, 0, "Flags should not overlap");
    }
}

