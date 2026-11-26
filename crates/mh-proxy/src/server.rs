//! The MCP server loop: reads requests on the calling thread, answers slow
//! work (tool calls, the first `tools/list`) on threads of their own, and
//! keeps the merged catalog current as backends come and go.

use crate::catalog::{Catalog, Target};
use crate::text;
use crate::NativeTool;
use mh_backend::{Backend, BackendEvent, CallError, CancelToken, EventSink};
use mh_core::Config;
use mh_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Answered when the client asks for a version not in the list.
pub const DEFAULT_PROTOCOL: &str = "2025-06-18";
/// The first `tools/list` waits at most this long (from startup) for backends.
const LIST_WAIT: Duration = Duration::from_secs(20);
/// On input EOF, how long to wait for calls still running.
const DRAIN_WAIT: Duration = Duration::from_secs(3);

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn log(msg: &str) {
    eprintln!("mem-harness: {msg}");
}

/// What the proxy knows about one backend's tools.
#[derive(Clone, Default)]
struct Slot {
    /// Bumped by every fetch; a fetch whose number is stale is discarded.
    generation: u64,
    /// The startup fetch (or a later one) has finished, ok or not.
    settled: bool,
    tools: Vec<Value>,
    /// Why the last start or `tools/list` failed.
    error: Option<String>,
    /// Why the process last ended, from `BackendEvent::Exited`.
    exited: Option<String>,
}

#[derive(Default)]
struct State {
    slots: Vec<Slot>,
    catalog: Catalog,
    initialized: bool,
    /// The client has been sent a `tools/list` reply.
    listed: bool,
    logged: HashSet<String>,
}

pub struct Shared {
    cfg: Config,
    instructions: String,
    natives: Vec<NativeTool>,
    native_defs: Vec<Value>,
    backends: OnceLock<Vec<Arc<dyn Backend>>>,
    out: Mutex<Box<dyn Write + Send>>,
    state: Mutex<State>,
    settled_cv: Condvar,
    inflight: Mutex<HashMap<String, CancelToken>>,
    active: Mutex<usize>,
    active_cv: Condvar,
    started: Instant,
    closing: AtomicBool,
}

/// Decrements the active-thread count when a tracked thread ends.
struct ActiveGuard(Arc<Shared>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut n = lock(&self.0.active);
        *n = n.saturating_sub(1);
        self.0.active_cv.notify_all();
    }
}

enum Reply {
    Result(Value),
    Error(i64, String, Option<Value>),
    None,
}

pub fn run<R: BufRead, W: Write + Send + 'static>(
    cfg: Config,
    natives: Vec<NativeTool>,
    make_backends: impl FnOnce(EventSink) -> Vec<Arc<dyn Backend>>,
    mut input: R,
    output: W,
) -> Result<(), String> {
    let mut native_defs = vec![text::status_def()];
    native_defs.extend(natives.iter().map(|n| n.def.clone()));
    let shared = Arc::new(Shared {
        instructions: text::instructions(&cfg),
        cfg,
        natives,
        native_defs,
        backends: OnceLock::new(),
        out: Mutex::new(Box::new(output)),
        state: Mutex::new(State::default()),
        settled_cv: Condvar::new(),
        inflight: Mutex::new(HashMap::new()),
        active: Mutex::new(0),
        active_cv: Condvar::new(),
        started: Instant::now(),
        closing: AtomicBool::new(false),
    });

    let weak = Arc::downgrade(&shared);
    let sink: EventSink = Arc::new(move |name: &str, ev: BackendEvent| {
        if let Some(s) = weak.upgrade() {
            s.on_event(name, ev);
        }
    });
    let backends = make_backends(sink);
    let n = backends.len();
    let _ = shared.backends.set(backends);
    {
        let mut st = lock(&shared.state);
        st.slots = vec![Slot::default(); n];
        shared.rebuild(&mut st);
    }
    for i in 0..n {
        let s = shared.clone();
        let name = format!("mh-start-{}", shared.backends()[i].name());
        let spawned = thread::Builder::new()
            .name(name)
            .spawn(move || s.fetch(i, true));
        if let Err(e) = spawned {
            let mut st = lock(&shared.state);
            st.slots[i].settled = true;
            st.slots[i].error = Some(format!("cannot start a thread: {e}"));
            shared.settled_cv.notify_all();
        }
    }

    let mut result = Ok(());
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match input.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                result = Err(format!("reading the client's input: {e}"));
                break;
            }
        }
        match std::str::from_utf8(&buf) {
            Ok(line) if line.trim().is_empty() => {}
            Ok(line) => shared.handle_line(line.trim()),
            Err(_) => shared.send_error(
                Value::Null,
                PARSE_ERROR,
                "parse error: invalid UTF-8".into(),
                None,
            ),
        }
    }

    shared.closing.store(true, Ordering::SeqCst);
    let stoppers: Vec<_> = shared
        .backends()
        .iter()
        .cloned()
        .filter_map(|b| thread::Builder::new().spawn(move || b.shutdown()).ok())
        .collect();
    for h in stoppers {
        let _ = h.join();
    }
    let deadline = Instant::now() + DRAIN_WAIT;
    let mut active = lock(&shared.active);
    while *active > 0 {
        let now = Instant::now();
        if now >= deadline {
            log(&format!("{} call(s) still running at exit", *active));
            break;
        }
        active = shared
            .active_cv
            .wait_timeout(active, deadline - now)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    result
}

