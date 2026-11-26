//! A fake stdio MCP server for mh-backend's tests (std and mh-json only).
//! Not part of the harness.
//!
//! Environment:
//! - `FAKE_LOG=<file>`: append every line received.
//! - `FAKE_PAGES=<n>`: `tools/list` in n pages of 2 tools (default 1 page).
//! - `FAKE_STARTUP_EXIT=<msg>`: print `<msg>` to stderr and exit 2 at once.
//! - `FAKE_GARBAGE=1`: print a non-JSON line before anything else.
//! - `FAKE_IGNORE_EOF=1`: keep running after stdin closes.
//!
//! Tools: `echo` (returns its arguments and `_meta`; `delay_ms` delays the
//! reply), `slow` (`ms`, stops on `notifications/cancelled`), `fail`
//! (JSON-RPC error -32000), `crash` (exit 3), `notify` (list_changed,
//! progress, message, then a reply), `ping` (sends the client a `ping` and
//! an unknown request, replies with both answers).

use mh_json::Value;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

type Out = Arc<Mutex<std::io::Stdout>>;

fn send(out: &Out, v: &Value) {
    let mut line = v.to_json();
    line.push('\n');
    let mut o = out.lock().unwrap();
    let _ = o.write_all(line.as_bytes());
    let _ = o.flush();
}

fn reply(out: &Out, id: &Value, result: Value) {
    send(
        out,
        &Value::obj(vec![
            ("jsonrpc", Value::str("2.0")),
            ("id", id.clone()),
            ("result", result),
        ]),
    );
}

fn error(out: &Out, id: &Value, code: i64, message: &str, data: Option<Value>) {
    let mut err = Value::obj(vec![
        ("code", Value::int(code)),
        ("message", Value::str(message)),
    ]);
    if let Some(d) = data {
        err.set("data", d);
    }
    send(
        out,
        &Value::obj(vec![
            ("jsonrpc", Value::str("2.0")),
            ("id", id.clone()),
            ("error", err),
        ]),
    );
}

fn text(s: &str) -> Value {
    Value::obj(vec![(
        "content",
        Value::Array(vec![Value::obj(vec![
            ("type", Value::str("text")),
            ("text", Value::str(s)),
        ])]),
    )])
}

fn tool(name: &str) -> Value {
    Value::obj(vec![
        ("name", Value::str(name)),
        ("description", Value::str(format!("fake {name}"))),
        (
            "inputSchema",
            Value::obj(vec![("type", Value::str("object"))]),
        ),
    ])
}

