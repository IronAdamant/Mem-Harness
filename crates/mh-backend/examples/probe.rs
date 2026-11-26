//! Live probe: start every built-in backend in a project directory and list
//! its tools.
//!
//! `cargo run -p mh-backend --example probe -- <project-dir>`
//!
//! Prints one line per backend (in config order): name, ok/ERROR, server
//! version, tool count, then the tool names. Server stderr goes to stderr.

use mh_backend::{build, EventSink};
use mh_core::Config;
use mh_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn main() {
    let Some(project) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: probe <project-dir>");
        std::process::exit(2);
    };
    let cfg = Config::builtin();
    let sink: EventSink = Arc::new(|name, ev| eprintln!("[{name}] event: {ev:?}"));
    let handles: Vec<_> = cfg
        .backends
        .iter()
        .map(|spec| {
            let (spec, env, project, sink) = (
                spec.clone(),
                cfg.backend_env(&spec.name),
                project.clone(),
                sink.clone(),
            );
            thread::spawn(move || {
                let t0 = Instant::now();
                let env_text: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                let backend = match build(&spec, env, &project, sink) {
                    Ok(b) => b,
                    Err(e) => return format!("{:<9} ERROR {e}", spec.name),
                };
                let line = match backend.start().and_then(|_| backend.list_tools()) {
                    Ok(tools) => {
                        let st = backend.status();
                        let version = st
                            .server_info
                            .as_ref()
                            .and_then(|i| i.get("version"))
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        let names: Vec<&str> = tools
                            .iter()
                            .filter_map(|t| t.get("name").and_then(Value::as_str))
                            .collect();
                        format!(
                            "{:<9} ok    v{version} {} tools ({} ms) [{}]: {}",
                            spec.name,
                            tools.len(),
                            t0.elapsed().as_millis(),
                            env_text.join(" "),
                            names.join(", ")
                        )
                    }
                    Err(e) => format!("{:<9} ERROR [{}] {e}", spec.name, env_text.join(" ")),
                };
                backend.shutdown();
                line
            })
        })
        .collect();
    for h in handles {
        println!(
            "{}",
            h.join().unwrap_or_else(|_| "probe thread panicked".into())
        );
    }
}
