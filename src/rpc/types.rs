//! RPC types and utilities
//!
//! Common types and utilities used across RPC methods.

use serde::{Deserialize, Serialize};
// use bllvm_protocol::{Block, Transaction, Hash};

/// RPC error response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

/// RPC response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse<T> {
    pub jsonrpc: String,
    pub result: Option<T>,
    pub error: Option<RpcError>,
    pub id: Option<serde_json::Value>,
}

impl<T> RpcResponse<T> {
    /// Create a successful response
    pub fn success(result: T, id: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Create an error response
    pub fn error(error: RpcError, id: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }
}

/// Blockchain info response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockchainInfo {
    pub chain: String,
    pub blocks: u64,
    pub headers: u64,
    pub bestblockhash: String,
    pub difficulty: f64,
    pub mediantime: u64,
    pub verificationprogress: f64,
    pub initialblockdownload: bool,
    pub chainwork: String,
    pub size_on_disk: u64,
    pub pruned: bool,
    pub softforks: Vec<serde_json::Value>,
    pub warnings: String,
}

/// Block info response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockInfo {
    pub hash: String,
    pub confirmations: u64,
    pub strippedsize: u64,
    pub size: u64,
    pub weight: u64,
    pub height: u64,
    pub version: i32,
    pub version_hex: String,
    pub merkleroot: String,
    pub tx: Vec<String>,
    pub time: u64,
    pub mediantime: u64,
    pub nonce: u64,
    pub bits: String,
    pub difficulty: f64,
    pub chainwork: String,
    pub n_tx: u64,
    pub previousblockhash: Option<String>,
    pub nextblockhash: Option<String>,
}

/// Transaction info response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionInfo {
    pub txid: String,
    pub hash: String,
    pub version: i32,
    pub size: u64,
    pub vsize: u64,
    pub weight: u64,
    pub locktime: u64,
    pub vin: Vec<serde_json::Value>,
    pub vout: Vec<serde_json::Value>,
    pub hex: String,
}

/// Network info response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub version: u32,
    pub subversion: String,
    pub protocolversion: u32,
    pub localservices: String,
    pub localrelay: bool,
    pub timeoffset: i64,
    pub networkactive: bool,
    pub connections: u32,
    pub networks: Vec<serde_json::Value>,
    pub relayfee: f64,
    pub incrementalfee: f64,
    pub localaddresses: Vec<serde_json::Value>,
    pub warnings: String,
}

/// Mining info response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningInfo {
    pub blocks: u64,
    pub currentblocksize: u64,
    pub currentblocktx: u64,
    pub difficulty: f64,
    pub networkhashps: f64,
    pub pooledtx: u64,
    pub chain: String,
    pub warnings: String,
}

