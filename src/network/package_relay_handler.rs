//! BIP331 Package Relay handlers
//!
//! These functions process incoming package relay messages and use the
//! `PackageRelay` validator to check structure before higher-level flow.

use anyhow::Result;
use tracing::{debug, warn};

use crate::network::package_relay::{
    PackageRelay, TransactionPackage, PackageId, PackageRejectReason,
};
use crate::network::protocol::{SendPkgTxnMessage, PkgTxnMessage, PkgTxnRejectMessage};
use protocol_engine::Transaction;

/// Handle sendpkgtxn request (peer signals intent to send a package)
pub fn handle_sendpkgtxn(_relay: &PackageRelay, msg: &SendPkgTxnMessage) -> Result<()> {
    debug!("Received sendpkgtxn for package {} ({} tx hashes)", hex::encode(&msg.package_id), msg.tx_hashes.len());
    // In a full implementation, we could decide to request the package or ignore it based on policy
    Ok(())
}

/// Handle pkgtxn: validate package and return optional rejection
pub fn handle_pkgtxn(relay: &mut PackageRelay, msg: &PkgTxnMessage) -> Result<Option<PkgTxnRejectMessage>> {
    // Deserialize transactions (they are bincode-serialized protocol_engine::Transaction)
    let mut txs: Vec<Transaction> = Vec::with_capacity(msg.transactions.len());
    for raw in &msg.transactions {
        match bincode::deserialize::<Transaction>(raw) {
            Ok(tx) => txs.push(tx),
            Err(_) => {
                return Ok(Some(PkgTxnRejectMessage {
                    package_id: msg.package_id.clone(),
                    reason: PackageRejectReason::InvalidStructure as u8,
                    reason_text: Some("failed to deserialize transaction".to_string()),
                }))
            }
        }
    }

    // Construct package
    let pkg = match TransactionPackage::new(txs) {
        Ok(p) => p,
        Err(_) => {
            return Ok(Some(PkgTxnRejectMessage {
                package_id: msg.package_id.clone(),
                reason: PackageRejectReason::InvalidStructure as u8,
                reason_text: Some("invalid package structure".to_string()),
            }))
        }
    };

    // Validate against limits
    if let Err(reason) = relay.validate_package(&pkg) {
        return Ok(Some(PkgTxnRejectMessage {
            package_id: msg.package_id.clone(),
            reason: reason as u8,
            reason_text: Some(format!("{:?}", reason)),
        }));
    }

    // Register package for further processing
    match relay.register_package(pkg) {
        Ok(_id) => Ok(None),
        Err(e) => {
            warn!("failed to register package: {}", e);
            Ok(Some(PkgTxnRejectMessage {
                package_id: msg.package_id.clone(),
                reason: PackageRejectReason::InvalidStructure as u8,
                reason_text: Some("registration failed".to_string()),
            }))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use protocol_engine::{TransactionInput, TransactionOutput, OutPoint};

    fn minimal_tx() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![TransactionOutput { value: 0, script_pubkey: vec![] }],
            lock_time: 0,
        }
    }

    #[test]
    fn test_pkgtxn_reject_on_bad_deserialization() {
        let mut relay = PackageRelay::new();
        let msg = PkgTxnMessage {
            package_id: vec![0u8; 32],
            transactions: vec![vec![0xde, 0xad, 0xbe, 0xef]], // not a valid bincode Transaction
        };

        let result = handle_pkgtxn(&mut relay, &msg).unwrap();
        assert!(result.is_some());
        let rej = result.unwrap();
        assert_eq!(rej.package_id, msg.package_id);
        assert_eq!(rej.reason, PackageRejectReason::InvalidStructure as u8);
    }

    #[test]
    fn test_pkgtxn_happy_single_minimal_tx() {
        let mut relay = PackageRelay::new();
        let tx = minimal_tx();
        let raw = bincode::serialize(&tx).unwrap();
        let msg = PkgTxnMessage {
            package_id: vec![1u8; 32],
            transactions: vec![raw],
        };

        let result = handle_pkgtxn(&mut relay, &msg).unwrap();
        assert!(result.is_none());
    }
}

