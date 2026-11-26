//! Backends: the things the proxy fronts. Today every backend is a stdio
//! MCP server (one of the nine suite tools). The trait is the seam for
//! later kinds (HTTP MCP, external APIs) without touching the proxy.

use mh_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub mod stdio;

/// Set by the proxy when the client cancels a call; a backend checks it
/// while it waits and tells its server.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Something a backend reports without being asked.
#[derive(Clone, Debug)]
pub enum BackendEvent {
    /// The server sent `notifications/tools/list_changed`: refetch its tools.
    ToolsChanged,
    /// A notification to relay to the client unchanged (for example
    /// `notifications/progress`, whose token the client chose). The value
    /// is the whole JSON-RPC notification object.
    Relay(Value),
    /// The server process ended; the text says why. The backend restarts
    /// it on the next call.
    Exited(String),
}

/// Receives events from every backend; the first argument is the backend name.
pub type EventSink = Arc<dyn Fn(&str, BackendEvent) + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
pub enum CallError {
    /// The backend could not be started or reached; the text says why and what to fix.
    Unavailable(String),
    /// The server answered with a JSON-RPC error.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The call was cancelled by the client.
    Cancelled,
    /// No answer within the backend's limit.
    Timeout(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unavailable(s) => write!(f, "unavailable: {s}"),
            CallError::Rpc { code, message, .. } => write!(f, "error {code}: {message}"),
            CallError::Cancelled => write!(f, "cancelled"),
            CallError::Timeout(s) => write!(f, "timed out: {s}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BackendStatus {
    pub up: bool,
    /// The last start or exit error, if any.
    pub error: Option<String>,
    /// `serverInfo` from the server's initialize reply.
    pub server_info: Option<Value>,
    /// `instructions` from the server's initialize reply.
    pub instructions: Option<String>,
    /// How many times the process was started.
    pub starts: u32,
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &str;

    /// Start the server and finish the MCP handshake. Idempotent: a running
    /// backend returns Ok at once. Blocks until ready or failed.
    fn start(&self) -> Result<(), CallError>;

    fn status(&self) -> BackendStatus;

    /// The server's tools, as the raw tool objects of `tools/list` (all
    /// pages). Starts the server if needed.
    fn list_tools(&self) -> Result<Vec<Value>, CallError>;

    /// Call one tool by its own (unprefixed) name. `meta` is the client's
    /// `_meta` (with any `progressToken`), passed through unchanged. Returns
    /// the `result` object of the server's reply. Blocks until the reply,
    /// cancellation, or the backend's limit. Starts the server if needed.
    fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        meta: Option<Value>,
        cancel: &CancelToken,
    ) -> Result<Value, CallError>;

    /// Stop the server process, failing any call in flight with `Unavailable`.
    fn shutdown(&self);
}

/// Build a backend from its spec. `env` is the full extra environment
/// (from `Config::backend_env`); `project` is the working directory.
pub fn build(
    spec: &mh_core::BackendSpec,
    env: Vec<(String, String)>,
    project: &Path,
    sink: EventSink,
) -> Result<Arc<dyn Backend>, String> {
    match spec.kind.as_str() {
        "stdio-mcp" => Ok(Arc::new(stdio::StdioBackend::new(spec, env, project, sink))),
        other => Err(format!(
            "backend {:?}: unknown kind {other:?} (known: {})",
            spec.name,
            mh_core::config::KNOWN_KINDS.join(", ")
        )),
    }
}
