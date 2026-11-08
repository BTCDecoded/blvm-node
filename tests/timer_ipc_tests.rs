//! Timer service over the real module IPC server.
//!
//! The test module speaks the wire protocol directly. No SDK.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use blvm_node::module::api::node_api::NodeApiImpl;
use blvm_node::module::ipc::client::ModuleIpcClient;
use blvm_node::module::ipc::protocol::{
    InvocationResultMessage, InvocationResultPayload, InvocationType, MessageType, ModuleMessage,
    RequestMessage, RequestPayload, ResponsePayload,
};
use blvm_node::module::ipc::server::{IpcTimerTransport, ModuleIpcServer};
use blvm_node::module::timers::service::TimerService;
use blvm_node::module::timers::types::{
    monotonic_interval, CreateMode, First, Pacing, Repeat, TimerAck, TimerConfig, TimerState,
    LEGACY_TIMER_UNSUPPORTED,
};
use blvm_node::storage::Storage;
use tempfile::TempDir;
use tokio::task::JoinHandle;

struct Running {
    _dir: TempDir,
    socket: std::path::PathBuf,
    server: JoinHandle<Result<(), blvm_node::module::traits::ModuleError>>,
}

async fn start_server() -> Running {
    let dir = TempDir::new_in("/tmp").expect("temp dir");
    let socket = dir.path().join("s.sock");
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let mut api = NodeApiImpl::new(storage);
    let mut config = TimerConfig::default();
    config.min_period_ms = 1;
    let service = TimerService::spawn(config, std::collections::HashMap::new());
    api.set_timer_service(service.clone());
    let mut server = ModuleIpcServer::new(&socket).with_timer_service(service.clone());
    let ipc = server.handle();
    service
        .set_transport(Arc::new(IpcTimerTransport::new(ipc)))
        .await;
    let api = Arc::new(api);
    let server = tokio::spawn(async move { server.start(api).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        if server.is_finished() || tokio::time::Instant::now() > deadline {
            panic!("timer ipc server did not bind {:?}", socket);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Running {
        _dir: dir,
        socket,
        server,
    }
}

fn module_id(name: &str, suffix: &str) -> String {
    format!("{name}_{suffix}")
}

async fn connect(running: &Running, name: &str, suffix: &str) -> ModuleIpcClient {
    let mut client = ModuleIpcClient::connect(&running.socket).await.unwrap();
    let correlation_id = client.next_correlation_id();
    let response = client
        .request(RequestMessage {
            correlation_id,
            request_type: MessageType::Handshake,
            payload: RequestPayload::Handshake {
                module_id: module_id(name, suffix),
                module_name: name.to_string(),
                version: "0.0.0".to_string(),
            },
        })
        .await
        .unwrap();
    assert!(response.success, "{:?}", response.error);
    client
}

#[tokio::test]
async fn five_fires_then_stop() {
    let running = start_server().await;
    let mut client = connect(&running, "timerfees", &"ab".repeat(16)).await;
    let mut spec = monotonic_interval(Duration::from_millis(30), Pacing::FixedRate);
    spec.first = First::Delay(Duration::from_millis(150));
    if let Repeat::Interval {
        ref mut max_fires, ..
    } = spec.repeat
    {
        *max_fires = Some(5);
    }
    let correlation_id = client.next_correlation_id();
    let created = client
        .request(RequestMessage {
            correlation_id,
            request_type: MessageType::TimerCreate,
            payload: RequestPayload::TimerCreate {
                spec,
                mode: CreateMode::CreateOnly,
            },
        })
        .await
        .unwrap();
    assert!(created.success, "{:?}", created.error);
    assert!(matches!(
        created.payload,
        Some(ResponsePayload::TimerCreated(_))
    ));

    for i in 0..5 {
        let message = tokio::time::timeout(Duration::from_secs(2), client.receive_message())
            .await
            .expect("fire timed out")
            .unwrap()
            .expect("connection closed");
        let ModuleMessage::Invocation(invocation) = message else {
            panic!("expected a timer invocation, got {message:?}");
        };
        let InvocationType::Timer(fire) = invocation.invocation_type else {
            panic!("expected Timer invocation");
        };
        assert_eq!(fire.seq, i as u64 + 1);
        assert_eq!(fire.is_last, i == 4);
        let ack = if i == 4 {
            TimerAck::Stop
        } else {
            TimerAck::Continue
        };
        client
            .send_invocation_result(InvocationResultMessage {
                correlation_id: invocation.correlation_id,
                success: true,
                payload: Some(InvocationResultPayload::Timer(ack)),
                error: None,
            })
            .await
            .unwrap();
    }

    let mut leftover = Vec::new();
    for _ in 0..50 {
        let correlation_id = client.next_correlation_id();
        let listed = client
            .request(RequestMessage {
                correlation_id,
                request_type: MessageType::TimerList,
                payload: RequestPayload::TimerList,
            })
            .await
            .unwrap();
        leftover = match listed.payload {
            Some(ResponsePayload::TimerList(list)) => list,
            other => panic!("expected a timer list, got {other:?}"),
        };
        if leftover.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(leftover.is_empty(), "{leftover:?}");
    running.server.abort();
}

#[tokio::test]
async fn silent_module_trips_the_breaker_and_other_modules_still_fire() {
    let running = start_server().await;
    let mut silent = connect(&running, "silentmod", &"cd".repeat(16)).await;
    let mut other = connect(&running, "othermod", &"ef".repeat(16)).await;

    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedDelay);
    spec.first = First::Delay(Duration::from_millis(100));
    spec.delivery.ack_timeout = Duration::from_secs(1);
    spec.delivery.max_consecutive_failures = 3;
    let correlation_id = silent.next_correlation_id();
    let created = silent
        .request(RequestMessage {
            correlation_id,
            request_type: MessageType::TimerCreate,
            payload: RequestPayload::TimerCreate {
                spec,
                mode: CreateMode::CreateOnly,
            },
        })
        .await
        .unwrap();
    assert!(created.success, "{:?}", created.error);
    let timer_id = match created.payload {
        Some(ResponsePayload::TimerCreated(info)) => info.timer_id,
        other => panic!("expected TimerCreated, got {other:?}"),
    };

    let mut other_spec = monotonic_interval(Duration::from_millis(40), Pacing::FixedRate);
    other_spec.first = First::Delay(Duration::from_millis(100));
    if let Repeat::Interval {
        ref mut max_fires, ..
    } = other_spec.repeat
    {
        *max_fires = Some(4);
    }
    let correlation_id = other.next_correlation_id();
    let created = other
        .request(RequestMessage {
            correlation_id,
            request_type: MessageType::TimerCreate,
            payload: RequestPayload::TimerCreate {
                spec: other_spec,
                mode: CreateMode::CreateOnly,
            },
        })
        .await
        .unwrap();
    assert!(created.success, "{:?}", created.error);

    let other_task = tokio::spawn(async move {
        let mut seen = 0u32;
        while seen < 4 {
            let message = tokio::time::timeout(Duration::from_secs(3), other.receive_message())
                .await
                .expect("other module fire timed out")
                .unwrap()
                .expect("other connection closed");
            let ModuleMessage::Invocation(invocation) = message else {
                continue;
            };
            if !matches!(invocation.invocation_type, InvocationType::Timer(_)) {
                continue;
            }
            seen += 1;
            other
                .send_invocation_result(InvocationResultMessage {
                    correlation_id: invocation.correlation_id,
                    success: true,
                    payload: Some(InvocationResultPayload::Timer(TimerAck::Continue)),
                    error: None,
                })
                .await
                .unwrap();
        }
        seen
    });

    for _ in 0..3 {
        let message = tokio::time::timeout(Duration::from_secs(3), silent.receive_message())
            .await
            .expect("silent fire timed out")
            .unwrap()
            .expect("silent connection closed");
        assert!(matches!(
            message,
            ModuleMessage::Invocation(ref invocation)
                if matches!(invocation.invocation_type, InvocationType::Timer(_))
        ));
    }

    // The third fire's acknowledgement window is one second. The breaker trips
    // on that timeout and does not schedule another fire.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let correlation_id = silent.next_correlation_id();
    let got = silent
        .request(RequestMessage {
            correlation_id,
            request_type: MessageType::TimerGet,
            payload: RequestPayload::TimerGet {
                target: blvm_node::module::timers::types::TimerTarget::Id(timer_id),
            },
        })
        .await
        .unwrap();
    match got.payload {
        Some(ResponsePayload::TimerInfo(info)) => {
            assert_eq!(info.state, TimerState::PausedByBreaker);
            assert_eq!(info.in_flight, 0);
        }
        other => panic!("expected TimerInfo, got {other:?} error={:?}", got.error),
    }
    assert_eq!(other_task.await.unwrap(), 4);
    running.server.abort();
}

#[tokio::test]
async fn legacy_timer_payloads_are_unsupported() {
    let running = start_server().await;
    let mut client = connect(&running, "legacy", &"11".repeat(16)).await;
    for (request_type, payload) in [
        (
            MessageType::RegisterTimer,
            RequestPayload::RegisterTimer {
                interval_seconds: 1,
            },
        ),
        (
            MessageType::CancelTimer,
            RequestPayload::CancelTimer { timer_id: 1 },
        ),
        (
            MessageType::ScheduleTask,
            RequestPayload::ScheduleTask { delay_seconds: 1 },
        ),
    ] {
        let correlation_id = client.next_correlation_id();
        let response = client
            .request(RequestMessage {
                correlation_id,
                request_type,
                payload,
            })
            .await
            .unwrap();
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some(LEGACY_TIMER_UNSUPPORTED)
        );
    }
    running.server.abort();
}
