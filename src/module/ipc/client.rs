//! IPC client for modules
//! 
//! Client-side IPC implementation that modules use to communicate with the node.
//! This will be used by module binaries to send requests and receive responses/events.

use tokio::net::UnixStream;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use futures::{SinkExt, StreamExt};
use std::path::Path;
use tracing::{debug, warn};

use crate::module::traits::ModuleError;
use crate::module::ipc::protocol::{ModuleMessage, RequestMessage, ResponseMessage, CorrelationId};

/// IPC client for modules to communicate with node
pub struct ModuleIpcClient {
    /// Framed reader for receiving messages
    reader: FramedRead<tokio::io::ReadHalf<UnixStream>, LengthDelimitedCodec>,
    /// Framed writer for sending messages
    writer: FramedWrite<tokio::io::WriteHalf<UnixStream>, LengthDelimitedCodec>,
    /// Next correlation ID to use
    next_correlation_id: CorrelationId,
}

impl ModuleIpcClient {
    /// Connect to node IPC socket
    pub async fn connect<P: AsRef<Path>>(socket_path: P) -> Result<Self, ModuleError> {
        let stream = UnixStream::connect(socket_path.as_ref())
            .await
            .map_err(|e| ModuleError::IpcError(format!("Failed to connect to socket: {}", e)))?;
        
        let (read_half, write_half) = tokio::io::split(stream);
        
        let reader = FramedRead::new(read_half, LengthDelimitedCodec::new());
        let writer = FramedWrite::new(write_half, LengthDelimitedCodec::new());
        
        debug!("Connected to node IPC socket");
        
        Ok(Self {
            reader,
            writer,
            next_correlation_id: 1,
        })
    }
    
    /// Send a request and wait for response
    pub async fn request(&mut self, request: RequestMessage) -> Result<ResponseMessage, ModuleError> {
        let correlation_id = request.correlation_id;
        
        // Serialize request
        let bytes = bincode::serialize(&ModuleMessage::Request(request))
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
        
        // Send request
        self.writer.send(bytes::Bytes::from(bytes))
            .await
            .map_err(|e| ModuleError::IpcError(format!("Failed to send request: {}", e)))?;
        
        debug!("Sent request with correlation_id={}", correlation_id);
        
        // Wait for response
        let response_bytes = self.reader
            .next()
            .await
            .ok_or_else(|| ModuleError::IpcError("Connection closed while waiting for response".to_string()))?
            .map_err(|e| ModuleError::IpcError(format!("Failed to read response: {}", e)))?;
        
        // Deserialize response
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
            _ => Err(ModuleError::IpcError("Received unexpected message type".to_string())),
        }
    }
    
    /// Receive an event message (non-blocking)
    pub async fn receive_event(&mut self) -> Result<Option<ModuleMessage>, ModuleError> {
        // Use tokio::select with a timeout to make this non-blocking
        use tokio::time::{sleep, Duration};
        
        // Try to read with a very short timeout (10ms)
        tokio::select! {
            result = self.reader.next() => {
                match result {
                    Some(Ok(bytes)) => {
                        let message: ModuleMessage = bincode::deserialize(&bytes)
                            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
                        
                        match &message {
                            ModuleMessage::Event(_) => Ok(Some(message)),
                            _ => {
                                warn!("Received non-event message in event stream");
                                Ok(None)
                            }
                        }
                    }
                    Some(Err(e)) => Err(ModuleError::IpcError(format!("Failed to read event: {}", e))),
                    None => Ok(None),
                }
            }
            _ = sleep(Duration::from_millis(10)) => {
                // Timeout - no data available
                Ok(None)
            }
        }
    }
    
    /// Get next correlation ID
    pub fn next_correlation_id(&mut self) -> CorrelationId {
        let id = self.next_correlation_id;
        self.next_correlation_id = self.next_correlation_id.wrapping_add(1);
        id
    }
}