fn log(line: &str) {
    if let Ok(path) = std::env::var("FAKE_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

struct PingWait {
    call_id: Value,
    ping: Option<Value>,
    other: Option<Value>,
}

fn main() {
    if let Ok(msg) = std::env::var("FAKE_STARTUP_EXIT") {
        eprintln!("{msg}");
        std::process::exit(2);
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    if std::env::var("FAKE_GARBAGE").is_ok() {
        let mut o = out.lock().unwrap();
        let _ = o.write_all(b"this is not json\n");
        let _ = o.flush();
    }
    eprintln!("fake: started");
    let pages: usize = std::env::var("FAKE_PAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let cancelled: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut ping_wait: Option<PingWait> = None;

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        log(&line);
        let Ok(msg) = mh_json::parse(&line) else {
            eprintln!("fake: bad line");
            continue;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // A reply to one of our requests.
        if method.is_empty() {
            if let (Some(w), Some(id)) = (ping_wait.as_mut(), id.as_ref()) {
                match id.as_str() {
                    Some("srv-ping") => w.ping = Some(msg.clone()),
                    Some("srv-other") => w.other = Some(msg.clone()),
                    _ => {}
                }
                if w.ping.is_some() && w.other.is_some() {
                    let w = ping_wait.take().unwrap();
                    let mut r = text("pinged");
                    r.set("ping", w.ping.unwrap());
                    r.set("other", w.other.unwrap());
                    reply(&out, &w.call_id, r);
                }
            }
            continue;
        }

        let Some(id) = id else {
            if method == "notifications/cancelled" {
                if let Some(rid) = params.get("requestId") {
                    cancelled.lock().unwrap().push(rid.to_json());
                }
            }
            continue;
        };

        match method.as_str() {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .cloned()
                    .unwrap_or(Value::str("2025-06-18"));
                reply(
                    &out,
                    &id,
                    Value::obj(vec![
                        ("protocolVersion", version),
                        (
                            "capabilities",
                            Value::obj(vec![(
                                "tools",
                                Value::obj(vec![("listChanged", Value::Bool(true))]),
                            )]),
                        ),
                        (
                            "serverInfo",
                            Value::obj(vec![
                                ("name", Value::str("fake-mcp")),
                                ("version", Value::str("9.9.9")),
                            ]),
                        ),
                        ("instructions", Value::str("fake instructions")),
                    ]),
                );
            }
            "tools/list" => {
                let page: usize = params
                    .get("cursor")
                    .and_then(Value::as_str)
                    .and_then(|c| c.strip_prefix('p'))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let names = if pages == 1 {
                    ["echo", "slow", "fail", "crash", "notify", "ping"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                } else {
                    vec![format!("t{page}a"), format!("t{page}b")]
                };
                let mut r = Value::obj(vec![(
                    "tools",
                    Value::Array(names.iter().map(|n| tool(n)).collect()),
                )]);
                if page + 1 < pages {
                    r.set("nextCursor", Value::str(format!("p{}", page + 1)));
                }
                reply(&out, &id, r);
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                let meta = params.get("_meta").cloned().unwrap_or(Value::Null);
                match name.as_str() {
                    "echo" => {
                        let delay =
                            args.get("delay_ms").and_then(Value::as_i64).unwrap_or(0) as u64;
                        let out = out.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(delay));
                            let mut r = text(&args.to_json());
                            r.set("arguments", args);
                            r.set("meta", meta);
                            reply(&out, &id, r);
                        });
                    }
                    "slow" => {
                        let ms = args.get("ms").and_then(Value::as_i64).unwrap_or(10_000) as u64;
                        let (out, cancelled) = (out.clone(), cancelled.clone());
                        thread::spawn(move || {
                            let key = id.to_json();
                            for _ in 0..ms / 10 {
                                if cancelled.lock().unwrap().contains(&key) {
                                    eprintln!("fake: call {key} cancelled");
                                    return;
                                }
                                thread::sleep(Duration::from_millis(10));
                            }
                            reply(&out, &id, text("slow done"));
                        });
                    }
                    "fail" => error(
                        &out,
                        &id,
                        -32000,
                        "boom",
                        Some(Value::obj(vec![("why", Value::str("asked to fail"))])),
                    ),
                    "crash" => {
                        eprintln!("fake: crashing now");
                        std::process::exit(3);
                    }
                    "notify" => {
                        let note = |method: &str, params: Value| {
                            Value::obj(vec![
                                ("jsonrpc", Value::str("2.0")),
                                ("method", Value::str(method)),
                                ("params", params),
                            ])
                        };
                        send(
                            &out,
                            &note("notifications/tools/list_changed", Value::obj(vec![])),
                        );
                        let token = meta.get("progressToken").cloned().unwrap_or(Value::Null);
                        send(
                            &out,
                            &note(
                                "notifications/progress",
                                Value::obj(vec![
                                    ("progressToken", token),
                                    ("progress", Value::int(1)),
                                    ("total", Value::int(2)),
                                ]),
                            ),
                        );
                        send(
                            &out,
                            &note(
                                "notifications/message",
                                Value::obj(vec![
                                    ("level", Value::str("info")),
                                    ("data", Value::str("hello")),
                                ]),
                            ),
                        );
                        send(
                            &out,
                            &note("notifications/unknown_thing", Value::obj(vec![])),
                        );
                        reply(&out, &id, text("notified"));
                    }
                    "ping" => {
                        ping_wait = Some(PingWait {
                            call_id: id,
                            ping: None,
                            other: None,
                        });
                        let req = |id: &str, method: &str| {
                            Value::obj(vec![
                                ("jsonrpc", Value::str("2.0")),
                                ("id", Value::str(id)),
                                ("method", Value::str(method)),
                            ])
                        };
                        send(&out, &req("srv-ping", "ping"));
                        send(&out, &req("srv-other", "sampling/createMessage"));
                    }
                    other => error(&out, &id, -32602, &format!("unknown tool {other}"), None),
                }
            }
            "ping" => reply(&out, &id, Value::obj(vec![])),
            other => error(
                &out,
                &id,
                -32601,
                &format!("method not found: {other}"),
                None,
            ),
        }
    }
    if std::env::var("FAKE_IGNORE_EOF").is_ok() {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
