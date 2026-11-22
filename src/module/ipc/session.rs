//! One reader owns the module session after handshake.

use super::ModuleIpcClient;
use super::protocol::{
    InvocationMessage, InvocationResultMessage, ModuleMessage, RequestMessage, ResponseMessage,
};
use crate::module::traits::ModuleError;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};

pub(crate) type SessionRequest = (RequestMessage, oneshot::Sender<ResponseMessage>);
type Invocation = (InvocationMessage, oneshot::Sender<InvocationResultMessage>);

fn invocation_error(correlation_id: u64) -> InvocationResultMessage {
    InvocationResultMessage {
        correlation_id,
        success: false,
        payload: None,
        error: Some("Module invocation handler unavailable".to_owned()),
    }
}

pub(crate) async fn drive_session(
    mut client: ModuleIpcClient,
    mut requests: mpsc::Receiver<SessionRequest>,
    events: &broadcast::Sender<ModuleMessage>,
    invocations: mpsc::Sender<Invocation>,
) -> Result<(), ModuleError> {
    let mut pending: Option<(u64, oneshot::Sender<ResponseMessage>)> = None;
    let mut invocation_results = FuturesUnordered::new();

    loop {
        tokio::select! {
            request = requests.recv(), if pending.is_none() => {
                let Some((mut request, reply)) = request else { return Ok(()) };
                if reply.is_closed() {
                    continue;
                }
                request.correlation_id = client.next_correlation_id();
                pending = Some((request.correlation_id, reply));
                client.send_request(request).await?;
            }
            Some(result) = invocation_results.next(), if !invocation_results.is_empty() => {
                client.send_invocation_result(result).await?;
            }
            message = client.receive_message() => match message? {
                Some(ModuleMessage::Response(response)) => {
                    let Some((id, reply)) = pending.take() else {
                        return Err(ModuleError::IpcError("Unexpected response on module session".into()));
                    };
                    if id != response.correlation_id {
                        return Err(ModuleError::IpcError(format!(
                            "Correlation ID mismatch: expected {id}, got {}", response.correlation_id
                        )));
                    }
                    let _ = reply.send(response);
                }
                Some(message @ ModuleMessage::Event(_)) => {
                    let _ = events.send(message);
                }
                Some(ModuleMessage::Invocation(invocation)) => {
                    let id = invocation.correlation_id;
                    let (tx, rx) = oneshot::channel();
                    if invocation_results.len() >= 32 || invocations.try_send((invocation, tx)).is_err() {
                        client.send_invocation_result(invocation_error(id)).await?;
                    } else {
                        invocation_results.push(async move { rx.await.unwrap_or_else(|_| invocation_error(id)) });
                    }
                }
                Some(_) => {}
                None => return Ok(()),
            },
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::module::ipc::{
        EventMessage, EventPayload, MessageType, module_ipc_length_codec,
        protocol::{RequestPayload, ResponsePayload},
    };
    use crate::module::traits::EventType;
    use futures::SinkExt;
    use std::time::Duration;
    use tokio::net::UnixListener;
    use tokio_util::codec::{FramedRead, FramedWrite};

    #[tokio::test]
    async fn routes_event_before_response_and_keeps_idle_session_writable() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("module.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let client = ModuleIpcClient::connect(&path).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();
            let (read, write) = server.into_split();
            let mut read = FramedRead::new(read, module_ipc_length_codec());
            let mut write = FramedWrite::new(write, module_ipc_length_codec());

            let (requests, rx) = mpsc::channel(1);
            let (events, mut observed) = broadcast::channel(4);
            let (invocations, _invocation_rx) = mpsc::channel(1);
            let session =
                tokio::spawn(async move { drive_session(client, rx, &events, invocations).await });
            tokio::task::yield_now().await;

            let (reply, received) = oneshot::channel();
            requests
                .send((
                    RequestMessage {
                        correlation_id: 0,
                        request_type: MessageType::GetBlockHeight,
                        payload: RequestPayload::GetBlockHeight,
                    },
                    reply,
                ))
                .await
                .unwrap();

            let bytes = read.next().await.unwrap().unwrap();
            let ModuleMessage::Request(request) = bincode::deserialize(&bytes).unwrap() else {
                panic!("expected a request");
            };
            let frames = [
                ModuleMessage::Event(EventMessage {
                    event_type: EventType::NewBlock,
                    payload: EventPayload::NewBlock {
                        block_hash: [7; 32],
                        height: 2,
                    },
                }),
                ModuleMessage::Response(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                )),
            ];
            for frame in frames {
                write
                    .send(bincode::serialize(&frame).unwrap().into())
                    .await
                    .unwrap();
            }
            assert!(matches!(
                observed.recv().await.unwrap(),
                ModuleMessage::Event(EventMessage {
                    payload: EventPayload::NewBlock { height: 2, .. },
                    ..
                })
            ));
            assert!(received.await.unwrap().success);
            drop(requests);
            session.await.unwrap().unwrap();
        })
        .await
        .expect("IPC event and response were not delivered");
    }
}
