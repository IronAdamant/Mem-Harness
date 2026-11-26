//! Drives `serve_with` through an in-memory pipe with fake backends.

use mh_backend::{Backend, BackendEvent, BackendStatus, CallError, CancelToken, EventSink};
use mh_core::Config;
use mh_json::{parse, Value};
use mh_proxy::{serve_with, NativeTool};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(10);

// ---------- fake backend ----------

#[derive(Default)]
struct Fake {
    name: String,
    tools: Mutex<Vec<Value>>,
    fail_start: Option<String>,
    /// `start` fails this many more times, then succeeds.
    flaky_starts: AtomicU32,
    /// When set, `start` blocks until it is opened.
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    calls: Mutex<Vec<(String, Value, Option<Value>)>>,
    lists: AtomicU32,
    release: AtomicBool,
    saw_cancel: AtomicBool,
    shut: AtomicBool,
    /// `start` was entered (before the gate).
    entered: AtomicBool,
    /// `start` finished (past the gate).
    started: AtomicBool,
    /// `shutdown` ran after `start` finished.
    shut_after_start: AtomicBool,
}

fn tool(name: &str) -> Value {
    parse(&format!(
        r#"{{"name":"{name}","description":"does {name}","inputSchema":{{"type":"object"}},"annotations":{{"x":1.50}}}}"#
    ))
    .unwrap()
}

impl Fake {
    fn new(name: &str, tools: &[&str]) -> Fake {
        Fake {
            name: name.into(),
            tools: Mutex::new(tools.iter().map(|t| tool(t)).collect()),
            ..Fake::default()
        }
    }
    fn wait_until(&self, what: &str, f: impl Fn() -> bool) {
        let end = Instant::now() + DEADLINE;
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Backend for Fake {
    fn name(&self) -> &str {
        &self.name
    }
    fn start(&self) -> Result<(), CallError> {
        self.entered.store(true, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            let mut open = g.0.lock().unwrap();
            while !*open {
                open = g.1.wait(open).unwrap();
            }
        }
        if self
            .flaky_starts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(CallError::Timeout("no initialize reply within 30 s".into()));
        }
        self.started.store(true, Ordering::SeqCst);
        match &self.fail_start {
            Some(e) => Err(CallError::Unavailable(e.clone())),
            None => Ok(()),
        }
    }
    fn status(&self) -> BackendStatus {
        BackendStatus {
            up: self.fail_start.is_none(),
            error: self.fail_start.clone(),
            server_info: Some(
                parse(&format!(
                    r#"{{"name":"{}-server","version":"9.9"}}"#,
                    self.name
                ))
                .unwrap(),
            ),
            instructions: None,
            starts: 1,
        }
    }
    fn list_tools(&self) -> Result<Vec<Value>, CallError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(self.tools.lock().unwrap().clone())
    }
    fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        meta: Option<Value>,
        cancel: &CancelToken,
    ) -> Result<Value, CallError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), arguments.clone(), meta.clone()));
        let text = |t: String| {
            Ok(Value::obj(vec![(
                "content",
                Value::Array(vec![Value::obj(vec![
                    ("type", Value::str("text")),
                    ("text", Value::str(t)),
                ])]),
            )]))
        };
        match tool {
            "block" => loop {
                if cancel.is_cancelled() {
                    self.saw_cancel.store(true, Ordering::SeqCst);
                    return Err(CallError::Cancelled);
                }
                if self.shut.load(Ordering::SeqCst) {
                    return Err(CallError::Unavailable("shut down".into()));
                }
                thread::sleep(Duration::from_millis(1));
            },
            "slow" => loop {
                if self.release.load(Ordering::SeqCst) {
                    return text("slow done".into());
                }
                if self.shut.load(Ordering::SeqCst) {
                    return Err(CallError::Unavailable("shut down".into()));
                }
                thread::sleep(Duration::from_millis(1));
            },
            "rpc_fail" => Err(CallError::Rpc {
                code: -32001,
                message: "bad input".into(),
                data: Some(parse(r#"{"field":"n"}"#).unwrap()),
            }),
            "down" => Err(CallError::Unavailable(
                "its process crashed; reinstall it".into(),
            )),
            "late" => Err(CallError::Timeout("no reply in 600 s".into())),
            "panic" => panic!("bug in backend"),
            _ => {
                let meta = meta.unwrap_or(Value::Null);
                text(format!(
                    r#"{{"tool":"{tool}","args":{arguments},"meta":{meta}}}"#
                ))
            }
        }
    }
    fn shutdown(&self) {
        self.shut.store(true, Ordering::SeqCst);
        if self.started.load(Ordering::SeqCst) {
            self.shut_after_start.store(true, Ordering::SeqCst);
        }
    }
}