impl Shared {
    fn backends(&self) -> &[Arc<dyn Backend>] {
        self.backends.get().map(Vec::as_slice).unwrap_or(&[])
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.backends().iter().position(|b| b.name() == name)
    }

    // ---- output ----

    fn write(&self, msg: &Value) {
        let mut line = msg.to_json();
        line.push('\n');
        let mut out = lock(&self.out);
        if let Err(e) = out.write_all(line.as_bytes()).and_then(|_| out.flush()) {
            if !self.closing.load(Ordering::SeqCst) {
                log(&format!("writing to the client failed: {e}"));
            }
        }
    }

    fn send_result(&self, id: Value, result: Value) {
        self.write(&Value::obj(vec![
            ("jsonrpc", Value::str("2.0")),
            ("id", id),
            ("result", result),
        ]));
    }

    fn send_error(&self, id: Value, code: i64, message: String, data: Option<Value>) {
        let mut err = Value::obj(vec![
            ("code", Value::int(code)),
            ("message", Value::str(message)),
        ]);
        if let Some(d) = data {
            err.set("data", d);
        }
        self.write(&Value::obj(vec![
            ("jsonrpc", Value::str("2.0")),
            ("id", id),
            ("error", err),
        ]));
    }

    fn send(&self, id: Value, reply: Reply) {
        match reply {
            Reply::Result(r) => self.send_result(id, r),
            Reply::Error(code, msg, data) => self.send_error(id, code, msg, data),
            Reply::None => {}
        }
    }

    // ---- threads ----

    /// Run `f` on a new thread counted as in flight. On failure to spawn,
    /// answers `id` (if any) with an internal error.
    fn spawn_tracked(
        self: &Arc<Self>,
        what: &str,
        id: Option<Value>,
        f: impl FnOnce() + Send + 'static,
    ) {
        *lock(&self.active) += 1;
        let guard = ActiveGuard(self.clone());
        let spawned = thread::Builder::new()
            .name(format!("mh-{what}"))
            .spawn(move || {
                let _guard = guard;
                f();
            });
        if let Err(e) = spawned {
            log(&format!("cannot start a thread for {what}: {e}"));
            if let Some(id) = id {
                self.send_error(
                    id,
                    INTERNAL_ERROR,
                    format!("cannot start a thread for {what}: {e}"),
                    None,
                );
            }
        }
    }

    // ---- catalog ----

    /// Rebuild the catalog from the slots; true when it changed.
    fn rebuild(&self, st: &mut State) -> bool {
        let names: Vec<&str> = self.backends().iter().map(|b| b.name()).collect();
        let lists: Vec<(&str, &[Value])> = names
            .iter()
            .zip(st.slots.iter())
            .map(|(n, s)| (*n, s.tools.as_slice()))
            .collect();
        let (cat, notes) = Catalog::build(&self.cfg, &self.native_defs, &lists);
        for note in notes {
            if st.logged.insert(note.clone()) {
                log(&note);
            }
        }
        let changed = cat.json != st.catalog.json;
        st.catalog = cat;
        changed
    }

