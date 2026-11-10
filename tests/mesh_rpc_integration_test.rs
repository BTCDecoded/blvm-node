//! Mesh RPC roundtrip via in-process ModuleAPI registry.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use blvm_node::module::inter_module::ModuleApiRegistry;
use blvm_node::module::inter_module::api::ModuleAPI;
use blvm_node::module::inter_module::router::ModuleRouter;
use blvm_node::module::traits::ModuleError;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const APP_PROTOCOL: &str = "app-ukm-v1";

/// Bincode-compatible with blvm-mesh `PaymentProof` (without `ctv` feature).
#[derive(Debug, Clone, Serialize, Deserialize)]
enum WirePaymentProof {
    Lightning {
        invoice: String,
        preimage: [u8; 32],
        amount_msats: u64,
        timestamp: u64,
        expires_at: u64,
    },
    OnChainSettlement {
        payment_request_id: String,
        tx_hash: [u8; 32],
        amount_sats: u64,
        timestamp: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendPacketRequest {
    destination: [u8; 32],
    payload: Vec<u8>,
    payment_proof: Option<WirePaymentProof>,
    protocol_id: Option<String>,
    ttl: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendPacketResponse {
    success: bool,
    packet_id: [u8; 32],
    route_length: usize,
    estimated_cost_sats: u64,
    error: Option<String>,
}

#[derive(Default)]
struct MockMeshModule {
    deliveries: Mutex<VecDeque<LocalDelivery>>,
}

#[derive(Clone, Debug)]
struct LocalDelivery {
    source: [u8; 32],
    payload: Vec<u8>,
    protocol_id: String,
}

#[async_trait]
impl ModuleAPI for MockMeshModule {
    async fn handle_request(
        &self,
        method: &str,
        params: &[u8],
        _caller: &str,
    ) -> Result<Vec<u8>, ModuleError> {
        match method {
            "send_packet" => {
                let req: SendPacketRequest = bincode::deserialize(params)
                    .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

                if req.destination == [0u8; 32] {
                    return bincode::serialize(&SendPacketResponse {
                        success: false,
                        packet_id: [0u8; 32],
                        route_length: 0,
                        estimated_cost_sats: 0,
                        error: Some("invalid destination".into()),
                    })
                    .map_err(|e| ModuleError::SerializationError(e.to_string()));
                }

                self.deliveries.lock().await.push_back(LocalDelivery {
                    source: [0xAA; 32],
                    payload: req.payload,
                    protocol_id: req.protocol_id.unwrap_or_default(),
                });

                Ok(bincode::serialize(&SendPacketResponse {
                    success: true,
                    packet_id: [0xBB; 32],
                    route_length: 1,
                    estimated_cost_sats: 0,
                    error: None,
                })
                .map_err(|e| ModuleError::SerializationError(e.to_string()))?)
            }
            "poll_local_deliveries" => {
                #[derive(Deserialize)]
                struct PollRequest {
                    protocol_id: Option<String>,
                    max_packets: Option<usize>,
                }
                #[derive(Serialize)]
                struct PolledPacket {
                    source: [u8; 32],
                    payload: Vec<u8>,
                    protocol_id: String,
                }

                let req: PollRequest = bincode::deserialize(params)
                    .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
                let max = req.max_packets.unwrap_or(8).min(64);
                let filter = req.protocol_id.as_deref();

                let mut out = Vec::new();
                let mut queue = self.deliveries.lock().await;
                let mut retained = VecDeque::new();
                while let Some(item) = queue.pop_front() {
                    if out.len() >= max {
                        retained.push_back(item);
                        continue;
                    }
                    if filter.is_some_and(|p| p != item.protocol_id) {
                        retained.push_back(item);
                        continue;
                    }
                    out.push(PolledPacket {
                        source: item.source,
                        payload: item.payload,
                        protocol_id: item.protocol_id,
                    });
                }
                *queue = retained;

                bincode::serialize(&out).map_err(|e| ModuleError::SerializationError(e.to_string()))
            }
            _ => Err(ModuleError::OperationError(format!(
                "unknown method {method}"
            ))),
        }
    }

    fn list_methods(&self) -> Vec<String> {
        vec!["send_packet".into(), "poll_local_deliveries".into()]
    }

    fn api_version(&self) -> u32 {
        1
    }
}

#[tokio::test]
async fn mesh_send_packet_poll_local_deliveries_roundtrip() {
    let registry = Arc::new(ModuleApiRegistry::new());
    let mesh = Arc::new(MockMeshModule::default());
    registry
        .register_api("blvm-mesh".into(), mesh)
        .await
        .unwrap();

    let router = ModuleRouter::new(registry);

    let payload = b"hello-mesh-app".to_vec();
    let send_req = SendPacketRequest {
        destination: [1u8; 32],
        payload: payload.clone(),
        payment_proof: None,
        protocol_id: Some(APP_PROTOCOL.into()),
        ttl: Some(3600),
    };
    let send_bytes = bincode::serialize(&send_req).unwrap();

    let send_resp = router
        .route_call("rpc", Some("blvm-mesh"), "send_packet", &send_bytes)
        .await
        .unwrap();
    let send: SendPacketResponse = bincode::deserialize(&send_resp).unwrap();
    assert!(send.success);

    #[derive(Serialize)]
    struct PollRequest {
        protocol_id: Option<String>,
        max_packets: Option<usize>,
    }
    let poll_req = PollRequest {
        protocol_id: Some(APP_PROTOCOL.into()),
        max_packets: Some(8),
    };
    let poll_bytes = bincode::serialize(&poll_req).unwrap();

    let poll_resp = router
        .route_call(
            "rpc",
            Some("blvm-mesh"),
            "poll_local_deliveries",
            &poll_bytes,
        )
        .await
        .unwrap();

    #[derive(Deserialize)]
    struct PolledPacket {
        source: [u8; 32],
        payload: Vec<u8>,
        protocol_id: String,
    }
    let packets: Vec<PolledPacket> = bincode::deserialize(&poll_resp).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].payload, payload);
    assert_eq!(packets[0].protocol_id, APP_PROTOCOL);
    assert_eq!(packets[0].source, [0xAA; 32]);
}
