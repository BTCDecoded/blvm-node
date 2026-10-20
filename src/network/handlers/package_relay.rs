//! BIP331 Package Relay handler (PkgTxn).

use crate::network::bandwidth_protection::ServiceType;
use crate::network::network_manager::NetworkManager;
use crate::network::package_relay::PackageRelay;
use crate::network::package_relay_handler::handle_pkgtxn;
use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use anyhow::Result;
use std::net::SocketAddr;
use tracing::warn;

impl NetworkManager {
    /// Handle PkgTxn message from a peer
    pub(crate) async fn handle_pkgtxn_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        match self
            .bandwidth_protection()
            .check_service_request(ServiceType::PackageRelay, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Package relay bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for package relay: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        self.bandwidth_protection()
            .record_service_request(ServiceType::PackageRelay, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::PkgTxn(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected PkgTxn message")),
        };

        let mut relay = PackageRelay::new();
        let reject = {
            let utxo = self.utxo_set_arc();
            let utxo_guard = utxo.lock().await;
            let reject = handle_pkgtxn(&mut relay, &request, Some(&utxo_guard))?;
            drop(utxo_guard);
            reject
        };
        if let Some(reject) = reject {
            let response_wire =
                ProtocolParser::serialize_message(&ProtocolMessage::PkgTxnReject(reject))?;
            let response_bytes = response_wire.len() as u64;
            self.send_to_peer(peer_addr, response_wire).await?;
            if response_bytes > 0 {
                self.bandwidth_protection()
                    .record_service_bandwidth(ServiceType::PackageRelay, peer_addr, response_bytes)
                    .await;
            }
            return Ok(());
        }

        let mut txs = Vec::with_capacity(request.transactions.len());
        for raw in &request.transactions {
            if let Ok(pair) = crate::network::package_relay_handler::decode_package_transaction(raw)
            {
                txs.push(pair);
            }
        }
        let _ = self.submit_transactions_to_mempool(&txs).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_pkgtxn_invalid_wire_errors() {
        let listen: SocketAddr = "127.0.0.1:18474".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18475".parse().unwrap();
        let nm = NetworkManager::new(listen);
        assert!(
            nm.handle_pkgtxn_request(vec![0xff, 0xfe], peer)
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_package_witness_larger_than_the_guess_sets_the_fee_rate() {
        use crate::network::package_relay::PackageRejectReason;
        use crate::network::package_relay_handler::decode_package_transaction;
        use crate::network::protocol::PkgTxnMessage;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::serialization::transaction::serialize_transaction_with_witness;
        use blvm_protocol::{
            BitcoinProtocolEngine, OutPoint, ProtocolVersion, Transaction, TransactionInput,
            TransactionOutput, UTXO, UtxoSet,
        };
        use std::sync::Arc;

        let funding = OutPoint {
            hash: [7u8; 32],
            index: 0,
        };
        let tx = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 49_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let witness = vec![vec![0x11u8; 4_000]];
        let raw = serialize_transaction_with_witness(&tx, &[witness.clone()]).unwrap();
        let (decoded, decoded_witness) = decode_package_transaction(&raw).unwrap();
        assert_eq!(decoded_witness.as_ref().map(|stacks| stacks.len()), Some(1));
        assert!(decoded_witness.as_ref().unwrap()[0][0].len() > 107);

        let mut utxo_set = UtxoSet::default();
        utxo_set.insert(
            funding,
            Arc::new(UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
        let msg = PkgTxnMessage {
            package_id: vec![1u8; 32],
            transactions: vec![raw],
        };
        let mut relay = PackageRelay::new();
        let reject = handle_pkgtxn(&mut relay, &msg, Some(&utxo_set))
            .unwrap()
            .expect("real witness weight is below 1 sat/vB");
        assert_eq!(reject.reason, PackageRejectReason::FeeRateTooLow as u8);

        let dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(crate::storage::Storage::new(dir.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 1;
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
        let listen: SocketAddr = "127.0.0.1:18581".parse().unwrap();
        let nm =
            NetworkManager::new(listen).with_dependencies(protocol, storage, Arc::clone(&mempool));
        nm.submit_transactions_to_mempool(&[(decoded, decoded_witness)])
            .await
            .unwrap();
        assert!(
            mempool
                .get_transactions()
                .iter()
                .all(|pooled| pooled.outputs[0].value != 49_000)
        );
    }
}
