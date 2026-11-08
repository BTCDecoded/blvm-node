//! Inbound services the node binds for a module.
pub mod host;

pub use host::{
    BindRequest, DEFAULT_MAX_CONNECTIONS, HostedModule, IpcHostedModule,
    SERVICE_INVOCATION_TIMEOUT, ServiceHost, ServiceHttpRequest, ServiceHttpResponse, ServiceKind,
    StreamEvent,
};