// ---------- in-memory pipe ----------

struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: VecDeque<u8>,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(bytes) => self.buf.extend(bytes),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len());
        for (slot, b) in out.iter_mut().zip(self.buf.drain(..n)) {
            *slot = b;
        }
        Ok(n)
    }
}

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Harness {
    tx: Option<Sender<Vec<u8>>>,
    out: SharedBuf,
    sink: Arc<Mutex<Option<EventSink>>>,
    join: Option<JoinHandle<Result<(), String>>>,
}

const CONFIG: &str = r#"{
  "version": 1,
  "backends": [
    { "name": "alpha", "command": "alpha-mcp", "disable_env": "ALPHA_DISABLE", "disable": [{"what": "extras", "why": "noise"}] },
    { "name": "relatum", "command": "relatum-mcp", "strip_prefix": "relatum_" },
    { "name": "beta", "command": "beta-mcp", "disable_env": "BETA_DISABLE" }
  ],
  "ownership": [
    { "feature": "locks", "owner": "alpha", "why": "one lock table", "disable": { "beta": ["take_lock"] } },
    { "feature": "rules", "owner": "relatum", "why": "notes only" }
  ],
  "routes": [ { "question": "who holds a file", "tool": "alpha_take_lock" } ]
}"#;

fn cfg() -> Config {
    Config::from_json(CONFIG, "test.json").unwrap()
}

fn start(natives: Vec<NativeTool>, backends: Vec<Arc<Fake>>) -> Harness {
    start_cfg(cfg(), natives, backends)
}

fn start_cfg(cfg: Config, natives: Vec<NativeTool>, backends: Vec<Arc<Fake>>) -> Harness {
    let (tx, rx) = channel();
    let out = SharedBuf::default();
    let sink = Arc::new(Mutex::new(None));
    let (o, s) = (out.clone(), sink.clone());
    let join = thread::spawn(move || {
        let input = BufReader::new(ChanReader {
            rx,
            buf: VecDeque::new(),
        });
        serve_with(
            cfg,
            natives,
            move |es: EventSink| {
                *s.lock().unwrap() = Some(es);
                backends
                    .into_iter()
                    .map(|b| b as Arc<dyn Backend>)
                    .collect()
            },
            input,
            o,
        )
    });
    Harness {
        tx: Some(tx),
        out,
        sink,
        join: Some(join),
    }
}

