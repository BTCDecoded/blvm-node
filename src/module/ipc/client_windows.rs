//! IPC client for modules (Windows named pipes)

use futures::{SinkExt, StreamExt};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::debug;

use crate::module::ipc::protocol::{
    CorrelationId, InvocationResultMessage, ModuleMessage, RequestMessage, ResponseMessage,
};
use crate::module::traits::ModuleError;

fn path_to_pipe_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("blvm-module");
    let safe = stem.replace(|c: char| !c.is_alphanumeric(), "-");
    format!(r"\\.\pipe\blvm-{}", safe)
}

/// IPC client for modules (Windows: named pipes)
pub struct ModuleIpcClient {
    reader: FramedRead<
        tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
        LengthDelimitedCodec,
    >,
    writer: FramedWrite<
        tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
        LengthDelimitedCodec,
    >,
    next_correlation_id: CorrelationId,
}

impl ModuleIpcClient {
    /// Connect to node IPC (named pipe)
    pub async fn connect<P: AsRef<Path>>(path: P) -> Result<Self, ModuleError> {
        let pipe_name = path_to_pipe_name(path.as_ref());
        let client = ClientOptions::new()
            .open(&pipe_name)
            .map_err(|e| ModuleError::IpcError(format!("Failed to connect to pipe: {e}")))?;

        let (read_half, write_half) = tokio::io::split(client);
        let reader = FramedRead::new(read_half, LengthDelimitedCodec::new());
        let writer = FramedWrite::new(write_half, LengthDelimitedCodec::new());

        debug!("Connected to node IPC pipe: {}", pipe_name);

        Ok(Self {
            reader,
            writer,
            next_correlation_id: 1,
        })
    }

    /// Send a request and wait for response
    pub async fn request(
        &mut self,
        request: RequestMessage,
    ) -> Result<ResponseMessage, ModuleError> {
        let correlation_id = request.correlation_id;

        let bytes = bincode::serialize(&ModuleMessage::Request(request))
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

        self.writer
            .send(bytes::Bytes::from(bytes))
            .await
            .map_err(|e| ModuleError::IpcError(format!("Failed to send request: {e}")))?;

        debug!("Sent request with correlation_id={}", correlation_id);

        let response_bytes = self
            .reader
            .next()
            .await
            .ok_or_else(|| {
                ModuleError::IpcError("Connection closed while waiting for response".to_string())
            })?
            .map_err(|e| ModuleError::IpcError(format!("Failed to read response: {e}")))?;

        let message: ModuleMessage = bincode::deserialize(&response_bytes)
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

        match message {
            ModuleMessage::Response(resp) => {
                if resp.correlation_id == correlation_id {
                    Ok(resp)
                } else {
                    Err(ModuleError::IpcError(format!(
                        "Correlation ID mismatch: expected {}, got {}",
                        correlation_id, resp.correlation_id
                    )))
                }
            }
            _ => Err(ModuleError::IpcError(
                "Received unexpected message type".to_string(),
            )),
        }
    }

    /// Send a log message to the node
    pub async fn send_log(
        &mut self,
        level: crate::module::ipc::protocol::LogLevel,
        module_id: &str,
        message: &str,
        target: Option<&str>,
    ) -> Result<(), ModuleError> {
        use crate::module::ipc::protocol::{LogMessage, ModuleMessage};
        use crate::utils::current_timestamp;

        let log_message = LogMessage {
            level,
            module_id: module_id.to_string(),
            message: message.to_string(),
            target: target.unwrap_or("module").to_string(),
            timestamp: current_timestamp(),
        };

        let bytes = bincode::serialize(&ModuleMessage::Log(log_message))
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

        self.writer
            .send(bytes::Bytes::from(bytes))
            .await
            .map_err(|e| ModuleError::IpcError(format!("Failed to send log: {e}")))?;

        Ok(())
    }

    /// Receive the next message
    pub async fn receive_message(&mut self) -> Result<Option<ModuleMessage>, ModuleError> {
        match self.reader.next().await {
            Some(Ok(bytes)) => {
                let message: ModuleMessage = bincode::deserialize(&bytes)
                    .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
                Ok(Some(message))
            }
            Some(Err(e)) => Err(ModuleError::IpcError(format!(
                "Failed to read message: {e}"
            ))),
            None => Ok(None),
        }
    }

    /// Send invocation result back to the node
    pub async fn send_invocation_result(
        &mut self,
        result: InvocationResultMessage,
    ) -> Result<(), ModuleError> {
        let bytes = bincode::serialize(&ModuleMessage::InvocationResult(result))
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
        self.writer
            .send(bytes::Bytes::from(bytes))
            .await
            .map_err(|e| ModuleError::IpcError(format!("Failed to send invocation result: {e}")))?;
        Ok(())
    }

    /// Receive an event message (non-blocking)
    pub async fn receive_event(&mut self) -> Result<Option<ModuleMessage>, ModuleError> {
        use tokio::time::{sleep, Duration};

        tokio::select! {
            result = self.reader.next() => {
                match result {
                    Some(Ok(bytes)) => {
                        let message: ModuleMessage = bincode::deserialize(&bytes)
                            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
                        match &message {
                            ModuleMessage::Event(_) => Ok(Some(message)),
                            _ => {
                                tracing::warn!("Received non-event message in event stream");
                                Ok(None)
                            }
                        }
                    }
                    Some(Err(e)) => Err(ModuleError::IpcError(format!("Failed to read event: {e}"))),
                    None => Ok(None),
                }
            }
            _ = sleep(Duration::from_millis(10)) => Ok(None),
        }
    }

    /// Get next correlation ID
    pub fn next_correlation_id(&mut self) -> CorrelationId {
        let id = self.next_correlation_id;
        self.next_correlation_id = self.next_correlation_id.wrapping_add(1);
        id
    }
}
