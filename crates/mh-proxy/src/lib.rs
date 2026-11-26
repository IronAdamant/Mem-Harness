//! The MCP server Claude Code talks to. It fronts every backend (one MCP
//! server per suite tool) as one server, plus native `harness_*` tools.
//!
//! Protocol: MCP over stdio, newline-delimited JSON-RPC 2.0. Input is read
//! on the calling thread; every tool call runs on its own thread, so a slow
//! call never blocks another. All output goes through one mutex, one
//! message per line, flushed after each.

mod catalog;
mod server;
mod text;

use mh_backend::{Backend, BackendStatus, CallError, CancelToken, EventSink};
use mh_core::Config;
use mh_json::Value;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

pub use server::{DEFAULT_PROTOCOL, PROTOCOL_VERSIONS};
pub use text::{instructions, INSTRUCTIONS_MAX};

/// A tool the harness answers itself.
#[derive(Clone)]
pub struct NativeTool {
    /// The full MCP tool object: `name` (already `harness_*`),
    /// `description`, `inputSchema`.
    pub def: Value,
    /// Gets the call's `arguments` object; returns the MCP `result` object
    /// (`{content:[...]}`), or Err(text), which becomes an `isError` result.
    pub run: Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>,
}

/// Serve MCP on `input`/`output` with one backend per `cfg.backends` entry
/// (built by `mh_backend::build`, run in `project`). Returns on input EOF
/// after shutting the backends down.
pub fn serve<R: BufRead, W: Write + Send + 'static>(
    cfg: Config,
    project: PathBuf,
    natives: Vec<NativeTool>,
    input: R,
    output: W,
) -> Result<(), String> {
    let plan: Vec<_> = cfg
        .backends
        .iter()
        .map(|spec| (spec.clone(), cfg.backend_env(&spec.name)))
        .collect();
    let make = move |sink: EventSink| {
        plan.into_iter()
            .map(
                |(spec, env)| match mh_backend::build(&spec, env, &project, sink.clone()) {
                    Ok(b) => b,
                    Err(e) => Arc::new(Broken {
                        name: spec.name.clone(),
                        error: e,
                    }) as Arc<dyn Backend>,
                },
            )
            .collect()
    };
    serve_with(cfg, natives, make, input, output)
}

/// [`serve`] with the backends made by `make_backends`, which gets the sink
/// every backend reports its events to. For tests and other kinds of backend.
pub fn serve_with<R: BufRead, W: Write + Send + 'static>(
    cfg: Config,
    natives: Vec<NativeTool>,
    make_backends: impl FnOnce(EventSink) -> Vec<Arc<dyn Backend>>,
    input: R,
    output: W,
) -> Result<(), String> {
    server::run(cfg, natives, make_backends, input, output)
}

/// Stands in for a backend that could not even be built, so the failure
/// shows in `harness_status` and in every call instead of vanishing.
struct Broken {
    name: String,
    error: String,
}

impl Backend for Broken {
    fn name(&self) -> &str {
        &self.name
    }
    fn start(&self) -> Result<(), CallError> {
        Err(CallError::Unavailable(self.error.clone()))
    }
    fn status(&self) -> BackendStatus {
        BackendStatus {
            error: Some(self.error.clone()),
            ..BackendStatus::default()
        }
    }
    fn list_tools(&self) -> Result<Vec<Value>, CallError> {
        self.start().map(|_| Vec::new())
    }
    fn call_tool(
        &self,
        _: &str,
        _: Value,
        _: Option<Value>,
        _: &CancelToken,
    ) -> Result<Value, CallError> {
        self.start().map(|_| Value::Null)
    }
    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_instructions_fit() {
        let text = instructions(&Config::builtin());
        assert!(text.len() <= INSTRUCTIONS_MAX, "{} bytes", text.len());
        assert!(text.contains("locks: hub"));
        assert!(text.contains("-> stele_find_references"));
        assert!(text.contains("compaction"));
        assert!(
            !text.contains("(more routes"),
            "built-in routes were cut:\n{text}"
        );
    }

    #[test]
    fn long_instructions_are_cut_at_a_line() {
        let mut cfg = Config::builtin();
        let r = cfg.routes[0].clone();
        for _ in 0..100 {
            cfg.routes.push(r.clone());
        }
        let text = instructions(&cfg);
        assert!(text.len() <= INSTRUCTIONS_MAX);
        assert!(text.contains("(more routes are in harness.json)"));
        assert!(text.ends_with("never rely on remembered ones."));
    }
}