impl Harness {
    fn send(&self, line: &str) {
        self.tx
            .as_ref()
            .unwrap()
            .send(format!("{line}\n").into_bytes())
            .unwrap();
    }
    fn raw_lines(&self) -> Vec<String> {
        let bytes = self.out.0.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.is_empty() || text.ends_with('\n'),
            "partial line in output"
        );
        text.lines().map(str::to_string).collect()
    }
    fn lines(&self) -> Vec<Value> {
        self.raw_lines()
            .iter()
            .map(|l| parse(l).unwrap_or_else(|e| panic!("bad output line {l}: {e}")))
            .collect()
    }
    fn wait(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let end = Instant::now() + DEADLINE;
        loop {
            if let Some(v) = self.lines().into_iter().find(&f) {
                return v;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}; output: {:?}",
                self.raw_lines()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn reply(&self, id: &str) -> Value {
        self.wait(&format!("reply {id}"), |v| {
            v.get("id").map(Value::to_json).as_deref() == Some(id)
        })
    }
    fn request(&self, id: i64, method: &str, params: &str) -> Value {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        self.reply(&id.to_string())
    }
    fn init(&self) {
        self.request(0, "initialize", r#"{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}"#);
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }
    fn tool_names(&self, id: i64) -> Vec<String> {
        let r = self.request(id, "tools/list", "{}");
        r.get("result")
            .unwrap()
            .get("tools")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.get("name").unwrap().as_str().unwrap().to_string())
            .collect()
    }
    fn emit(&self, backend: &str, ev: BackendEvent) {
        let sink = self.sink.lock().unwrap().clone().unwrap();
        sink(backend, ev);
    }
    /// Close the input, wait for `serve_with` to return, give back the output.
    fn finish(mut self) -> Vec<Value> {
        self.tx = None;
        let r = self.join.take().unwrap().join().unwrap();
        assert_eq!(r, Ok(()));
        self.lines()
    }
}

fn text_of(reply: &Value) -> String {
    reply
        .get("result")
        .unwrap()
        .get("content")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("text")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

fn is_error(reply: &Value) -> bool {
    reply
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn three() -> (Arc<Fake>, Arc<Fake>, Arc<Fake>) {
    (
        Arc::new(Fake::new(
            "alpha",
            &[
                "echo",
                "block",
                "slow",
                "rpc_fail",
                "down",
                "late",
                "take_lock",
            ],
        )),
        Arc::new(Fake::new("relatum", &["relatum_get", "get", "relatum_new"])),
        Arc::new(Fake::new("beta", &["echo"])),
    )
}

fn native(
    name: &str,
    f: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
) -> NativeTool {
    NativeTool {
        def: parse(&format!(
            r#"{{"name":"{name}","description":"native","inputSchema":{{"type":"object"}}}}"#
        ))
        .unwrap(),
        run: Arc::new(f),
    }
}

// ---------- tests ----------

#[test]
fn initialize_negotiates_the_version() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a, r, b]);
    let v = h.request(1, "initialize", r#"{"protocolVersion":"2024-11-05"}"#);
    let res = v.get("result").unwrap();
    assert_eq!(
        res.get("protocolVersion").unwrap().as_str(),
        Some("2024-11-05")
    );
    assert_eq!(
        res.get("serverInfo").unwrap().get("name").unwrap().as_str(),
        Some("mem-harness")
    );
    assert_eq!(
        res.get("capabilities").unwrap().to_json(),
        r#"{"tools":{"listChanged":true}}"#
    );
    let ins = res.get("instructions").unwrap().as_str().unwrap();
    assert!(
        ins.contains("locks: alpha") && ins.contains("who holds a file -> alpha_take_lock"),
        "{ins}"
    );
    assert!(ins.contains("compaction"));

    // Unknown, and 2025-03-26 (it needs batches, which are not taken):
    // the newest supported.
    for (id, asked) in [(2, "1999-01-01"), (5, "2025-03-26")] {
        let v = h.request(
            id,
            "initialize",
            &format!(r#"{{"protocolVersion":"{asked}"}}"#),
        );
        assert_eq!(
            v.get("result")
                .unwrap()
                .get("protocolVersion")
                .unwrap()
                .as_str(),
            Some(mh_proxy::PROTOCOL_VERSIONS[0]),
            "{asked}"
        );
    }
    assert_eq!(mh_proxy::PROTOCOL_VERSIONS[0], "2025-11-25");
    assert!(!mh_proxy::PROTOCOL_VERSIONS.contains(&"2025-03-26"));
    let v = h.request(3, "initialize", r#"{"protocolVersion":"2025-11-25"}"#);
    assert_eq!(
        v.get("result")
            .unwrap()
            .get("protocolVersion")
            .unwrap()
            .as_str(),
        Some("2025-11-25")
    );
    assert_eq!(
        h.request(4, "ping", "{}").get("result").unwrap().to_json(),
        "{}"
    );
    // The initialized notification gets no reply.
    h.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let out = h.finish();
    assert_eq!(out.len(), 5, "{out:?}");
}

#[test]
fn builtin_config_instructions_fit() {
    let h = start_cfg(Config::builtin(), vec![], vec![]);
    let v = h.request(1, "initialize", "{}");
    let ins = v
        .get("result")
        .unwrap()
        .get("instructions")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert!(ins.len() < 2000, "{} bytes", ins.len());
    assert!(ins.contains("task-board: libellus"));
    assert!(ins.contains("which tests to run after an edit -> chisel_diff_impact"));
    h.finish();
}

#[test]
fn tools_list_merges_and_prefixes() {
    let (a, r, b) = three();
    let natives = vec![
        native("harness_hello", |_| Ok(Value::obj(vec![]))),
        native("alpha_echo", |_| Ok(Value::obj(vec![]))),
    ];
    let h = start(natives, vec![a, r, b]);
    h.init();
    let v = h.request(1, "tools/list", r#"{"cursor":null}"#);
    let tools = v
        .get("result")
        .unwrap()
        .get("tools")
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t.get("name").unwrap().as_str().unwrap())
        .collect();
    assert_eq!(
        &names[..3],
        &["harness_status", "harness_hello", "alpha_echo"]
    );
    // The native alpha_echo won; alpha's echo is not listed twice.
    assert_eq!(names.iter().filter(|n| **n == "alpha_echo").count(), 1);
    assert_eq!(
        tools[2].get("description").unwrap().as_str(),
        Some("native")
    );
    assert!(names.contains(&"alpha_take_lock"));
    assert!(names.contains(&"beta_echo"));
    // strip_prefix: relatum_get stays relatum_get; bare `get` collides with it and loses.
    assert!(names.contains(&"relatum_get") && names.contains(&"relatum_new"));
    assert_eq!(names.iter().filter(|n| **n == "relatum_get").count(), 1);
    assert!(!names.iter().any(|n| n.contains("relatum_relatum")));
    // Everything but the name is kept, numbers byte for byte.
    let t = tools
        .iter()
        .find(|t| t.get("name").unwrap().as_str() == Some("beta_echo"))
        .unwrap();
    assert_eq!(
        t.to_json(),
        r#"{"name":"beta_echo","description":"does echo","inputSchema":{"type":"object"},"annotations":{"x":1.50}}"#
    );
    h.finish();
}

#[test]
fn calls_route_and_pass_arguments_and_meta_through() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a.clone(), r.clone(), b.clone()]);
    h.init();
    h.tool_names(1);
    h.send(r#"{"jsonrpc":"2.0","id":"s-1","method":"tools/call","params":{"name":"alpha_echo","arguments":{"n":1.50,"big":12345678901234567890},"_meta":{"progressToken":7}}}"#);
    let v = h.reply(r#""s-1""#);
    assert_eq!(
        text_of(&v),
        r#"{"tool":"echo","args":{"n":1.50,"big":12345678901234567890},"meta":{"progressToken":7}}"#
    );
    let v = h.request(2, "tools/call", r#"{"name":"relatum_get"}"#);
    assert_eq!(
        text_of(&v),
        r#"{"tool":"relatum_get","args":{},"meta":null}"#
    );
    let v = h.request(3, "tools/call", r#"{"name":"beta_echo","arguments":{}}"#);
    assert!(text_of(&v).contains(r#""tool":"echo""#));
    assert_eq!(a.calls.lock().unwrap().len(), 1);
    assert_eq!(r.calls.lock().unwrap()[0].0, "relatum_get");
    assert_eq!(b.calls.lock().unwrap().len(), 1);
    h.finish();
}

#[test]
fn backend_errors_map_to_rpc_errors_or_error_results() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a, r, b]);
    h.init();
    h.tool_names(1);
    let v = h.request(
        2,
        "tools/call",
        r#"{"name":"alpha_rpc_fail","arguments":{}}"#,
    );
    assert_eq!(
        v.get("error").unwrap().to_json(),
        r#"{"code":-32001,"message":"bad input","data":{"field":"n"}}"#
    );
    let v = h.request(3, "tools/call", r#"{"name":"alpha_down"}"#);
    assert!(is_error(&v));
    let t = text_of(&v);
    assert!(
        t.contains("\"alpha\"") && t.contains("crashed") && t.contains("Fix"),
        "{t}"
    );
    let v = h.request(4, "tools/call", r#"{"name":"alpha_late"}"#);
    assert!(is_error(&v));
    assert!(text_of(&v).contains("did not answer in time"));
    h.finish();
}

#[test]
fn native_tools_run_and_report_errors() {
    let natives = vec![
        native("harness_ok", |args| {
            Ok(Value::obj(vec![
                ("content", Value::Array(vec![])),
                ("got", args),
            ]))
        }),
        native("harness_bad", |_| Err("no store here; run init".into())),
    ];
    let h = start(natives, vec![]);
    h.init();
    let v = h.request(
        1,
        "tools/call",
        r#"{"name":"harness_ok","arguments":{"a":[1]}}"#,
    );
    assert_eq!(
        v.get("result").unwrap().to_json(),
        r#"{"content":[],"got":{"a":[1]}}"#
    );
    let v = h.request(2, "tools/call", r#"{"name":"harness_bad"}"#);
    assert!(is_error(&v));
    assert_eq!(text_of(&v), "no store here; run init");
    h.finish();
}

#[test]
fn cancelled_call_gets_no_reply() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a.clone(), r, b]);
    h.init();
    h.tool_names(1);
    h.send(r#"{"jsonrpc":"2.0","id":"c-7","method":"tools/call","params":{"name":"alpha_block"}}"#);
    a.wait_until("the call to arrive", || !a.calls.lock().unwrap().is_empty());
    h.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"c-7","reason":"user"}}"#);
    a.wait_until("the cancel", || a.saw_cancel.load(Ordering::SeqCst));
    h.request(2, "ping", "{}");
    let out = h.finish();
    assert!(
        !out.iter()
            .any(|v| v.get("id").map(Value::to_json).as_deref() == Some(r#""c-7""#)),
        "{out:?}"
    );
}

#[test]
fn slow_call_does_not_block_a_fast_one() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a.clone(), r, b]);
    h.init();
    h.tool_names(1);
    h.send(r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"alpha_slow"}}"#);
    a.wait_until("the slow call", || !a.calls.lock().unwrap().is_empty());
    let fast = h.request(11, "tools/call", r#"{"name":"alpha_echo"}"#);
    assert!(text_of(&fast).contains("echo"));
    assert!(!h
        .lines()
        .iter()
        .any(|v| v.get("id").map(Value::to_json).as_deref() == Some("10")));
    a.release.store(true, Ordering::SeqCst);
    let slow = h.reply("10");
    assert_eq!(text_of(&slow), "slow done");
    let out = h.finish();
    let pos = |id: &str| {
        out.iter()
            .position(|v| v.get("id").map(Value::to_json).as_deref() == Some(id))
            .unwrap()
    };
    assert!(pos("11") < pos("10"));
}

#[test]
fn tools_changed_sends_list_changed_once_per_real_change() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a, r, b.clone()]);
    h.init();
    assert!(!h.tool_names(1).contains(&"beta_fresh".to_string()));
    let lists = b.lists.load(Ordering::SeqCst);
    // Unchanged list: refetched, nothing sent.
    h.emit("beta", BackendEvent::ToolsChanged);
    b.wait_until("the refetch", || b.lists.load(Ordering::SeqCst) > lists);
    b.tools.lock().unwrap().push(tool("fresh"));
    h.emit("beta", BackendEvent::ToolsChanged);
    let is_changed = |v: &Value| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
    };
    h.wait("list_changed", is_changed);
    assert!(h.tool_names(2).contains(&"beta_fresh".to_string()));
    // An exit keeps the tools (the next call restarts it): no change.
    h.emit("beta", BackendEvent::Exited("killed by signal 9".into()));
    let st = h.request(3, "tools/call", r#"{"name":"harness_status"}"#);
    assert!(text_of(&st).contains("killed by signal 9"));
    let out = h.finish();
    assert_eq!(out.iter().filter(|v| is_changed(v)).count(), 1, "{out:?}");
}