    /// Rebuild, and tell the client if the list changed after it has seen one.
    fn rebuild_and_notify(&self, st: &mut State) {
        if self.rebuild(st) && st.initialized && st.listed && !self.closing.load(Ordering::SeqCst) {
            self.write(&Value::obj(vec![
                ("jsonrpc", Value::str("2.0")),
                ("method", Value::str("notifications/tools/list_changed")),
            ]));
        }
    }

    /// Fetch one backend's tools (starting it first at startup) and fold
    /// the result into the catalog. Never called with the state lock held.
    fn fetch(self: &Arc<Self>, i: usize, start_first: bool) {
        let generation = {
            let mut st = lock(&self.state);
            st.slots[i].generation += 1;
            st.slots[i].generation
        };
        let b = self.backends()[i].clone();
        let res = if start_first {
            b.start().and_then(|_| b.list_tools())
        } else {
            b.list_tools()
        };
        let mut st = lock(&self.state);
        if st.slots[i].generation != generation {
            return;
        }
        let slot = &mut st.slots[i];
        slot.settled = true;
        match res {
            Ok(tools) => {
                slot.tools = tools;
                slot.error = None;
            }
            Err(e) => {
                let msg = match e {
                    CallError::Unavailable(s) | CallError::Timeout(s) => s,
                    other => other.to_string(),
                };
                log(&format!("backend {:?}: {msg}", b.name()));
                slot.error = Some(msg);
            }
        }
        self.rebuild_and_notify(&mut st);
        self.settled_cv.notify_all();
    }

    fn on_event(self: &Arc<Self>, name: &str, ev: BackendEvent) {
        match ev {
            BackendEvent::Relay(msg) => {
                if !self.closing.load(Ordering::SeqCst) {
                    self.write(&msg);
                }
            }
            BackendEvent::ToolsChanged => {
                let Some(i) = self.index_of(name) else { return };
                if self.closing.load(Ordering::SeqCst) {
                    return;
                }
                // Not on the backend's own thread: list_tools waits for it.
                let s = self.clone();
                if let Err(e) = thread::Builder::new()
                    .name(format!("mh-refetch-{name}"))
                    .spawn(move || s.fetch(i, false))
                {
                    log(&format!("backend {name:?}: cannot refetch its tools: {e}"));
                }
            }
            BackendEvent::Exited(why) => {
                let Some(i) = self.index_of(name) else { return };
                log(&format!("backend {name:?} exited: {why}"));
                let mut st = lock(&self.state);
                // Its tools stay listed: the next call starts it again.
                st.slots[i].exited = Some(why);
                self.rebuild_and_notify(&mut st);
            }
        }
    }

    // ---- requests ----

