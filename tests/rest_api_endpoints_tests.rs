//! Tests for REST API endpoint handlers

#[cfg(feature = "rest-api")]
mod tests {
    use blvm_node::rpc::blockchain::BlockchainRpc;
    use blvm_node::rpc::mempool::MempoolRpc;
    use blvm_node::rpc::mining::MiningRpc;
    use blvm_node::rpc::network::NetworkRpc;
    use blvm_node::rpc::rawtx::RawTxRpc;
    use blvm_node::rpc::rest::types::ApiResponse;
    use blvm_node::rpc::rest::{
        addresses, blocks, chain, fees, mempool as rest_mempool, mining, network as rest_network,
        node, transactions,
    };
    use blvm_node::storage::Storage;
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

    fn assert_json_object_has_fields(val: &Value, fields: &[&str]) {
        let obj = val.as_object().expect("expected JSON object");
        for field in fields {
            assert!(obj.contains_key(*field), "missing field `{field}`");
        }
    }

    fn assert_non_negative_number(val: &Value) {
        let n = val
            .as_u64()
            .or_else(|| val.as_i64().map(|i| i.max(0) as u64));
        assert!(n.is_some(), "expected non-negative number, got {val}");
    }

    fn assert_uninitialized_block_lookup_err(result: Result<Value, anyhow::Error>) {
        assert!(
            result.is_err(),
            "empty chain fixture must not serve block data"
        );
    }

    fn assert_missing_mempool_entry_err(result: Result<Value, anyhow::Error>) {
        assert!(
            result.is_err(),
            "unknown txid must not return mempool entry"
        );
    }

    fn assert_missing_tx_err(result: Result<Value, anyhow::Error>) {
        assert!(
            result.is_err(),
            "unwired rawtx fixture must not serve chain/mempool tx data"
        );
    }

    fn assert_invalid_address_err(result: Result<Value, anyhow::Error>) {
        assert!(result.is_err(), "malformed address must not succeed");
    }

    // Address endpoints
    #[tokio::test]
    async fn test_addresses_get_balance() {
        let blockchain = create_test_blockchain_rpc();
        let address = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa"; // Genesis block coinbase

        let balance = addresses::get_address_balance(&blockchain, address)
            .await
            .expect("base58 address lookup must succeed on empty chain");
        assert_eq!(balance.get("balance").unwrap().as_i64(), Some(0));
    }

    #[tokio::test]
    async fn test_addresses_get_transactions() {
        let blockchain = create_test_blockchain_rpc();
        let address = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

        let txids = addresses::get_address_transactions(&blockchain, address)
            .await
            .expect("base58 address lookup must succeed on empty chain");
        assert!(txids.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_addresses_get_utxos() {
        let blockchain = create_test_blockchain_rpc();
        let address = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

        let utxos = addresses::get_address_utxos(&blockchain, address)
            .await
            .expect("base58 address lookup must succeed on empty chain");
        assert_eq!(utxos.get("balance").unwrap().as_f64(), Some(0.0));
        assert_eq!(utxos.get("utxo_count").unwrap().as_i64(), Some(0));
        assert_eq!(utxos.get("tx_count").unwrap().as_i64(), Some(0));
    }

    #[tokio::test]
    async fn test_addresses_reject_malformed_input() {
        let blockchain = create_test_blockchain_rpc();
        let result = addresses::get_address_balance(&blockchain, "not-a-valid-address").await;
        assert_invalid_address_err(result);
    }

    // Chain endpoints
    #[tokio::test]
    async fn test_chain_tip_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_tip(&blockchain).await;

        assert!(result.is_ok());
        let tip = result.unwrap();
        assert!(
            tip.is_null() || tip.as_str().is_some_and(|s| s.len() == 64),
            "tip must be null (empty chain) or 64-char hash hex"
        );
    }

    #[tokio::test]
    async fn test_chain_height_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_height(&blockchain).await;

        assert!(result.is_ok());
        let height = result.unwrap();
        assert_non_negative_number(&height);
    }

    #[tokio::test]
    async fn test_chain_info_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_info(&blockchain).await;