#[test]
fn relay_is_passed_through_unchanged() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a, r, b]);
    h.init();
    let msg = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":7,"progress":0.50,"total":1}}"#;
    h.emit("alpha", BackendEvent::Relay(parse(msg).unwrap()));
    h.wait("relay", |v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/progress")
    });
    assert!(h.raw_lines().iter().any(|l| l == msg));
    h.finish();
}

#[test]
fn unknown_tools_and_methods_and_bad_input() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a, r, b]);
    h.init();
    h.tool_names(1);
    let v = h.request(2, "tools/call", r#"{"name":"gamma_take_lock"}"#);
    let e = v.get("error").unwrap();
    assert_eq!(e.get("code").unwrap().as_i64(), Some(-32602));
    let m = e.get("message").unwrap().as_str().unwrap();
    assert!(
        m.contains("gamma_take_lock") && m.contains("alpha_take_lock"),
        "{m}"
    );
    // Hidden by the ownership table: says who owns it.
    let v = h.request(3, "tools/call", r#"{"name":"beta_take_lock"}"#);
    let m = v
        .get("error")
        .unwrap()
        .get("message")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        m.contains("alpha_take_lock") && m.contains("owned by alpha"),
        "{m}"
    );

    let v = h.request(4, "resources/list", "{}");
    assert_eq!(
        v.get("error").unwrap().get("code").unwrap().as_i64(),
        Some(-32601)
    );
    h.send(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#);

    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"#);
    let v = h.wait("parse error", |v| {
        v.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64)
            == Some(-32700)
    });
    assert!(v.get("id").unwrap().is_null());
    h.send(r#"[{"jsonrpc":"2.0","id":6,"method":"ping"}]"#);
    let v = h.wait("batch error", |v| {
        v.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64)
            == Some(-32600)
    });
    assert!(v.get("id").unwrap().is_null());
    h.request(7, "ping", "{}");
    let out = h.finish();
    // init, list, 2 unknown tools, unknown method, parse, batch, ping.
    assert_eq!(out.len(), 8, "{out:?}");
}

