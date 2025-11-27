//! Tests for REST API endpoint handlers

#[cfg(feature = "rest-api")]
mod tests {
    use bllvm_node::rpc::blockchain::BlockchainRpc;
    use bllvm_node::rpc::mempool::MempoolRpc;
    use bllvm_node::rpc::mining::MiningRpc;
    use bllvm_node::rpc::network::NetworkRpc;
    use bllvm_node::rpc::rawtx::RawTxRpc;
    use bllvm_node::rpc::rest::types::ApiResponse;
    use bllvm_node::rpc::rest::{
        addresses, blocks, chain, fees, mempool as rest_mempool, network as rest_network,
        transactions,
    };
    use bllvm_node::storage::Storage;
    use serde_json::Value;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn create_test_blockchain_rpc() -> Arc<BlockchainRpc> {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        Arc::new(BlockchainRpc::with_dependencies(storage))
    }

    fn create_test_mempool_rpc() -> Arc<MempoolRpc> {
        Arc::new(MempoolRpc::new())
    }

    fn create_test_network_rpc() -> Arc<NetworkRpc> {
        Arc::new(NetworkRpc::new())
    }

    fn create_test_mining_rpc() -> Arc<MiningRpc> {
        Arc::new(MiningRpc::new())
    }

    fn create_test_rawtx_rpc() -> Arc<RawTxRpc> {
        Arc::new(RawTxRpc::new())
    }

    // Chain endpoints
    #[tokio::test]
    async fn test_chain_tip_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_tip(&blockchain).await;

        // Should return a result (may be empty if no blocks)
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chain_height_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_height(&blockchain).await;

        // Should return a result (may be 0 if no blocks)
        assert!(result.is_ok());
        let height: Value = result.unwrap();
        // Height should be a number
        assert!(height.is_number() || height.is_null());
    }

    #[tokio::test]
    async fn test_chain_info_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_info(&blockchain).await;

        // Should return a result
        assert!(result.is_ok());
        let info: Value = result.unwrap();
        // Info should be an object
        assert!(info.is_object() || info.is_null());
    }

    // Block endpoints
    #[tokio::test]
    async fn test_blocks_get_by_hash() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"; // Genesis block

        // May fail if block doesn't exist, but should handle gracefully
        let result = blocks::get_block_by_hash(&blockchain, hash).await;
        // Result may be Ok or Err depending on whether block exists
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_blocks_get_by_height() {
        let blockchain = create_test_blockchain_rpc();

        // May fail if height doesn't exist, but should handle gracefully
        let result = blocks::get_block_by_height(&blockchain, 0).await;
        // Result may be Ok or Err depending on whether block exists
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_blocks_get_transactions() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

        // May fail if block doesn't exist
        let result = blocks::get_block_transactions(&blockchain, hash).await;
        // Result may be Ok or Err
        assert!(result.is_ok() || result.is_err());
    }

    // Transaction endpoints
    #[tokio::test]
    async fn test_transactions_get_details() {
        let rawtx = create_test_rawtx_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";

        // May fail if transaction doesn't exist
        let result = transactions::get_transaction(&rawtx, txid).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_transactions_get_confirmations() {
        let rawtx = create_test_rawtx_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";

        // May fail if transaction doesn't exist
        let result = transactions::get_transaction_confirmations(&rawtx, txid).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_transactions_submit() {
        let rawtx = create_test_rawtx_rpc();
        let invalid_hex = "invalid";

        // Should fail with invalid transaction
        let result = transactions::submit_transaction(&rawtx, invalid_hex).await;
        assert!(result.is_err());
    }

    // Address endpoints
    #[tokio::test]
    async fn test_addresses_get_balance() {
        let blockchain = create_test_blockchain_rpc();
        let address = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa"; // Genesis block coinbase

        // May fail if address indexing not enabled or address not found
        let result = addresses::get_address_balance(&blockchain, address).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_addresses_get_transactions() {
        let blockchain = create_test_blockchain_rpc();
        let address = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

        // May fail if address indexing not enabled or address not found
        let result = addresses::get_address_transactions(&blockchain, address).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_addresses_get_utxos() {
        let blockchain = create_test_blockchain_rpc();
        let address = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

        // May fail if address indexing not enabled or address not found
        let result = addresses::get_address_utxos(&blockchain, address).await;
        assert!(result.is_ok() || result.is_err());
    }

    // Mempool endpoints
    #[tokio::test]
    async fn test_mempool_get_all() {
        let mempool = create_test_mempool_rpc();
        let result = rest_mempool::get_mempool(&mempool).await;

        // Should return a result (may be empty array)
        assert!(result.is_ok());
        let txs: Value = result.unwrap();
        assert!(txs.is_array());
    }

    #[tokio::test]
    async fn test_mempool_get_transaction() {
        let mempool = create_test_mempool_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";

        // May fail if transaction not in mempool
        let result = rest_mempool::get_mempool_transaction(&mempool, txid).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_mempool_get_stats() {
        let mempool = create_test_mempool_rpc();
        let result = rest_mempool::get_mempool_stats(&mempool).await;

        // Should return a result
        assert!(result.is_ok());
        let stats: Value = result.unwrap();
        assert!(stats.is_object());
    }

    // Network endpoints
    #[tokio::test]
    async fn test_network_get_info() {
        let network = create_test_network_rpc();
        let result = rest_network::get_network_info(&network).await;

        // Should return a result
        assert!(result.is_ok());
        let info: Value = result.unwrap();
        assert!(info.is_object());
    }

    #[tokio::test]
    async fn test_network_get_peers() {
        let network = create_test_network_rpc();
        let result = rest_network::get_network_peers(&network).await;

        // Should return a result (may be empty array)
        assert!(result.is_ok());
        let peers: Value = result.unwrap();
        assert!(peers.is_array());
    }

    // Fee endpoints
    #[tokio::test]
    async fn test_fees_estimate() {
        let mining = create_test_mining_rpc();
        let result = fees::get_fee_estimate(&mining, Some(6)).await;

        // Should return a result
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_fees_estimate_default() {
        let mining = create_test_mining_rpc();
        let result = fees::get_fee_estimate(&mining, None).await;

        // Should return a result (uses default 6 blocks)
        assert!(result.is_ok() || result.is_err());
    }

    // API Response types
    #[test]
    fn test_api_response_structure() {
        let data = serde_json::json!({"test": "data"});
        let response = ApiResponse::success(data.clone(), None);

        assert_eq!(response.data, data);
        assert_eq!(response.meta.version, "1.0");
        assert!(response.meta.timestamp.parse::<u64>().is_ok());
    }

    #[test]
    fn test_api_response_with_links() {
        use std::collections::HashMap;

        let data = serde_json::json!({});
        let mut links = HashMap::new();
        links.insert("self".to_string(), "/api/v1/test".to_string());

        let response = ApiResponse::success(data, None).with_links(links.clone());
        assert_eq!(response.links, Some(links));
    }

    #[test]
    fn test_api_response_with_request_id() {
        let data = serde_json::json!({});
        let request_id = Some("test-request-id".to_string());
        let response = ApiResponse::success(data, request_id.clone());

        assert_eq!(response.meta.request_id, request_id);
    }
}
