//! Governance message handlers (EconomicNodeRegistration, Veto, Status, ForkDecision).
//!
//! Requires governance feature.

use crate::network::network_manager::NetworkManager;
use crate::network::transport::TransportAddr;
use anyhow::Result;
use std::net::SocketAddr;
use tracing::{debug, error, info, warn};

#[cfg(feature = "governance")]
impl NetworkManager {
    /// Handle EconomicNodeRegistration message - Relay to governance node if enabled
    pub(crate) async fn handle_economic_node_registration(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeRegistrationMessage,
    ) -> Result<()> {
        let event_publisher_guard = self.event_publisher().lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let payload = EventPayload::EconomicNodeRegistered {
                node_id: msg.entity_name.clone(),
                node_type: msg.node_type.clone(),
                hashpower_percent: None,
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeRegistered, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeRegistered event: {}", e);
            }
        }

        debug!(
            "EconomicNodeRegistration received from {}: node_type={}, entity={}, message_id={}",
            peer_addr, msg.node_type, msg.entity_name, msg.message_id
        );

        if let Err(e) = self
            .replay_protection()
            .check_message_id(&msg.message_id, msg.timestamp)
            .await
        {
            warn!(
                "Replay protection: Rejected EconomicNodeRegistration from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }
        if let Err(e) = super::super::replay_protection::ReplayProtection::validate_timestamp(
            msg.timestamp,
            3600,
        ) {
            warn!(
                "Replay protection: Invalid timestamp in EconomicNodeRegistration from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        let governance_enabled = self
            .governance_config()
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if !governance_enabled {
            debug!("Governance relay disabled, gossiping message to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        if let Some(config) = self.governance_config() {
            if let Some(ref commons_url) = config.commons_url {
                let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                let api_key = env_api_key
                    .as_deref()
                    .or(config.api_key.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                let url = format!("{commons_url}/internal/governance/registration");

                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to forward registration: {}", e))?;

                if response.status().is_success() {
                    info!(
                        "Successfully forwarded EconomicNodeRegistration to blvm-commons: message_id={}",
                        msg.message_id
                    );
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to forward EconomicNodeRegistration: status={}, error={}",
                        status, error_text
                    );
                }
            } else {
                warn!("Governance enabled but commons_url not configured");
            }
        }

        self.gossip_governance_message(peer_addr, &msg).await?;

        Ok(())
    }

    /// Helper: Publish governance event and call handler, preserving return value
    pub(crate) async fn handle_governance_with_event<F, Fut>(
        &self,
        event_type: crate::module::traits::EventType,
        payload: crate::module::ipc::protocol::EventPayload,
        handler: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let event_publisher_guard = self.event_publisher().lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            if let Err(e) = event_publisher.publish_event(event_type, payload).await {
                warn!("Failed to publish governance event: {}", e);
            }
        }
        handler().await
    }

    /// Gossip a governance message to other governance-enabled peers
    pub(crate) async fn gossip_governance_message<T: serde::Serialize>(
        &self,
        sender_addr: SocketAddr,
        msg: &T,
    ) -> Result<()> {
        let pm = self.peer_manager_mutex().lock().await;
        let governance_peers = pm.get_governance_peers();
        drop(pm);

        let governance_peers: Vec<_> = governance_peers
            .into_iter()
            .filter(|(_, addr)| *addr != sender_addr)
            .collect();

        if governance_peers.is_empty() {
            debug!("No governance-enabled peers to gossip to");
            return Ok(());
        }

        let msg_json = serde_json::to_vec(msg)
            .map_err(|e| anyhow::anyhow!("Failed to serialize governance message: {}", e))?;

        let pm = self.peer_manager_mutex().lock().await;
        for (_transport_addr, peer_addr) in governance_peers {
            if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
                if let Err(e) = peer.send_message(msg_json.clone()).await {
                    debug!("Failed to gossip to peer {}: {}", peer_addr, e);
                } else {
                    debug!("Gossiped governance message to peer {}", peer_addr);
                }
            }
        }

        Ok(())
    }

    /// Handle EconomicNodeVeto message - Relay to governance node if enabled
    pub(crate) async fn handle_economic_node_veto(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeVetoMessage,
    ) -> Result<()> {
        let event_publisher_guard = self.event_publisher().lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let payload = EventPayload::EconomicNodeVeto {
                proposal_id: format!("{}", msg.pr_id),
                node_id: msg.node_id.map(|id| id.to_string()).unwrap_or_default(),
                reason: msg.signal_type.clone(),
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeVeto, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeVeto event: {}", e);
            }
        }

        debug!(
            "EconomicNodeVeto received from {}: pr_id={}, signal={}, message_id={}",
            peer_addr, msg.pr_id, msg.signal_type, msg.message_id
        );

        if let Err(e) = self
            .replay_protection()
            .check_message_id(&msg.message_id, msg.timestamp)
            .await
        {
            warn!(
                "Replay protection: Rejected EconomicNodeVeto from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }
        if let Err(e) = super::super::replay_protection::ReplayProtection::validate_timestamp(
            msg.timestamp,
            3600,
        ) {
            warn!(
                "Replay protection: Invalid timestamp in EconomicNodeVeto from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        let governance_enabled = self
            .governance_config()
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if !governance_enabled {
            debug!("Governance relay disabled, gossiping message to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        if let Some(config) = self.governance_config() {
            if let Some(ref commons_url) = config.commons_url {
                let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                let api_key = env_api_key
                    .as_deref()
                    .or(config.api_key.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                let url = format!("{commons_url}/internal/governance/veto");

                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to forward veto: {}", e))?;

                if response.status().is_success() {
                    info!(
                        "Successfully forwarded EconomicNodeVeto to blvm-commons: message_id={}",
                        msg.message_id
                    );
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to forward EconomicNodeVeto: status={}, error={}",
                        status, error_text
                    );
                }
            } else {
                warn!("Governance enabled but commons_url not configured");
            }
        }

        self.gossip_governance_message(peer_addr, &msg).await?;

        Ok(())
    }

    /// Handle EconomicNodeStatus message - Query or respond with node status
    pub(crate) async fn handle_economic_node_status(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeStatusMessage,
    ) -> Result<()> {
        debug!(
            "EconomicNodeStatus received from {}: request_id={}, query_type={}, identifier={}",
            peer_addr, msg.request_id, msg.query_type, msg.node_identifier
        );

        let event_publisher_guard = self.event_publisher().lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let response_data = msg
                .status
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_default());
            let payload = EventPayload::EconomicNodeStatus {
                request_id: msg.request_id.to_string(),
                query_type: msg.query_type.clone(),
                node_id: Some(msg.node_identifier.clone()),
                response_data,
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeStatus, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeStatus event: {}", e);
            }
        }

        if msg.status.is_some() {
            debug!("EconomicNodeStatus response, relaying to governance peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        let governance_enabled = self
            .governance_config()
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if governance_enabled {
            if let Some(config) = self.governance_config() {
                if let Some(ref commons_url) = config.commons_url {
                    let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                    let api_key = env_api_key
                        .as_deref()
                        .or(config.api_key.as_deref())
                        .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .build()
                        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                    let url = format!("{commons_url}/internal/governance/status");

                    info!(
                        "Forwarding EconomicNodeStatus query to blvm-commons: request_id={}, query_type={}",
                        msg.request_id, msg.query_type
                    );

                    let request_payload = serde_json::json!({
                        "request_id": msg.request_id,
                        "node_identifier": msg.node_identifier,
                        "query_type": msg.query_type,
                        "status": null
                    });

                    let response = client
                        .post(&url)
                        .header("X-API-Key", api_key)
                        .json(&request_payload)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to forward status query: {}", e))?;

                    if response.status().is_success() {
                        let status_response: serde_json::Value =
                            response.json().await.map_err(|e| {
                                anyhow::anyhow!("Failed to parse status response: {}", e)
                            })?;

                        let status_data = status_response.get("status").and_then(|s| s.as_object());
                        let response_status = if let Some(data) = status_data {
                            Some(crate::network::protocol::NodeStatusResponse {
                                node_id: data
                                    .get("node_id")
                                    .and_then(|v| v.as_i64())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid node_id"))?
                                    as i32,
                                node_type: data
                                    .get("node_type")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid node_type"))?
                                    .to_string(),
                                entity_name: data
                                    .get("entity_name")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("Missing or invalid entity_name")
                                    })?
                                    .to_string(),
                                status: data
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid status"))?
                                    .to_string(),
                                weight: data
                                    .get("weight")
                                    .and_then(|v| v.as_f64())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid weight"))?,
                                registered_at: data
                                    .get("registered_at")
                                    .and_then(|v| v.as_i64())
                                    .ok_or_else(|| {
                                    anyhow::anyhow!("Missing or invalid registered_at")
                                })?,
                                last_verified_at: data
                                    .get("last_verified_at")
                                    .and_then(|v| v.as_i64()),
                            })
                        } else {
                            None
                        };

                        let response_msg = crate::network::protocol::EconomicNodeStatusMessage {
                            request_id: msg.request_id,
                            node_identifier: msg.node_identifier.clone(),
                            query_type: msg.query_type.clone(),
                            status: response_status,
                        };

                        let pm = self.peer_manager_mutex().lock().await;
                        if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
                            let msg_bytes = bincode::serialize(
                                &crate::network::protocol::ProtocolMessage::EconomicNodeStatus(
                                    response_msg,
                                ),
                            )
                            .map_err(|e| anyhow::anyhow!("Failed to serialize response: {}", e))?;
                            if let Err(e) = peer.send_message(msg_bytes).await {
                                warn!(
                                    "Failed to send status response to peer {}: {}",
                                    peer_addr, e
                                );
                            } else {
                                info!(
                                    "Sent status response to peer {} for request_id={}",
                                    peer_addr, msg.request_id
                                );
                            }
                        }
                    } else {
                        let status = response.status();
                        let error_text = response.text().await.unwrap_or_default();
                        error!(
                            "Failed to forward EconomicNodeStatus query: status={}, error={}",
                            status, error_text
                        );
                    }
                } else {
                    warn!("Governance enabled but commons_url not configured");
                }
            }
        } else {
            debug!("Governance relay disabled, gossiping query to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
        }

        Ok(())
    }

    /// Handle EconomicNodeForkDecision message - Relay to governance node if enabled
    pub(crate) async fn handle_economic_node_fork_decision(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeForkDecisionMessage,
    ) -> Result<()> {
        debug!(
            "EconomicNodeForkDecision received from {}: ruleset={}, message_id={}",
            peer_addr, msg.chosen_ruleset, msg.message_id
        );

        let event_publisher_guard = self.event_publisher().lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let node_id = msg
                .node_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| msg.public_key.clone());
            let decision = if msg.chosen_ruleset.is_empty() {
                "abstain".to_string()
            } else {
                "adopt".to_string()
            };
            let payload = EventPayload::EconomicNodeForkDecision {
                message_id: msg.message_id.clone(),
                ruleset_version: msg.chosen_ruleset.clone(),
                decision,
                node_id,
                timestamp: msg.timestamp as u64,
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeForkDecision, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeForkDecision event: {}", e);
            }
        }

        if let Err(e) = self
            .replay_protection()
            .check_message_id(&msg.message_id, msg.timestamp)
            .await
        {
            warn!(
                "Replay protection: Rejected EconomicNodeForkDecision from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }
        if let Err(e) = super::super::replay_protection::ReplayProtection::validate_timestamp(
            msg.timestamp,
            3600,
        ) {
            warn!(
                "Replay protection: Invalid timestamp in EconomicNodeForkDecision from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        let governance_enabled = self
            .governance_config()
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if !governance_enabled {
            debug!("Governance relay disabled, gossiping fork decision to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        if let Some(config) = self.governance_config() {
            if let Some(ref commons_url) = config.commons_url {
                let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                let api_key = env_api_key
                    .as_deref()
                    .or(config.api_key.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                let url = format!("{commons_url}/internal/governance/fork-decision");

                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to forward fork decision: {}", e))?;

                if response.status().is_success() {
                    info!(
                        "Successfully forwarded EconomicNodeForkDecision to blvm-commons: message_id={}",
                        msg.message_id
                    );
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to forward EconomicNodeForkDecision: status={}, error={}",
                        status, error_text
                    );
                }
            } else {
                warn!("Governance enabled but commons_url not configured");
            }
        }

        self.gossip_governance_message(peer_addr, &msg).await?;

        Ok(())
    }
}
