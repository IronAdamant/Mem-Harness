//! A fake stdio MCP server for mh-backend's tests (std, mh-json and mh-sys
//! only). Not part of the harness. `mh-fake-mcp script <step>...` is a
//! second mode: see [`script`].
//!
//! Environment:
//! - `FAKE_LOG=<file>`: append every line received.
//! - `FAKE_PAGES=<n>`: `tools/list` in n pages of 2 tools (default 1 page).
//! - `FAKE_STARTUP_EXIT=<msg>`: print `<msg>` to stderr and exit 2 at once.
//! - `FAKE_STARTUP_HANG=1`: never read stdin (never answers `initialize`).
//! - `FAKE_GARBAGE=1`: print a non-JSON line before anything else.
//! - `FAKE_IGNORE_EOF=1`: keep running after stdin closes.
//!
//! The log also gets `{"event":"started","pid":N}` at start and
//! `{"event":"stdin-eof"}` when stdin closes.
//!
//! Tools: `echo` (returns its arguments and `_meta`; `delay_ms` delays the
//! reply), `slow` (`ms`, stops on `notifications/cancelled`), `fail`
//! (JSON-RPC error -32000), `crash` (exit 3), `notify` (list_changed,
//! progress, message, then a reply), `ping` (sends the client a `ping` and
//! an unknown request, replies with both answers), `env` (`names`: a list
//! of variable names; returns `{"vars": {name: value or null}}`). Unlisted:
//! `deep` (a reply nested too deep for mh-json).

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

/// The first `"id":<digits>` in a line (what the `/bin/sh` test servers
/// extract with `sed`), or `0`.
fn line_id(line: &str) -> String {
    numeric_id(line).unwrap_or_else(|| "0".into())
}

fn numeric_id(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("\"id\":")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

fn read_line(stdin: &mut std::io::StdinLock<'_>) -> Option<String> {
    let mut l = String::new();
    match stdin.read_line(&mut l) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(l),
    }
}

fn write_raw(to: &mut dyn Write, bytes: &[u8]) {
    let _ = to.write_all(bytes);
    let _ = to.flush();
}

/// `n` copies of `unit`, written in chunks.
fn write_repeat(to: &mut dyn Write, unit: &[u8], n: usize) {
    let per = (1 << 16) / unit.len().max(1) + 1;
    let chunk = unit.repeat(per);
    let mut left = n;
    while left > 0 {
        let k = left.min(per);
        if to.write_all(&chunk[..k * unit.len()]).is_err() {
            return;
        }
        left -= k;
    }
    let _ = to.flush();
}

/// `mh-fake-mcp sleep <ms> <pidfile|->`: write our pid (and a newline) to
/// `pidfile`, then sleep. A stand-in for the `sleep` a test server leaves
/// running.
fn child_sleep(args: &[String]) -> ! {
    let ms: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(30_000);
    if let Some(file) = args.get(1).filter(|f| *f != "-") {
        let _ = std::fs::write(file, format!("{}\n", std::process::id()));
    }
    thread::sleep(Duration::from_millis(ms));
    std::process::exit(0)
}

/// `mh-fake-mcp tee <file>`: append everything read from stdin to `file`,
/// then `eof` once stdin closes.
fn child_tee(args: &[String]) -> ! {
    use std::io::Read;
    let Some(file) = args.first() else {
        std::process::exit(2)
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    let mut buf = [0u8; 8192];
    let mut stdin = std::io::stdin().lock();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let _ = f.write_all(&buf[..n]);
                let _ = f.flush();
            }
        }
    }
    let _ = writeln!(f, "eof");
    std::process::exit(0)
}