#[test]
fn failed_backend_shows_in_status_and_its_tools_are_absent() {
    let (a, r, _) = three();
    let beta = Arc::new(Fake {
        fail_start: Some("beta-mcp not found on PATH; install it".into()),
        ..Fake::new("beta", &["echo"])
    });
    let h = start(vec![], vec![a, r, beta]);
    h.init();
    let names = h.tool_names(1);
    assert!(!names.iter().any(|n| n.starts_with("beta_")), "{names:?}");
    let v = h.request(
        2,
        "tools/call",
        r#"{"name":"harness_status","arguments":{}}"#,
    );
    let st = parse(&text_of(&v)).unwrap();
    assert_eq!(st.get("config").unwrap().as_str(), Some("test.json"));
    let backends = st.get("backends").unwrap().as_array().unwrap();
    let beta = backends
        .iter()
        .find(|b| b.get("name").unwrap().as_str() == Some("beta"))
        .unwrap();
    assert_eq!(beta.get("state").unwrap().as_str(), Some("failed"));
    assert!(beta
        .get("error")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("not found on PATH"));
    assert_eq!(beta.get("tools").unwrap().as_i64(), Some(0));
    assert_eq!(beta.get("disabled").unwrap().to_json(), r#"["take_lock"]"#);
    let alpha = &backends[0];
    assert_eq!(alpha.get("state").unwrap().as_str(), Some("up"));
    assert_eq!(alpha.get("tools").unwrap().as_i64(), Some(7));
    assert_eq!(
        alpha.get("server").unwrap().to_json(),
        r#"{"name":"alpha-server","version":"9.9"}"#
    );
    assert_eq!(alpha.get("disabled").unwrap().to_json(), r#"["extras"]"#);
    assert_eq!(alpha.get("starts").unwrap().as_i64(), Some(1));
    let own = st.get("ownership").unwrap().as_array().unwrap();
    assert_eq!(
        own[0].to_json(),
        r#"{"feature":"locks","owner":"alpha","why":"one lock table"}"#
    );
    // Calling one of its tools says why.
    let v = h.request(3, "tools/call", r#"{"name":"beta_echo"}"#);
    let m = v
        .get("error")
        .unwrap()
        .get("message")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        m.contains("backend \"beta\" failed") && m.contains("not found on PATH"),
        "{m}"
    );
    h.finish();
}

#[test]
fn first_list_waits_for_a_slow_start_without_blocking_input() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let slow = Arc::new(Fake {
        gate: Some(gate.clone()),
        ..Fake::new("beta", &["echo"])
    });
    let (a, r, _) = three();
    let h = start(vec![], vec![a, r, slow]);
    h.init();
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    // Other requests are answered while the list waits.
    h.request(2, "ping", "{}");
    let st = h.request(3, "tools/call", r#"{"name":"harness_status"}"#);
    assert!(text_of(&st).contains(r#""state":"starting""#));
    assert!(!h
        .lines()
        .iter()
        .any(|v| v.get("id").map(Value::to_json).as_deref() == Some("1")));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let v = h.reply("1");
    let names = v.get("result").unwrap().get("tools").unwrap().to_json();
    assert!(names.contains("beta_echo") && names.contains("alpha_echo"));
    let out = h.finish();
    // The backend settled before the client saw a list: no list_changed.
    assert!(!out.iter().any(|v| v.get("method").is_some()), "{out:?}");
}

/// A backend still starting when input ends is stopped once its start
/// finishes, so no server outlives the harness.
#[test]
fn a_backend_that_finishes_starting_during_shutdown_is_stopped() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let slow = Arc::new(Fake {
        gate: Some(gate.clone()),
        ..Fake::new("beta", &["echo"])
    });
    let (a, r, _) = three();
    let h = start(vec![], vec![a, r, slow.clone()]);
    h.init();
    // The startup fetch must be inside `start` (past its closing check)
    // before input ends; otherwise it rightly never starts at all.
    slow.wait_until("start entered", || slow.entered.load(Ordering::SeqCst));
    let opener = {
        let gate = gate.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        })
    };
    h.finish();
    opener.join().unwrap();
    assert!(slow.started.load(Ordering::SeqCst));
    assert!(
        slow.shut_after_start.load(Ordering::SeqCst),
        "the backend started after shutdown and was never stopped"
    );
}

#[test]
fn eof_shuts_backends_down() {
    let (a, r, b) = three();
    let h = start(vec![], vec![a.clone(), r.clone(), b.clone()]);
    h.init();
    h.tool_names(1);
    // A call still running at EOF fails when its backend shuts down.
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"alpha_slow"}}"#);
    a.wait_until("the slow call", || !a.calls.lock().unwrap().is_empty());
    let out = h.finish();
    assert!(
        a.shut.load(Ordering::SeqCst)
            && r.shut.load(Ordering::SeqCst)
            && b.shut.load(Ordering::SeqCst)
    );
    let v = out
        .iter()
        .find(|v| v.get("id").map(Value::to_json).as_deref() == Some("9"))
        .expect("reply to 9");
    assert!(is_error(v));
}

