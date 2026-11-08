//! Inbound services the node binds for a module.
pub mod host;

pub use host::{
    BindRequest, HostedModule, IpcHostedModule, ServiceHost, ServiceHttpRequest,
    ServiceHttpResponse, ServiceKind, StreamEvent, DEFAULT_MAX_CONNECTIONS,
    SERVICE_INVOCATION_TIMEOUT,
};