/// Block template response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTemplate {
    pub capabilities: Vec<String>,
    pub version: u32,
    pub rules: Vec<String>,
    pub vbavailable: serde_json::Value,
    pub vbrequired: u32,
    pub previousblockhash: String,
    pub transactions: Vec<serde_json::Value>,
    pub coinbaseaux: serde_json::Value,
    pub coinbasevalue: u64,
    pub longpollid: String,
    pub target: String,
    pub mintime: u64,
    pub mutable: Vec<String>,
    pub noncerange: String,
    pub sigoplimit: u64,
    pub sizelimit: u64,
    pub weightlimit: u64,
    pub curtime: u64,
    pub bits: String,
    pub height: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_rpc_response_success() {
        let result = "test_result";
        let id = serde_json::Value::Number(serde_json::Number::from(1));
        let response = RpcResponse::success(result, Some(id.clone()));

        assert_eq!(response.jsonrpc, "2.0");
        assert_eq!(response.result, Some("test_result"));
        assert!(response.error.is_none());
        assert_eq!(response.id, Some(id));
    }

    #[test]
    fn test_rpc_response_error() {
        let error = RpcError {
            code: -32601,
            message: "Method not found".to_string(),
            data: None,
        };
        let id = serde_json::Value::Number(serde_json::Number::from(1));
        let response: RpcResponse<String> = RpcResponse::error(error.clone(), Some(id.clone()));

        assert_eq!(response.jsonrpc, "2.0");
        assert!(response.result.is_none());
        assert_eq!(response.error, Some(error));
        assert_eq!(response.id, Some(id));
    }

    #[test]
    fn test_rpc_response_success_without_id() {
        let result = "test_result";
        let response = RpcResponse::success(result, None);

        assert_eq!(response.jsonrpc, "2.0");
        assert_eq!(response.result, Some("test_result"));
        assert!(response.error.is_none());
        assert!(response.id.is_none());
    }

    #[test]
    fn test_rpc_response_error_without_id() {
        let error = RpcError {
            code: -32601,
            message: "Method not found".to_string(),
            data: Some(serde_json::Value::String("test_data".to_string())),
        };
        let response: RpcResponse<String> = RpcResponse::error(error.clone(), None);

        assert_eq!(response.jsonrpc, "2.0");
        assert!(response.result.is_none());
        assert_eq!(response.error, Some(error));
        assert!(response.id.is_none());
    }

    #[test]
    fn test_rpc_error_serialization() {
        let error = RpcError {
            code: -32601,
            message: "Method not found".to_string(),
            data: Some(serde_json::Value::String("test_data".to_string())),
        };

        let json = serde_json::to_string(&error).unwrap();
        let deserialized: RpcError = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.code, error.code);
        assert_eq!(deserialized.message, error.message);
        assert_eq!(deserialized.data, error.data);
    }

    #[test]
    fn test_blockchain_info_serialization() {
        let info = BlockchainInfo {
            chain: "main".to_string(),
            blocks: 100,
            headers: 100,
            bestblockhash: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            difficulty: 1.0,
            mediantime: 1234567890,
            verificationprogress: 1.0,
            initialblockdownload: false,
            chainwork: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            size_on_disk: 1000000,
            pruned: false,
            softforks: vec![],
            warnings: "".to_string(),
        };

        let json = serde_json::to_string(&info).unwrap();
        let deserialized: BlockchainInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.chain, info.chain);
        assert_eq!(deserialized.blocks, info.blocks);
        assert_eq!(deserialized.difficulty, info.difficulty);
    }

    #[test]
    fn test_block_info_serialization() {
        let block_info = BlockInfo {
            hash: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            confirmations: 1,
            strippedsize: 100,
            size: 200,
            weight: 300,
            height: 1,
            version: 1,
            version_hex: "00000001".to_string(),
            merkleroot: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            tx: vec!["tx1".to_string(), "tx2".to_string()],
            time: 1234567890,
            mediantime: 1234567890,
            nonce: 12345,
            bits: "1d00ffff".to_string(),
            difficulty: 1.0,
            chainwork: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            n_tx: 2,
            previousblockhash: Some(
                "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            ),
            nextblockhash: Some(
                "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            ),
        };

        let json = serde_json::to_string(&block_info).unwrap();
        let deserialized: BlockInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.hash, block_info.hash);
        assert_eq!(deserialized.height, block_info.height);
        assert_eq!(deserialized.n_tx, block_info.n_tx);
    }

    #[test]
    fn test_transaction_info_serialization() {
        let tx_info = TransactionInfo {
            txid: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            hash: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            version: 1,
            size: 100,
            vsize: 100,
            weight: 400,
            locktime: 0,
            vin: vec![serde_json::Value::Object(serde_json::Map::new())],
            vout: vec![serde_json::Value::Object(serde_json::Map::new())],
            hex: "01000000000000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        };

        let json = serde_json::to_string(&tx_info).unwrap();
        let deserialized: TransactionInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.txid, tx_info.txid);
        assert_eq!(deserialized.version, tx_info.version);
        assert_eq!(deserialized.size, tx_info.size);
    }

    #[test]
    fn test_network_info_serialization() {
        let network_info = NetworkInfo {
            version: 70015,
            subversion: "/Satoshi:0.21.0/".to_string(),
            protocolversion: 70015,
            localservices: "0000000000000001".to_string(),
            localrelay: true,
            timeoffset: 0,
            networkactive: true,
            connections: 8,
            networks: vec![],
            relayfee: 0.00001,
            incrementalfee: 0.00001,
            localaddresses: vec![],
            warnings: "".to_string(),
        };

        let json = serde_json::to_string(&network_info).unwrap();
        let deserialized: NetworkInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.version, network_info.version);
        assert_eq!(deserialized.connections, network_info.connections);
        assert_eq!(deserialized.relayfee, network_info.relayfee);
    }

    #[test]
    fn test_mining_info_serialization() {
        let mining_info = MiningInfo {
            blocks: 100,
            currentblocksize: 1000,
            currentblocktx: 10,
            difficulty: 1.0,
            networkhashps: 1000000.0,
            pooledtx: 5,
            chain: "main".to_string(),
            warnings: "".to_string(),
        };

        let json = serde_json::to_string(&mining_info).unwrap();
        let deserialized: MiningInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.blocks, mining_info.blocks);
        assert_eq!(deserialized.difficulty, mining_info.difficulty);
        assert_eq!(deserialized.chain, mining_info.chain);
    }

    #[test]
    fn test_block_template_serialization() {
        let template = BlockTemplate {
            capabilities: vec!["proposal".to_string()],
            version: 536870912,
            rules: vec!["csv".to_string(), "segwit".to_string()],
            vbavailable: serde_json::Value::Object(serde_json::Map::new()),
            vbrequired: 0,
            previousblockhash: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            transactions: vec![],
            coinbaseaux: serde_json::Value::Object(serde_json::Map::new()),
            coinbasevalue: 5000000000,
            longpollid: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            target: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            mintime: 1234567890,
            mutable: vec!["time".to_string(), "transactions".to_string()],
            noncerange: "00000000ffffffff".to_string(),
            sigoplimit: 80000,
            sizelimit: 4000000,
            weightlimit: 4000000,
            curtime: 1234567890,
            bits: "1d00ffff".to_string(),
            height: 100,
        };

        let json = serde_json::to_string(&template).unwrap();
        let deserialized: BlockTemplate = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.version, template.version);
        assert_eq!(deserialized.height, template.height);
        assert_eq!(deserialized.capabilities, template.capabilities);
    }

    #[test]
    fn test_rpc_error_with_data() {
        let error = RpcError {
            code: -32602,
            message: "Invalid params".to_string(),
            data: Some(serde_json::Value::String("param_name".to_string())),
        };

        assert_eq!(error.code, -32602);
        assert_eq!(error.message, "Invalid params");
        assert!(error.data.is_some());
    }

    #[test]
    fn test_rpc_error_without_data() {
        let error = RpcError {
            code: -32601,
            message: "Method not found".to_string(),
            data: None,
        };

        assert_eq!(error.code, -32601);
        assert_eq!(error.message, "Method not found");
        assert!(error.data.is_none());
    }
}