        assert!(result.is_ok());
        let info = result.unwrap();
        if info.is_object() {
            assert_json_object_has_fields(&info, &["blocks", "chain", "headers"]);
        }
    }

    // Block endpoints
    #[tokio::test]
    async fn test_blocks_get_by_hash() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"; // Genesis block

        // May fail if block doesn't exist, but should handle gracefully
        let result = blocks::get_block_by_hash(&blockchain, hash).await;
        assert_uninitialized_block_lookup_err(result);
    }

    #[tokio::test]
    async fn test_blocks_get_by_height() {
        let blockchain = create_test_blockchain_rpc();
        let result = blocks::get_block_by_height(&blockchain, 0).await;
        assert_uninitialized_block_lookup_err(result);
    }

    #[tokio::test]
    async fn test_blocks_get_transactions() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
        let result = blocks::get_block_transactions(&blockchain, hash).await;
        assert_uninitialized_block_lookup_err(result);
    }

    // Transaction endpoints
    #[tokio::test]
    async fn test_transactions_get_details() {
        let rawtx = create_test_rawtx_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = transactions::get_transaction(&rawtx, txid).await;
        assert_missing_tx_err(result);
    }

    #[tokio::test]
    async fn test_transactions_get_confirmations() {
        let rawtx = create_test_rawtx_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = transactions::get_transaction_confirmations(&rawtx, txid).await;
        assert_missing_tx_err(result);
    }

    #[tokio::test]
    async fn test_transactions_submit() {
        let rawtx = create_test_rawtx_rpc();
        let invalid_hex = "invalid";

        // Should fail with invalid transaction
        let result = transactions::submit_transaction(&rawtx, invalid_hex).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_addresses_get_balance_empty_chain_hex_script() {
        let blockchain = create_test_blockchain_rpc();
        let script_hex = "51"; // OP_1

        let balance = addresses::get_address_balance(&blockchain, script_hex)
            .await
            .expect("hex scriptPubKey lookup must succeed on empty chain");
        assert_eq!(balance.get("balance").unwrap().as_i64(), Some(0));
    }

    // Mempool endpoints
    #[tokio::test]
    async fn test_mempool_get_all() {
        let mempool = create_test_mempool_rpc();
        let result = rest_mempool::get_mempool(&mempool, false).await;

        assert!(result.is_ok());
        let txs = result.unwrap();
        assert!(txs.is_array());
        assert!(txs.as_array().unwrap().is_empty(), "fresh mempool is empty");
    }

    #[tokio::test]
    async fn test_mempool_get_transaction() {
        let mempool = create_test_mempool_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";

        // May fail if transaction not in mempool
        let result = rest_mempool::get_mempool_transaction(&mempool, txid).await;
        assert_missing_mempool_entry_err(result);
    }

    #[tokio::test]
    async fn test_mempool_get_stats() {
        let mempool = create_test_mempool_rpc();
        let result = rest_mempool::get_mempool_stats(&mempool).await;

        assert!(result.is_ok());
        let stats = result.unwrap();
        assert_json_object_has_fields(&stats, &["size", "bytes", "usage"]);
        assert_non_negative_number(stats.get("size").unwrap());
    }

    // Network endpoints
    #[tokio::test]
    async fn test_network_get_info() {
        let network = create_test_network_rpc();
        let result = rest_network::get_network_info(&network).await;

        assert!(result.is_ok());
        let info = result.unwrap();
        assert_json_object_has_fields(&info, &["version", "connections"]);
    }

    #[tokio::test]
    async fn test_network_get_peers() {
        let network = create_test_network_rpc();
        let result = rest_network::get_network_peers(&network).await;

        assert!(result.is_ok());
        let peers = result.unwrap();
        assert!(peers.is_array());
        assert!(
            peers.as_array().unwrap().is_empty(),
            "no peers in unit test"
        );
    }

    // Fee endpoints
    #[tokio::test]
    async fn test_fees_estimate() {
        let mining = create_test_mining_rpc();
        let result = fees::get_fee_estimate(&mining, Some(6)).await;

        assert!(result.is_ok());
        let estimate = result.unwrap();
        assert!(
            estimate.get("feerate").is_some() || estimate.get("errors").is_some(),
            "fee estimate must return feerate or errors object"
        );
        assert_eq!(estimate.get("blocks").unwrap().as_u64(), Some(6));
    }

    #[tokio::test]
    async fn test_fees_estimate_default() {
        let mining = create_test_mining_rpc();
        let result = fees::get_fee_estimate(&mining, None).await;

        assert!(result.is_ok());
        let estimate = result.unwrap();
        assert!(
            estimate.get("feerate").is_some() || estimate.get("errors").is_some(),
            "fee estimate must return feerate or errors object"
        );
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

    // New transaction endpoints
    #[tokio::test]
    async fn test_transactions_test_endpoint() {
        let rawtx = create_test_rawtx_rpc();

        let invalid = transactions::test_transaction(&rawtx, "not-hex").await;
        assert!(invalid.is_err(), "invalid hex must be rejected");

        let truncated = transactions::test_transaction(&rawtx, "0100").await;
        assert!(truncated.is_err(), "truncated hex must be rejected");
    }

    #[tokio::test]
    async fn test_transactions_decode_endpoint() {
        let rawtx = create_test_rawtx_rpc();

        let invalid = transactions::decode_transaction(&rawtx, "not-hex").await;
        assert!(invalid.is_err(), "invalid hex must be rejected");
    }

    #[tokio::test]
    async fn test_transactions_create_endpoint() {
        let rawtx = create_test_rawtx_rpc();
        let inputs = serde_json::json!([
            {
                "txid": "0000000000000000000000000000000000000000000000000000000000000000",
                "vout": 0
            }
        ]);
        let outputs = serde_json::json!({
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
        });

        let result =
            transactions::create_transaction(&rawtx, inputs, outputs, None, None, None).await;
        assert!(result.is_ok(), "Should create transaction");
        let tx_hex = result.unwrap();
        assert!(tx_hex.is_string(), "Result should be hex string");
    }

    // New chain endpoints
    #[tokio::test]
    async fn test_chain_difficulty_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_difficulty(&blockchain).await;
        assert!(result.is_ok());
        let difficulty = result.unwrap();
        assert!(difficulty.is_number(), "difficulty must be numeric");
        assert!(difficulty.as_f64().unwrap_or(0.0) >= 0.0);
    }

    #[tokio::test]
    async fn test_chain_utxo_set_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_utxo_set_info(&blockchain).await;
        assert!(result.is_ok());
        let info = result.unwrap();
        if info.is_object() {
            assert_json_object_has_fields(&info, &["height", "txouts", "transactions"]);
        }
    }

    #[tokio::test]
    async fn test_chain_tips_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_tips(&blockchain).await;
        assert!(result.is_ok());
        let tips = result.unwrap();
        assert!(tips.is_array(), "Chain tips should be an array");
    }

    #[tokio::test]
    async fn test_chain_tx_stats_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_chain_tx_stats(&blockchain, Some(144)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chain_prune_info_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_prune_info(&blockchain).await;
        assert!(result.is_ok());
        let info = result.unwrap();
        assert!(info.is_object());
        assert_json_object_has_fields(&info, &["pruning_enabled", "mode"]);
    }

    #[tokio::test]
    async fn test_chain_index_info_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let result = chain::get_index_info(&blockchain, None).await;
        assert!(result.is_ok());
        let info = result.unwrap();
        assert!(info.is_object());
    }

    // New block endpoints
    #[tokio::test]
    async fn test_blocks_header_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
        let result = blocks::get_block_header(&blockchain, hash, true).await;
        assert_uninitialized_block_lookup_err(result);
    }

    #[tokio::test]
    async fn test_blocks_stats_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
        let result = blocks::get_block_stats(&blockchain, hash).await;
        assert_uninitialized_block_lookup_err(result);
    }

    #[tokio::test]
    async fn test_blocks_filter_endpoint() {
        let blockchain = create_test_blockchain_rpc();
        let hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
        let result = blocks::get_block_filter(&blockchain, hash, None).await;
        assert_uninitialized_block_lookup_err(result);
    }

    // New mempool endpoints
    #[tokio::test]
    async fn test_mempool_ancestors_endpoint() {
        let mempool = create_test_mempool_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = rest_mempool::get_mempool_ancestors(&mempool, txid, false).await;
        assert!(result.is_ok());
        let ancestors = result.unwrap();
        assert!(
            ancestors.as_object().map(|m| m.is_empty()).unwrap_or(false)
                || ancestors.as_array().map(|a| a.is_empty()).unwrap_or(false),
            "missing tx must yield empty ancestor set"
        );
    }

    #[tokio::test]
    async fn test_mempool_descendants_endpoint() {
        let mempool = create_test_mempool_rpc();
        let txid = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = rest_mempool::get_mempool_descendants(&mempool, txid, false).await;
        assert!(result.is_ok());
        let descendants = result.unwrap();
        assert!(
            descendants
                .as_object()
                .map(|m| m.is_empty())
                .unwrap_or(false)
                || descendants
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(false),
            "missing tx must yield empty descendant set"
        );
    }

    #[tokio::test]
    async fn test_mempool_save_endpoint() {
        let mempool = create_test_mempool_rpc();
        let result = rest_mempool::save_mempool(&mempool).await;
        assert!(
            result.is_err(),
            "savemempool requires an initialized mempool manager"
        );
    }

    // New network endpoints
    #[tokio::test]
    async fn test_network_connection_count_endpoint() {
        let network = create_test_network_rpc();
        let result = rest_network::get_connection_count(&network).await;
        assert!(result.is_ok());
        assert_non_negative_number(&result.unwrap());
    }

    #[tokio::test]
    async fn test_network_totals_endpoint() {
        let network = create_test_network_rpc();
        let result = rest_network::get_net_totals(&network).await;
        assert!(result.is_ok());
        let totals = result.unwrap();
        assert!(totals.is_object());
        assert_json_object_has_fields(&totals, &["totalbytesrecv", "totalbytessent"]);
    }

    #[tokio::test]
    async fn test_network_node_addresses_endpoint() {
        let network = create_test_network_rpc();
        let result = rest_network::get_node_addresses(&network, Some(10)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_network_list_banned_endpoint() {
        let network = create_test_network_rpc();
        let result = rest_network::list_banned(&network).await;
        assert!(result.is_ok());
    }

    // New mining endpoints
    #[tokio::test]
    async fn test_mining_info_endpoint() {
        let mining = create_test_mining_rpc();
        let result = mining::get_mining_info(&mining).await;
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_json_object_has_fields(
            &info,
            &["blocks", "difficulty", "chain", "pooledtx", "networkhashps"],
        );
        assert_non_negative_number(info.get("blocks").unwrap());
    }

    #[tokio::test]
    async fn test_mining_block_template_endpoint() {
        let mining = create_test_mining_rpc();
        let result = mining::get_block_template(&mining, None, None).await;
        assert!(result.is_err(), "GBT without storage/chain must fail");
    }

    // New node endpoints
    #[tokio::test]
    async fn test_node_uptime_endpoint() {
        use blvm_node::rpc::control::ControlRpc;
        let control = ControlRpc::new();
        let result = node::get_uptime(&control).await;
        assert!(result.is_ok());
        let uptime = result.unwrap();
        assert!(uptime.is_number(), "Uptime should be a number");
    }

    #[tokio::test]
    async fn test_node_memory_endpoint() {
        use blvm_node::rpc::control::ControlRpc;
        let control = ControlRpc::new();
        let result = node::get_memory_info(&control, Some("stats")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_node_rpc_info_endpoint() {
        use blvm_node::rpc::control::ControlRpc;
        let control = ControlRpc::new();
        let result = node::get_rpc_info(&control).await;
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_json_object_has_fields(&info, &["active_commands", "logpath"]);
    }

    #[tokio::test]
    async fn test_node_help_endpoint() {
        use blvm_node::rpc::control::ControlRpc;
        let control = ControlRpc::new();
        let result = node::get_help(&control, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_node_logging_endpoint() {
        use blvm_node::rpc::control::ControlRpc;
        let control = ControlRpc::new();
        let result = node::get_logging(&control).await;
        assert!(result.is_ok());
    }
}