    fn handle_line(self: &Arc<Self>, line: &str) {
        let msg = match mh_json::parse(line) {
            Ok(v) => v,
            Err(e) => {
                return self.send_error(Value::Null, PARSE_ERROR, format!("parse error: {e}"), None)
            }
        };
        let pairs = match &msg {
            Value::Object(_) => &msg,
            Value::Array(_) => {
                return self.send_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "invalid request: batches are not supported; send one message per line".into(),
                    None,
                )
            }
            _ => {
                return self.send_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "invalid request: expected an object".into(),
                    None,
                )
            }
        };
        let id = pairs.get("id").cloned();
        let method = match pairs.get("method") {
            Some(Value::Str(m)) => m.clone(),
            Some(_) => {
                return self.send_error(
                    id.unwrap_or(Value::Null),
                    INVALID_REQUEST,
                    "invalid request: method must be a string".into(),
                    None,
                )
            }
            // A response to a request we never sent: ignore.
            None => return,
        };
        let params = pairs.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            return self.on_notification(&method, &params);
        };
        match method.as_str() {
            "initialize" => self.on_initialize(id, &params),
            "ping" => self.send_result(id, Value::obj(vec![])),
            "tools/list" => self.on_list(id),
            "tools/call" => self.on_call(id, &params),
            other => self.send_error(
                id,
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
                None,
            ),
        }
    }

    fn on_notification(&self, method: &str, params: &Value) {
        if method == "notifications/cancelled" {
            if let Some(req) = params.get("requestId") {
                if let Some(token) = lock(&self.inflight).get(&req.to_json()) {
                    token.cancel();
                }
            }
        }
    }

    fn on_initialize(&self, id: Value, params: &Value) {
        let asked = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        let version = PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == asked)
            .copied()
            .unwrap_or(DEFAULT_PROTOCOL);
        let result = Value::obj(vec![
            ("protocolVersion", Value::str(version)),
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
                    ("name", Value::str("mem-harness")),
                    ("version", Value::str(env!("CARGO_PKG_VERSION"))),
                ]),
            ),
            ("instructions", Value::str(self.instructions.clone())),
        ]);
        lock(&self.state).initialized = true;
        self.send_result(id, result);
    }

    fn all_settled(st: &State) -> bool {
        st.slots.iter().all(|s| s.settled)
    }

    fn reply_list(&self, id: Value) {
        let mut st = lock(&self.state);
        st.listed = true;
        let result = Value::obj(vec![("tools", Value::Array(st.catalog.tools.clone()))]);
        // Written under the state lock so no list_changed can overtake it.
        self.send_result(id, result);
    }

    fn on_list(self: &Arc<Self>, id: Value) {
        let deadline = self.started + LIST_WAIT;
        if Self::all_settled(&lock(&self.state)) || Instant::now() >= deadline {
            return self.reply_list(id);
        }
        let s = self.clone();
        self.spawn_tracked("tools-list", Some(id.clone()), move || {
            let mut st = lock(&s.state);
            while !Self::all_settled(&st) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                st = s
                    .settled_cv
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            drop(st);
            s.reply_list(id);
        });
    }

    fn on_call(self: &Arc<Self>, id: Value, params: &Value) {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return self.send_error(
                id,
                INVALID_PARAMS,
                "tools/call: params.name must be a string".into(),
                None,
            );
        };
        let target = lock(&self.state).catalog.targets.get(name).cloned();
        let Some(target) = target else {
            let msg = self.unknown_tool(name);
            return self.send_error(id, INVALID_PARAMS, msg, None);
        };
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Value::obj(vec![]),
            Some(a) => a.clone(),
        };
        let meta = params.get("_meta").cloned();
        let key = id.to_json();
        let token = CancelToken::new();
        lock(&self.inflight).insert(key.clone(), token.clone());
        let s = self.clone();
        let name = name.to_string();
        self.spawn_tracked(&format!("call-{name}"), Some(id.clone()), move || {
            let reply = s.call(&name, target, arguments, meta, &token);
            lock(&s.inflight).remove(&key);
            if !token.is_cancelled() {
                s.send(id, reply);
            }
        });
    }

    fn call(
        &self,
        name: &str,
        target: Target,
        arguments: Value,
        meta: Option<Value>,
        token: &CancelToken,
    ) -> Reply {
        match target {
            Target::Status => Reply::Result(text::text_result(self.status_json().to_json(), false)),
            Target::Native(i) => {
                let run = self.natives[i].run.clone();
                match catch_unwind(AssertUnwindSafe(|| run(arguments))) {
                    Ok(Ok(v)) => Reply::Result(v),
                    Ok(Err(t)) => Reply::Result(text::text_result(t, true)),
                    Err(_) => Reply::Result(text::text_result(
                        format!("{name} failed: the harness tool panicked"),
                        true,
                    )),
                }
            }
            Target::Backend(i, tool) => {
                let b = self.backends()[i].clone();
                match b.call_tool(&tool, arguments, meta, token) {
                    Ok(v) => Reply::Result(v),
                    Err(CallError::Rpc {
                        code,
                        message,
                        data,
                    }) => Reply::Error(code, message, data),
                    Err(CallError::Unavailable(why)) => Reply::Result(text::text_result(
                        text::unavailable(name, b.name(), &why),
                        true,
                    )),
                    Err(CallError::Timeout(why)) => {
                        Reply::Result(text::text_result(text::timeout(name, b.name(), &why), true))
                    }
                    Err(CallError::Cancelled) => Reply::None,
                }
            }
        }
    }

    /// The -32602 message for a name not in the catalog, with hints.
    fn unknown_tool(&self, name: &str) -> String {
        let mut msg = format!("unknown tool {name:?}");
        let mut prefixes: Vec<&str> = self.backends().iter().map(|b| b.name()).collect();
        prefixes.push("harness");
        let st = lock(&self.state);
        let similar = st.catalog.similar(name, &prefixes);
        if !similar.is_empty() {
            let list: Vec<String> = similar.iter().map(|s| format!("{s:?}")).collect();
            msg.push_str(&format!("; a similar tool exists: {}", list.join(", ")));
        }
        // Is it under a backend that failed, or hidden by the ownership table?
        let owner = self
            .backends()
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                name.strip_prefix(b.name())
                    .and_then(|r| r.strip_prefix('_'))
                    .map(|r| (i, b.name(), r))
            })
            .max_by_key(|(_, b, _)| b.len());
        if let Some((i, bname, rest)) = owner {
            let slot = &st.slots[i];
            if !slot.settled {
                msg.push_str(&format!(
                    "; backend {bname:?} is still starting, list tools again shortly"
                ));
            } else if let Some(err) = &slot.error {
                msg.push_str(&format!(
                    "; backend {bname:?} failed: {err} (see harness_status)"
                ));
            }
            for o in &self.cfg.ownership {
                if let Some(list) = o.disable.get(bname) {
                    if list.iter().any(|t| t == rest) {
                        msg.push_str(&format!(
                            "; it is disabled in {bname} because {:?} is owned by {}",
                            o.feature, o.owner
                        ));
                    }
                }
            }
        }
        msg
    }

    fn status_json(&self) -> Value {
        let (slots, counts) = {
            let st = lock(&self.state);
            (st.slots.clone(), st.catalog.counts.clone())
        };
        let mut backends = Vec::new();
        for (i, b) in self.backends().iter().enumerate() {
            // Asked without the state lock: a backend may hold its own lock
            // while it reports an event.
            let status = b.status();
            let slot = slots.get(i).cloned().unwrap_or_default();
            let state = if !slot.settled {
                "starting"
            } else if slot.error.is_some() {
                "failed"
            } else if status.up {
                "up"
            } else {
                "down"
            };
            let error = slot.error.clone().or(status.error.clone());
            let opt = |s: Option<String>| s.map(Value::str).unwrap_or(Value::Null);
            let server = match &status.server_info {
                Some(info) => Value::obj(vec![
                    ("name", info.get("name").cloned().unwrap_or(Value::Null)),
                    (
                        "version",
                        info.get("version").cloned().unwrap_or(Value::Null),
                    ),
                ]),
                None => Value::Null,
            };
            let disabled = self
                .cfg
                .disabled_in(b.name())
                .into_iter()
                .map(Value::str)
                .collect();
            backends.push(Value::obj(vec![
                ("name", Value::str(b.name())),
                ("state", Value::str(state)),
                ("up", Value::Bool(status.up)),
                ("starts", Value::int(i64::from(status.starts))),
                ("error", opt(error)),
                ("exited", opt(slot.exited.clone())),
                ("server", server),
                (
                    "tools",
                    Value::int(counts.get(i).copied().unwrap_or(0) as i64),
                ),
                ("disabled", Value::Array(disabled)),
            ]));
        }
        let ownership = self
            .cfg
            .ownership
            .iter()
            .map(|o| {
                Value::obj(vec![
                    ("feature", Value::str(o.feature.clone())),
                    ("owner", Value::str(o.owner.clone())),
                    ("why", Value::str(o.why.clone())),
                ])
            })
            .collect();
        Value::obj(vec![
            ("config", Value::str(self.cfg.source.clone())),
            ("backends", Value::Array(backends)),
            ("ownership", Value::Array(ownership)),
            ("native_tools", Value::int(self.native_defs.len() as i64)),
        ])
    }
}