// ---------- regressions from the adversarial review ----------

fn is_list_changed(v: &Value) -> bool {
    v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
}

fn status_of(h: &Harness, id: i64, backend: &str) -> Value {
    let v = h.request(id, "tools/call", r#"{"name":"harness_status"}"#);
    let st = parse(&text_of(&v)).unwrap();
    st.get("backends")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b.get("name").unwrap().as_str() == Some(backend))
        .unwrap()
        .clone()
}

/// A backend whose first start fails is fetched again by a call under its
/// prefix: the call is routed and the client hears list_changed.
#[test]
fn failed_first_start_is_retried_by_a_call() {
    let (a, r, _) = three();
    let beta = Arc::new(Fake::new("beta", &["echo"]));
    beta.flaky_starts.store(1, Ordering::SeqCst);
    let h = start(vec![], vec![a, r, beta.clone()]);
    h.init();
    assert!(!h.tool_names(1).contains(&"beta_echo".to_string()));
    let v = h.request(2, "tools/call", r#"{"name":"beta_echo"}"#);
    assert!(text_of(&v).contains(r#""tool":"echo""#), "{}", v.to_json());
    h.wait("list_changed", is_list_changed);
    assert!(h.tool_names(3).contains(&"beta_echo".to_string()));
    assert_eq!(
        status_of(&h, 4, "beta").get("state").unwrap().as_str(),
        Some("up")
    );
    h.finish();
}

/// Without any call, the retry timer brings the backend back.
#[test]
fn failed_first_start_is_retried_by_the_timer() {
    let (a, r, _) = three();
    let beta = Arc::new(Fake::new("beta", &["echo"]));
    beta.flaky_starts.store(1, Ordering::SeqCst);
    let h = start(vec![], vec![a, r, beta.clone()]);
    h.init();
    assert!(!h.tool_names(1).contains(&"beta_echo".to_string()));
    h.wait("list_changed", is_list_changed);
    assert!(h.tool_names(2).contains(&"beta_echo".to_string()));
    let out = h.finish();
    assert_eq!(out.iter().filter(|v| is_list_changed(v)).count(), 1);
}

/// A call under a backend that keeps failing tries it again and says why.
#[test]
fn call_under_a_failing_backend_reports_its_error() {
    let (a, r, _) = three();
    let beta = Arc::new(Fake::new("beta", &["echo"]));
    beta.flaky_starts.store(1000, Ordering::SeqCst);
    let h = start(vec![], vec![a, r, beta.clone()]);
    h.init();
    h.tool_names(1);
    let before = beta.flaky_starts.load(Ordering::SeqCst);
    let v = h.request(2, "tools/call", r#"{"name":"beta_echo"}"#);
    let e = v.get("error").unwrap();
    assert_eq!(e.get("code").unwrap().as_i64(), Some(-32602));
    let m = e.get("message").unwrap().as_str().unwrap();
    assert!(
        m.contains("backend \"beta\" failed") && m.contains("no initialize reply"),
        "{m}"
    );
    assert!(
        beta.flaky_starts.load(Ordering::SeqCst) < before,
        "not retried"
    );
    h.finish();
}

/// A panic in a backend's call_tool is an isError result, and the proxy
/// keeps serving.
#[test]
fn backend_panic_is_an_error_result() {
    let (_, r, b) = three();
    let alpha = Arc::new(Fake::new("alpha", &["panic", "echo"]));
    let h = start(vec![], vec![alpha, r, b]);
    h.init();
    h.tool_names(1);
    let v = h.request(42, "tools/call", r#"{"name":"alpha_panic"}"#);
    assert!(is_error(&v), "{}", v.to_json());
    let t = text_of(&v);
    assert!(
        t.contains("panicked") && t.contains("bug in backend"),
        "{t}"
    );
    let v = h.request(43, "tools/call", r#"{"name":"alpha_echo"}"#);
    assert!(text_of(&v).contains("echo"));
    h.finish();
}

/// `hide_tools` hides a backend's tool in the proxy: not listed, and a call
/// gets the unknown-tool error. A disabled tool the backend still lists is
/// flagged in harness_status.
#[test]
fn hide_tools_hides_and_ignored_disable_is_flagged() {
    let text = CONFIG.replace(
        r#""command": "beta-mcp","#,
        r#""command": "beta-mcp", "hide_tools": ["sec*"],"#,
    );
    let cfg = Config::from_json(&text, "test.json").unwrap();
    let (a, r, _) = three();
    let beta = Arc::new(Fake::new(
        "beta",
        &["echo", "take_lock", "secret", "second"],
    ));
    let h = start_cfg(cfg, vec![], vec![a, r, beta.clone()]);
    h.init();
    let names = h.tool_names(1);
    assert!(names.contains(&"beta_echo".to_string()), "{names:?}");
    assert!(
        !names.iter().any(|n| n.starts_with("beta_sec")),
        "{names:?}"
    );
    let v = h.request(2, "tools/call", r#"{"name":"beta_secret"}"#);
    let e = v.get("error").unwrap();
    assert_eq!(e.get("code").unwrap().as_i64(), Some(-32602));
    let m = e.get("message").unwrap().as_str().unwrap();
    assert!(m.starts_with("unknown tool \"beta_secret\""), "{m}");
    assert!(beta.calls.lock().unwrap().is_empty());
    let st = status_of(&h, 3, "beta");
    assert_eq!(st.get("hidden").unwrap().as_i64(), Some(2));
    assert_eq!(st.get("tools").unwrap().as_i64(), Some(2));
    assert_eq!(
        st.get("ignored_disable").unwrap().to_json(),
        r#"["take_lock"]"#
    );
    let alpha = status_of(&h, 4, "alpha");
    assert_eq!(alpha.get("ignored_disable").unwrap().to_json(), "[]");
    assert_eq!(alpha.get("hidden").unwrap().as_i64(), Some(0));
    h.finish();
}

/// `only_tools` matches the backend's own name, before the prefix is stripped.
#[test]
fn only_tools_matches_the_own_name() {
    let text = CONFIG.replace(
        r#""strip_prefix": "relatum_""#,
        r#""strip_prefix": "relatum_", "only_tools": ["relatum_g*"]"#,
    );
    let cfg = Config::from_json(&text, "test.json").unwrap();
    let (a, r, b) = three();
    let h = start_cfg(cfg, vec![], vec![a, r, b]);
    h.init();
    let names = h.tool_names(1);
    let rel: Vec<&String> = names.iter().filter(|n| n.starts_with("relatum_")).collect();
    assert_eq!(rel, ["relatum_get"], "{names:?}");
    h.finish();
}

/// An id without method, result or error is an invalid request; a response
/// (result or error) is ignored.
#[test]
fn request_without_method_is_rejected() {
    let h = start(vec![], vec![]);
    h.init();
    h.send(r#"{"jsonrpc":"2.0","id":5,"params":{}}"#);
    let v = h.reply("5");
    assert_eq!(
        v.get("error").unwrap().get("code").unwrap().as_i64(),
        Some(-32600)
    );
    h.send(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#);
    h.send(r#"{"jsonrpc":"2.0","id":8,"error":{"code":1,"message":"x"}}"#);
    h.request(6, "ping", "{}");
    let out = h.finish();
    // initialize, the -32600, the ping.
    assert_eq!(out.len(), 3, "{out:?}");
}

/// Ids of every legal shape come back verbatim.
#[test]
fn ids_are_verbatim() {
    let h = start(vec![], vec![]);
    for id in [
        r#""x\"y""#,
        "12345678901234567890",
        "1.0",
        "-0",
        "1e3",
        "null",
    ] {
        h.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#));
        h.reply(id);
    }
    h.finish();
}

/// We never issue cursors, so any cursor is invalid.
#[test]
fn bogus_cursor_is_rejected() {
    let h = start(vec![], vec![]);
    h.init();
    let v = h.request(1, "tools/list", r#"{"cursor":"bogus"}"#);
    assert_eq!(
        v.get("error").unwrap().get("code").unwrap().as_i64(),
        Some(-32602),
        "{}",
        v.to_json()
    );
    h.finish();
}

/// A line over the cap is answered -32700 (id null) and skipped; the next
/// line is served.
#[test]
fn over_long_line_is_skipped() {
    let h = start(vec![], vec![]);
    let tx = h.tx.as_ref().unwrap();
    tx.send(br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"pad":""#.to_vec())
        .unwrap();
    let chunk = vec![b' '; 1 << 20];
    for _ in 0..=(mh_proxy::MAX_LINE >> 20) {
        tx.send(chunk.clone()).unwrap();
    }
    tx.send(b"\"}}\n".to_vec()).unwrap();
    h.request(2, "ping", "{}");
    let out = h.finish();
    assert_eq!(out.len(), 2, "{out:?}");
    let e = out[0].get("error").unwrap();
    assert_eq!(e.get("code").unwrap().as_i64(), Some(-32700));
    assert!(out[0].get("id").unwrap().is_null());
    assert_eq!(out[1].get("id").unwrap().to_json(), "2");
}
