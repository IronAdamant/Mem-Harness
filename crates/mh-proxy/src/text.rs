//! Text the proxy writes for the model: the `initialize` instructions, the
//! `harness_status` tool definition, and the messages for failed calls.

use mh_core::Config;
use mh_json::Value;

/// The most `instructions` may hold; Claude Code shows it in every session.
pub const INSTRUCTIONS_MAX: usize = 2000;

/// The `instructions` of the `initialize` reply: who owns each feature,
/// which tool answers which question, and the rule to re-read claims and
/// rules after compaction. Never longer than [`INSTRUCTIONS_MAX`] bytes:
/// route lines that do not fit are dropped and the last line says so.
pub fn instructions(cfg: &Config) -> String {
    let head = format!(
        "Mem-Harness fronts {} tool servers as one. Tool names are <backend>_<tool>; \
         harness_status shows which backends are up and why one is not.",
        cfg.backends.len()
    );
    let tail = "After compaction, re-read claims and rules with their tools; never rely on remembered ones.";
    let mut lines = vec![head];
    if !cfg.ownership.is_empty() {
        lines.push("Owner of each feature (use only the owner's tools for it):".to_string());
        for o in &cfg.ownership {
            lines.push(format!("{}: {}", o.feature, o.owner));
        }
    }
    let mut routes: Vec<String> = cfg
        .routes
        .iter()
        .map(|r| format!("{} -> {}", r.question, r.tool))
        .collect();
    if !routes.is_empty() {
        routes.insert(0, "Which tool answers which question:".to_string());
    }
    let join = |lines: &[String], routes: &[String], more: bool| {
        let mut all: Vec<&str> = lines.iter().map(String::as_str).collect();
        all.extend(routes.iter().map(String::as_str));
        if more {
            all.push("(more routes are in harness.json)");
        }
        all.push(tail);
        all.join("\n")
    };
    let mut text = join(&lines, &routes, false);
    while text.len() > INSTRUCTIONS_MAX && routes.len() > 1 {
        routes.pop();
        text = join(&lines, &routes, true);
    }
    while text.len() > INSTRUCTIONS_MAX && lines.len() > 1 {
        lines.pop();
        text = join(&lines, &[], true);
    }
    if text.len() > INSTRUCTIONS_MAX {
        let mut cut = INSTRUCTIONS_MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

pub const STATUS_TOOL: &str = "harness_status";

pub fn status_def() -> Value {
    Value::obj(vec![
        ("name", Value::str(STATUS_TOOL)),
        (
            "description",
            Value::str(
                "Mem-Harness state as JSON: per backend whether it is up, how often it started, its last error, \
                 server name and version, how many tools it exposes, what the ownership table hides there, \
                 how many tools the harness hid (hide_tools/only_tools) and which disabled tools it still lists (ignored_disable); \
                 plus the ownership table and where the config came from. Call it when a tool is missing or fails.",
            ),
        ),
        (
            "inputSchema",
            Value::obj(vec![("type", Value::str("object")), ("properties", Value::obj(vec![]))]),
        ),
        ("annotations", Value::obj(vec![("readOnlyHint", Value::Bool(true))])),
    ])
}

/// A tool result holding one text item.
pub fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    let item = Value::obj(vec![
        ("type", Value::str("text")),
        ("text", Value::str(text)),
    ]);
    let mut v = Value::obj(vec![("content", Value::Array(vec![item]))]);
    if is_error {
        v.set("isError", Value::Bool(true));
    }
    v
}

pub fn unavailable(tool: &str, backend: &str, why: &str) -> String {
    format!(
        "{tool} failed: backend \"{backend}\" is unavailable: {why}. \
         Fix: make sure its server is installed and on PATH (run `mem-harness doctor`); \
         harness_status shows its state. The next call starts it again."
    )
}

/// A call cut short because the harness itself is stopping (its client
/// closed stdin): nothing is wrong with the backend.
pub fn shutting_down(tool: &str, backend: &str, why: &str) -> String {
    format!(
        "{tool} did not finish: the harness is shutting down, so backend \"{backend}\" \
         was stopped ({why})."
    )
}

pub fn panicked(tool: &str, backend: &str, why: &str) -> String {
    format!(
        "{tool} failed: the harness's client for backend \"{backend}\" panicked: {why}. \
         This is a bug; retry once, and check harness_status."
    )
}

pub fn timeout(tool: &str, backend: &str, why: &str) -> String {
    format!(
        "{tool} failed: backend \"{backend}\" did not answer in time: {why}. \
         The work may still be running there; retry with a smaller request, or check harness_status."
    )
}