/// `mh-fake-mcp script <step>...`: a server that does exactly the steps
/// given, in order, for tests of pipe and process edge cases on any OS (on
/// Unix those tests use tiny `/bin/sh` servers instead). `$ID` in a text is
/// the id of the last line read. Steps:
///
/// - `hs`: read `initialize`, answer it, read `initialized`.
/// - `read`: read one line.
/// - `print:<text>` / `eprint:<text>`: write a line to stdout / stderr.
/// - `sleep:<ms>`, `exit:<code>`, `touch:<path>`.
/// - `exit-if:<code>:<msg>:<path>`: when `path` exists, print `msg` to
///   stderr and exit with `code`.
/// - `spawn:<ms>:<io>:<wait|bg>:<pidfile|->`: start `sleep <ms>` (see
///   above) with stdin closed, keeping our stdout when `io` has `o` and our
///   stderr when it has `e`; `wait` waits for it, `bg` goes on.
/// - `tee:<file>`: start `tee <file>` reading our stdin, and go on.
/// - `bigline:<n>`: a reply with id 2 whose string result is `n` bytes.
/// - `bigerr:<n>`: a stderr line of `n` bytes.
/// - `bigreply:<n>`: a reply to `$ID` whose `x` is an array of `n + 1` ones.
/// - `pings:<n>`: `n` ping requests with ids `p0`, `p1`, ...
/// - `serial`: from here on read one line at a time and answer each line
///   with an id with `{}`, after 2 s when it calls the tool `a`.
// `spawn:..:bg` and `tee` leave their child running on purpose: what
// happens to it is what the tests check.
#[allow(clippy::zombie_processes)]
fn script(steps: &[String]) -> ! {
    // Windows: a child is handed every inheritable handle, so without this
    // a `spawn` with stdout closed would still hold our stdout pipe, unlike
    // `sleep >/dev/null &` in the `/bin/sh` servers these steps stand in for.
    // The handles a `spawn` keeps are passed explicitly, so they still work.
    mh_sys::stop_std_inheritance();
    let me = std::env::current_exe().unwrap();
    let mut stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let mut id = String::from("0");
    for step in steps {
        let (name, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        match name {
            "hs" => {
                let Some(l) = read_line(&mut stdin) else {
                    break;
                };
                id = line_id(&l);
                write_raw(
                    &mut out,
                    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}\n").as_bytes(),
                );
                if read_line(&mut stdin).is_none() {
                    break;
                }
            }
            "read" => {
                let Some(l) = read_line(&mut stdin) else {
                    break;
                };
                id = line_id(&l);
            }
            "print" => write_raw(
                &mut out,
                format!("{}\n", arg.replace("$ID", &id)).as_bytes(),
            ),
            "eprint" => write_raw(
                &mut err,
                format!("{}\n", arg.replace("$ID", &id)).as_bytes(),
            ),
            "sleep" => thread::sleep(Duration::from_millis(arg.parse().unwrap())),
            "exit" => std::process::exit(arg.parse().unwrap()),
            "touch" => {
                let _ = std::fs::write(arg, "");
            }
            "exit-if" => {
                // <code>:<msg>:<path>; the path is last as it may hold a drive colon.
                let mut it = arg.splitn(3, ':');
                let (code, msg, path) =
                    (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
                if std::path::Path::new(path).exists() {
                    write_raw(&mut err, format!("{msg}\n").as_bytes());
                    std::process::exit(code.parse().unwrap());
                }
            }
            "spawn" => {
                // <ms>:<io>:<wait|bg>:<pidfile|->; the pidfile is last (drive colon).
                let mut it = arg.splitn(4, ':');
                let (ms, io, mode, pidfile) = (
                    it.next().unwrap(),
                    it.next().unwrap(),
                    it.next().unwrap(),
                    it.next().unwrap(),
                );
                let mut c = std::process::Command::new(&me);
                c.args(["sleep", ms, pidfile])
                    .stdin(std::process::Stdio::null())
                    .stdout(if io.contains('o') {
                        std::process::Stdio::inherit()
                    } else {
                        std::process::Stdio::null()
                    })
                    .stderr(if io.contains('e') {
                        std::process::Stdio::inherit()
                    } else {
                        std::process::Stdio::null()
                    });
                let mut child = c.spawn().unwrap();
                if mode == "wait" {
                    let _ = child.wait();
                }
            }
            "tee" => {
                std::process::Command::new(&me)
                    .args(["tee", arg])
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
            }
            "bigline" => {
                write_raw(
                    &mut out,
                    b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"x\":\"",
                );
                write_repeat(&mut out, b"a", arg.parse().unwrap());
                write_raw(&mut out, b"\"}}\n");
            }
            "bigerr" => {
                write_repeat(&mut err, b"e", arg.parse().unwrap());
                write_raw(&mut err, b"\n");
            }
            "bigreply" => {
                write_raw(
                    &mut out,
                    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"x\":[").as_bytes(),
                );
                write_repeat(&mut out, b"1,", arg.parse().unwrap());
                write_raw(&mut out, b"1]}}\n");
            }
            "pings" => {
                let n: usize = arg.parse().unwrap();
                for i in 0..n {
                    let _ = writeln!(
                        out,
                        "{{\"jsonrpc\":\"2.0\",\"id\":\"p{i}\",\"method\":\"ping\"}}"
                    );
                }
                let _ = out.flush();
            }
            "serial" => {
                while let Some(l) = read_line(&mut stdin) {
                    // Like the Perl server it stands in for: a line without
                    // a numeric id (a notification) gets no answer.
                    let Some(lid) = numeric_id(&l) else { continue };
                    if l.contains("\"name\":\"a\"") {
                        thread::sleep(Duration::from_secs(2));
                    }
                    write_raw(
                        &mut out,
                        format!("{{\"jsonrpc\":\"2.0\",\"id\":{lid},\"result\":{{}}}}\n")
                            .as_bytes(),
                    );
                }
            }
            other => {
                write_raw(
                    &mut err,
                    format!("fake script: unknown step {other}\n").as_bytes(),
                );
                std::process::exit(99);
            }
        }
    }
    std::process::exit(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("script") => script(&args[1..]),
        Some("sleep") => child_sleep(&args[1..]),
        Some("tee") => child_tee(&args[1..]),
        _ => {}
    }
    if let Ok(msg) = std::env::var("FAKE_STARTUP_EXIT") {
        eprintln!("{msg}");
        std::process::exit(2);
    }
    if std::env::var("FAKE_STARTUP_HANG").is_ok() {
        // Logged so a test can check the hung process is stopped.
        log(&format!(
            "{{\"event\":\"started\",\"pid\":{}}}",
            std::process::id()
        ));
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    if std::env::var("FAKE_GARBAGE").is_ok() {
        let mut o = out.lock().unwrap();
        let _ = o.write_all(b"this is not json\n");
        let _ = o.flush();
    }
    eprintln!("fake: started");
    log(&format!(
        "{{\"event\":\"started\",\"pid\":{}}}",
        std::process::id()
    ));
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
                    // Not listed: a reply nested too deep to parse.
                    "deep" => {
                        let line = format!(
                            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}{}}}\n",
                            id.to_json(),
                            "[".repeat(300),
                            "]".repeat(300)
                        );
                        let mut o = out.lock().unwrap();
                        let _ = o.write_all(line.as_bytes());
                        let _ = o.flush();
                    }
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
                    "env" => {
                        let vars = args
                            .get("names")
                            .and_then(Value::as_array)
                            .map(|names| {
                                names
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(|n| {
                                        let v =
                                            std::env::var(n).map(Value::str).unwrap_or(Value::Null);
                                        (n.to_string(), v)
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let mut obj = Value::obj(vec![]);
                        for (k, v) in vars {
                            obj.set(&k, v);
                        }
                        reply(&out, &id, Value::obj(vec![("vars", obj)]));
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
    log("{\"event\":\"stdin-eof\"}");
    if std::env::var("FAKE_IGNORE_EOF").is_ok() {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
