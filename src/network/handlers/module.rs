//! Module serving handlers (GetModule, GetModuleByHash, GetModuleList).

use crate::network::bandwidth_protection::ServiceType;
use crate::network::module_registry_extensions::{
    handle_get_module, handle_get_module_by_hash, handle_get_module_list,
};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::warn;

impl NetworkManager {
    /// Handle GetModule request from a peer
    pub(crate) async fn handle_get_module(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetModuleMessage,
    ) -> Result<()> {
        match self
            .bandwidth_protection()
            .check_service_request(ServiceType::ModuleServing, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Module serving bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for module serving: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        self.bandwidth_protection()
            .record_service_request(ServiceType::ModuleServing, peer_addr)
            .await;

        if let Err(e) = self
            .replay_protection()
            .check_request_id(message.request_id)
            .await
        {
            warn!(
                "Replay protection: Rejected duplicate GetModule request from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        let registry = self.module_registry().lock().await.as_ref().map(Arc::clone);
        let payment_processor = self
            .payment_processor()
            .lock()
            .await
            .as_ref()
            .map(Arc::clone);
        let payment_state_machine = self
            .payment_state_machine()
            .lock()
            .await
            .as_ref()
            .map(Arc::clone);
        let encryption = self
            .module_encryption()
            .lock()
            .await
            .as_ref()
            .map(Arc::clone);
        let modules_dir = self.modules_dir().lock().await.clone();
        let node_script = self.node_payment_script().lock().await.clone();

        match handle_get_module(
            message,
            registry,
            payment_processor,
            payment_state_machine,
            encryption,
            modules_dir,
            node_script,
        )
        .await
        {
            Ok(module_response) => {
                let response_wire =
                    ProtocolParser::serialize_message(&ProtocolMessage::Module(module_response))?;
                let response_bytes = response_wire.len() as u64;
                self.send_to_peer(peer_addr, response_wire).await?;

                self.bandwidth_protection()
                    .record_service_bandwidth(ServiceType::ModuleServing, peer_addr, response_bytes)
                    .await;

                Ok(())
            }
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("requires payment") {
                    warn!("Module requires payment: {}", error_msg);
                }
                Err(e)
            }
        }
    }

    /// Handle GetModuleByHash request from a peer
    pub(crate) async fn handle_get_module_by_hash(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetModuleByHashMessage,
    ) -> Result<()> {
        let registry = self.module_registry().lock().await.as_ref().map(Arc::clone);
        let response = handle_get_module_by_hash(message, registry).await?;

        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::ModuleByHash(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle GetModuleList request from a peer
    pub(crate) async fn handle_get_module_list(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetModuleListMessage,
    ) -> Result<()> {
        let registry = self.module_registry().lock().await.as_ref().map(Arc::clone);
        let response = handle_get_module_list(message, registry).await?;

        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::ModuleList(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::protocol::{GetModuleListMessage, GetModuleMessage};
    use std::net::SocketAddr;

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_get_module_without_registry_errors() {
        let listen: SocketAddr = "127.0.0.1:18478".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18479".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let msg = GetModuleMessage {
            request_id: 42,
            name: "cov-mod".into(),
            version: None,
            payment_id: None,
        };
        assert!(nm.handle_get_module(peer, msg).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_get_module_list_without_registry_errors() {
        let listen: SocketAddr = "127.0.0.1:18480".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18481".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let msg = GetModuleListMessage {
            name_prefix: None,
            max_count: None,
        };
        assert!(nm.handle_get_module_list(peer, msg).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_get_module_by_hash_without_registry_errors() {
        use crate::network::protocol::GetModuleByHashMessage;

        let listen: SocketAddr = "127.0.0.1:18482".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18483".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let msg = GetModuleByHashMessage {
            request_id: 7,
            hash: [0xcd; 32],
            include_binary: false,
        };
        assert!(nm.handle_get_module_by_hash(peer, msg).await.is_err());
    }
}
