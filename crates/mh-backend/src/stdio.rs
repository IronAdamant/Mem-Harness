//! The stdio MCP client: one suite tool's MCP server run as a child
//! process, spoken to with newline-delimited JSON-RPC 2.0 on its stdin and
//! stdout (one message per line).
//!
//! Nothing is spawned until the first `start`, `list_tools` or `call_tool`.
//! A server that exits is restarted on the next use, unless it keeps dying
//! right after starting (see [`Limits::restart_block`]).
//!
//! Threads per running server: one reads stdout and routes replies to the
//! waiting caller by id, one keeps the last stderr lines and echoes them to
//! our stderr prefixed `[<name>] `.

use crate::{Backend, BackendEvent, BackendStatus, CallError, CancelToken, EventSink};
use mh_core::BackendSpec;
use mh_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// The MCP protocol version sent in `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// How many stderr lines are kept for error messages.
const STDERR_LINES: usize = 20;
/// An exit this soon after start counts towards the restart-storm guard.
const QUICK_EXIT: Duration = Duration::from_secs(5);
/// This many quick exits in a row block restarts for `restart_block`.
const STORM_EXITS: u32 = 3;
/// How often a waiting call checks its cancel token and deadline.
const POLL: Duration = Duration::from_millis(150);
/// The most `tools/list` pages followed before giving up.
const MAX_PAGES: usize = 1000;

/// Time limits. The defaults suit the suite tools; tests shorten them.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Wait for the `initialize` reply.
    pub handshake: Duration,
    /// Wait for each `tools/list` page.
    pub page: Duration,
    /// Overall cap on one `tools/call` (some suite calls block for minutes).
    pub call: Duration,
    /// After [`STORM_EXITS`] exits in a row, each within 5 s of starting,
    /// refuse to restart for this long.
    pub restart_block: Duration,
    /// On shutdown, how long to wait after closing stdin before killing.
    pub shutdown_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            handshake: Duration::from_secs(20),
            page: Duration::from_secs(20),
            call: Duration::from_secs(3600),
            restart_block: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(2),
        }
    }
}

type Reply = Result<Value, CallError>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Calls waiting for a reply, and why the connection ended (once it has).
#[derive(Default)]
struct Pending {
    dead: Option<String>,
    waiting: HashMap<u64, Sender<Reply>>,
}

/// One running server process.
struct Conn {
    /// `command args...` as configured, for messages.
    label: String,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    pending: Mutex<Pending>,
    tail: Arc<Mutex<VecDeque<String>>>,
    stderr_done: Arc<AtomicBool>,
    started: Instant,
    /// The handshake finished.
    ready: AtomicBool,
    /// We are stopping it on purpose (shutdown): no storm count, no event.
    closing: AtomicBool,
    /// `end` has run.
    ended: AtomicBool,
}

impl Conn {
    /// Write one message as one line, under the stdin lock.
    fn send(&self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_json();
        line.push('\n');
        let mut stdin = lock(&self.stdin);
        let w = stdin.as_mut().ok_or("its stdin is closed")?;
        w.write_all(line.as_bytes())
            .and_then(|_| w.flush())
            .map_err(|e| e.to_string())
    }

    /// `; last stderr lines:\n  ...` or `; no stderr output`.
    fn tail_text(&self) -> String {
        let tail = lock(&self.tail);
        if tail.is_empty() {
            "; no stderr output".to_string()
        } else {
            let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
            format!("; last stderr lines:\n  {}", lines.join("\n  "))
        }
    }

    /// Wait up to `grace` for the process to exit, then kill it.
    fn reap(&self, grace: Duration) -> String {
        let deadline = Instant::now() + grace;
        loop {
            {
                let mut child = lock(&self.child);
                match child.try_wait() {
                    Ok(Some(st)) => return st.to_string(),
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        return match child.wait() {
                            Ok(st) => format!("{st}, killed"),
                            Err(e) => format!("killed; wait failed: {e}"),
                        };
                    }
                    Ok(None) => {}
                    Err(e) => return format!("exit status unknown: {e}"),
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn live(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && !self.ended.load(Ordering::SeqCst)
    }
}

struct Inner {
    conn: Option<Arc<Conn>>,
    status: BackendStatus,
    /// Exits in a row that came within `QUICK_EXIT` of their start.
    quick_exits: u32,
    /// When the storm guard tripped.
    blocked_at: Option<Instant>,
    /// Start attempts so far, and the error of the last one (None if it worked).
    attempts: u64,
    last_attempt: Option<CallError>,
}

/// State shared with the reader threads.
struct Core {
    name: String,
    sink: EventSink,
    next_id: AtomicU64,
    inner: Mutex<Inner>,
}

impl Core {
    /// Send a request and wait for its reply.
    fn request(
        &self,
        conn: &Conn,
        method: &str,
        params: Value,
        limit: Duration,
        cancel: Option<&CancelToken>,
    ) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut p = lock(&conn.pending);
            if let Some(why) = &p.dead {
                return Err(CallError::Unavailable(why.clone()));
            }
            p.waiting.insert(id, tx);
        }
        let msg = Value::obj(vec![
            ("jsonrpc", Value::str("2.0")),
            ("id", Value::int(id as i64)),
            ("method", Value::str(method)),
            ("params", params),
        ]);
        if let Err(e) = conn.send(&msg) {
            let mut p = lock(&conn.pending);
            p.waiting.remove(&id);
            let why = p.dead.clone().unwrap_or_else(|| {
                format!(
                    "{}: cannot write to `{}`: {e}{}",
                    self.name,
                    conn.label,
                    conn.tail_text()
                )
            });
            return Err(CallError::Unavailable(why));
        }
        let deadline = Instant::now() + limit;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Disconnected) => {
                    let why = lock(&conn.pending).dead.clone();
                    return Err(CallError::Unavailable(
                        why.unwrap_or_else(|| format!("{}: connection closed", self.name)),
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.is_some_and(CancelToken::is_cancelled) {
                        self.abandon(conn, id, "cancelled by client");
                        return Err(CallError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        self.abandon(conn, id, "timed out");
                        return Err(CallError::Timeout(format!(
                            "{}: no reply to {method} within {} s",
                            self.name,
                            limit.as_secs_f64()
                        )));
                    }
                }
            }
        }
    }

    /// Stop waiting for `id` and tell the server.
    fn abandon(&self, conn: &Conn, id: u64, reason: &str) {
        lock(&conn.pending).waiting.remove(&id);
        let _ = conn.send(&notification(
            "notifications/cancelled",
            Value::obj(vec![
                ("requestId", Value::int(id as i64)),
                ("reason", Value::str(reason)),
            ]),
        ));
    }

    /// Route one message from the server.
    fn handle(&self, conn: &Conn, msg: Value) {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    Value::obj(vec![
                        ("jsonrpc", Value::str("2.0")),
                        ("id", id),
                        ("result", Value::obj(vec![])),
                    ])
                } else {
                    Value::obj(vec![
                        ("jsonrpc", Value::str("2.0")),
                        ("id", id),
                        (
                            "error",
                            Value::obj(vec![
                                ("code", Value::int(-32601)),
                                ("message", Value::str(format!("method not found: {method}"))),
                            ]),
                        ),
                    ])
                };
                if let Err(e) = conn.send(&reply) {
                    eprintln!(
                        "[{}] cannot answer the server's {method} request: {e}",
                        self.name
                    );
                }
            }
            (Some(method), None) => match method.as_str() {
                "notifications/tools/list_changed" => {
                    (self.sink)(&self.name, BackendEvent::ToolsChanged)
                }
                "notifications/progress" | "notifications/message" => {
                    (self.sink)(&self.name, BackendEvent::Relay(msg))
                }
                _ => {}
            },
            (None, Some(id)) => {
                let reply = if let Some(err) = msg.get("error") {
                    Err(CallError::Rpc {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        data: err.get("data").cloned(),
                    })
                } else if let Some(result) = msg.get("result") {
                    Ok(result.clone())
                } else {
                    eprintln!(
                        "[{}] skipping a reply with neither result nor error: {}",
                        self.name,
                        clip(&msg.to_json())
                    );
                    return;
                };
                // A reply for a call we gave up on (cancelled, timed out) is dropped.
                let waiter = id
                    .as_i64()
                    .and_then(|n| u64::try_from(n).ok())
                    .and_then(|n| lock(&conn.pending).waiting.remove(&n));
                if let Some(tx) = waiter {
                    let _ = tx.send(reply);
                }
            }
            (None, None) => eprintln!(
                "[{}] skipping a message with no method and no id: {}",
                self.name,
                clip(&msg.to_json())
            ),
        }
    }

    /// The connection is over: fail every waiting call with `reason`, make
    /// sure the process is gone, update the status and the storm guard,
    /// and report the exit. Runs once per connection.
    fn end(&self, conn: &Arc<Conn>, reason: String) {
        if conn.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut p = lock(&conn.pending);
            p.dead = Some(reason.clone());
            for (_, tx) in p.waiting.drain() {
                let _ = tx.send(Err(CallError::Unavailable(reason.clone())));
            }
        }
        *lock(&conn.stdin) = None;
        conn.reap(Duration::ZERO);
        let closing = conn.closing.load(Ordering::SeqCst);
        {
            let mut inner = lock(&self.inner);
            if inner.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                inner.conn = None;
                inner.status.up = false;
            }
            if !closing {
                inner.status.error = Some(reason.clone());
                if conn.started.elapsed() < QUICK_EXIT {
                    inner.quick_exits += 1;
                    if inner.quick_exits >= STORM_EXITS {
                        inner.blocked_at = Some(Instant::now());
                    }
                } else {
                    inner.quick_exits = 0;
                }
            }
        }
        if !closing && conn.ready.load(Ordering::SeqCst) {
            (self.sink)(&self.name, BackendEvent::Exited(reason));
        }
    }
}

fn notification(method: &str, params: Value) -> Value {
    Value::obj(vec![
        ("jsonrpc", Value::str("2.0")),
        ("method", Value::str(method)),
        ("params", params),
    ])
}

fn clip(s: &str) -> String {
    const MAX: usize = 300;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

fn reader_loop(core: Arc<Core>, conn: Arc<Conn>, stdout: impl Read) {
    let mut r = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim();
        if line.is_empty() {
            continue;
        }
        match mh_json::parse(line) {
            Ok(msg) => core.handle(&conn, msg),
            Err(e) => eprintln!(
                "[{}] skipping an unparseable line from the server ({e}): {}",
                core.name,
                clip(line)
            ),
        }
    }
    let status = conn.reap(Duration::from_secs(2));
    // Let the stderr thread catch the last lines (the reason it exited).
    let until = Instant::now() + Duration::from_secs(1);
    while !conn.stderr_done.load(Ordering::SeqCst) && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    let reason = format!(
        "{}: `{}` exited ({status}){}",
        core.name,
        conn.label,
        conn.tail_text()
    );
    core.end(&conn, reason);
}

fn stderr_loop(
    name: String,
    stderr: impl Read,
    tail: Arc<Mutex<VecDeque<String>>>,
    done: Arc<AtomicBool>,
) {
    let mut r = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
        eprintln!("[{name}] {line}");
        let mut t = lock(&tail);
        if t.len() == STDERR_LINES {
            t.pop_front();
        }
        t.push_back(line);
    }
    done.store(true, Ordering::SeqCst);
}

/// An MCP client for one stdio MCP server.
pub struct StdioBackend {
    core: Arc<Core>,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    project: PathBuf,
    limits: Limits,
    start_lock: Mutex<()>,
}

impl StdioBackend {
    /// `env` is the whole extra environment for the process (normally
    /// `Config::backend_env`, which already holds `spec.env`); `spec.env`
    /// is not added again. Spawns nothing.
    pub fn new(
        spec: &BackendSpec,
        env: Vec<(String, String)>,
        project: &Path,
        sink: EventSink,
    ) -> StdioBackend {
        StdioBackend {
            core: Arc::new(Core {
                name: spec.name.clone(),
                sink,
                next_id: AtomicU64::new(1),
                inner: Mutex::new(Inner {
                    conn: None,
                    status: BackendStatus::default(),
                    quick_exits: 0,
                    blocked_at: None,
                    attempts: 0,
                    last_attempt: None,
                }),
            }),
            command: spec.command.clone(),
            args: spec.args.clone(),
            env,
            project: project.to_path_buf(),
            limits: Limits::default(),
            start_lock: Mutex::new(()),
        }
    }

    /// Replace the time limits (tests use short ones).
    pub fn with_limits(mut self, limits: Limits) -> StdioBackend {
        self.limits = limits;
        self
    }

    fn label(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The running, initialized connection; starts the server if needed.
    fn ensure(&self) -> Result<Arc<Conn>, CallError> {
        let seen = {
            let inner = lock(&self.core.inner);
            if let Some(c) = inner.conn.as_ref().filter(|c| c.live()) {
                return Ok(c.clone());
            }
            inner.attempts
        };
        let _one_starter = lock(&self.start_lock);
        let conn = {
            let mut inner = lock(&self.core.inner);
            if let Some(c) = inner.conn.as_ref().filter(|c| c.live()) {
                return Ok(c.clone());
            }
            // Another thread tried while we waited and failed: share its answer
            // rather than spawning again at once.
            if inner.attempts != seen {
                if let Some(e) = &inner.last_attempt {
                    return Err(e.clone());
                }
            }
            if let Some(at) = inner.blocked_at {
                let until = at + self.limits.restart_block;
                let now = Instant::now();
                if now < until {
                    let last = inner.status.error.clone().unwrap_or_default();
                    return Err(CallError::Unavailable(format!(
                        "{}: exited {STORM_EXITS} times in a row within {} s of starting; not restarting for another {} s. Fix the cause and retry. Last error: {last}",
                        self.core.name,
                        QUICK_EXIT.as_secs(),
                        (until - now).as_secs() + 1,
                    )));
                }
                inner.blocked_at = None;
            }
            inner.attempts += 1;
            match self.spawn() {
                Ok(conn) => {
                    inner.status.starts += 1;
                    inner.conn = Some(conn.clone());
                    conn
                }
                Err(why) => {
                    let e = CallError::Unavailable(why.clone());
                    inner.status.error = Some(why);
                    inner.last_attempt = Some(e.clone());
                    return Err(e);
                }
            }
        };
        match self.handshake(&conn) {
            Ok((server_info, instructions)) => {
                let mut inner = lock(&self.core.inner);
                if conn.ended.load(Ordering::SeqCst) {
                    let why = lock(&conn.pending).dead.clone().unwrap_or_default();
                    let e = CallError::Unavailable(why);
                    inner.last_attempt = Some(e.clone());
                    return Err(e);
                }
                conn.ready.store(true, Ordering::SeqCst);
                inner.status.up = true;
                inner.status.server_info = server_info;
                inner.status.instructions = instructions;
                inner.last_attempt = None;
                Ok(conn)
            }
            Err(e) => {
                let why = match e {
                    CallError::Unavailable(s) => s,
                    other => format!(
                        "{}: initialize failed: {other}{}",
                        self.core.name,
                        conn.tail_text()
                    ),
                };
                self.core.end(&conn, why.clone());
                let e = CallError::Unavailable(why);
                lock(&self.core.inner).last_attempt = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Start the process and its reader threads.
    fn spawn(&self) -> Result<Arc<Conn>, String> {
        let name = &self.core.name;
        let path = mh_core::project::find_command(&self.command).ok_or_else(|| {
            format!(
                "{name}: cannot find `{}` on PATH or ~/.cargo/bin; install it or set `command` in harness.json",
                self.command
            )
        })?;
        let mut child = Command::new(&path)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "{name}: cannot start `{}` in {}: {e}",
                    path.display(),
                    self.project.display()
                )
            })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("stdout was not piped")?;
        let stderr = child.stderr.take().ok_or("stderr was not piped")?;
        let conn = Arc::new(Conn {
            label: self.label(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending: Mutex::new(Pending::default()),
            tail: Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_LINES))),
            stderr_done: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            ready: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        let (n, tail, done) = (name.clone(), conn.tail.clone(), conn.stderr_done.clone());
        let stderr_thread = thread::Builder::new()
            .name(format!("mh-{name}-stderr"))
            .spawn(move || stderr_loop(n, stderr, tail, done));
        let (core, c) = (self.core.clone(), conn.clone());
        let reader_thread = thread::Builder::new()
            .name(format!("mh-{name}-stdout"))
            .spawn(move || reader_loop(core, c, stdout));
        if let Err(e) = stderr_thread.and(reader_thread) {
            // The caller holds the state lock, so no `Core::end` here.
            conn.closing.store(true, Ordering::SeqCst);
            conn.ended.store(true, Ordering::SeqCst);
            *lock(&conn.stdin) = None;
            conn.reap(Duration::ZERO);
            return Err(format!("{name}: cannot start a reader thread: {e}"));
        }
        Ok(conn)
    }

    /// `initialize`, then `notifications/initialized`.
    fn handshake(&self, conn: &Conn) -> Result<(Option<Value>, Option<String>), CallError> {
        let params = Value::obj(vec![
            ("protocolVersion", Value::str(PROTOCOL_VERSION)),
            ("capabilities", Value::obj(vec![])),
            (
                "clientInfo",
                Value::obj(vec![
                    ("name", Value::str("mem-harness")),
                    ("version", Value::str(env!("CARGO_PKG_VERSION"))),
                ]),
            ),
        ]);
        let result = self
            .core
            .request(conn, "initialize", params, self.limits.handshake, None)?;
        conn.send(&notification(
            "notifications/initialized",
            Value::obj(vec![]),
        ))
        .map_err(|e| {
            CallError::Unavailable(format!(
                "{}: cannot send notifications/initialized to `{}`: {e}{}",
                self.core.name,
                conn.label,
                conn.tail_text()
            ))
        })?;
        let server_info = result.get("serverInfo").cloned();
        let instructions = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok((server_info, instructions))
    }
}

impl Backend for StdioBackend {
    fn name(&self) -> &str {
        &self.core.name
    }

    fn start(&self) -> Result<(), CallError> {
        self.ensure().map(|_| ())
    }

    fn status(&self) -> BackendStatus {
        let inner = lock(&self.core.inner);
        let mut st = inner.status.clone();
        st.up = inner.conn.as_ref().is_some_and(|c| c.live());
        st
    }

    fn list_tools(&self) -> Result<Vec<Value>, CallError> {
        let conn = self.ensure()?;
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => Value::obj(vec![("cursor", c.clone())]),
                None => Value::obj(vec![]),
            };
            let page = self
                .core
                .request(&conn, "tools/list", params, self.limits.page, None)?;
            match page.get("tools") {
                Some(Value::Array(items)) => tools.extend(items.iter().cloned()),
                None => {}
                Some(_) => {
                    return Err(CallError::Unavailable(format!(
                        "{}: tools/list reply has a `tools` that is not a list",
                        self.core.name
                    )))
                }
            }
            match page.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::Str(s)) if s.is_empty() => return Ok(tools),
                Some(next) => {
                    if cursor.as_ref() == Some(next) {
                        return Err(CallError::Unavailable(format!(
                            "{}: tools/list returned the same nextCursor {next} twice",
                            self.core.name
                        )));
                    }
                    cursor = Some(next.clone());
                }
            }
        }
        Err(CallError::Unavailable(format!(
            "{}: tools/list has more than {MAX_PAGES} pages",
            self.core.name
        )))
    }

    fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        meta: Option<Value>,
        cancel: &CancelToken,
    ) -> Result<Value, CallError> {
        if cancel.is_cancelled() {
            return Err(CallError::Cancelled);
        }
        let conn = self.ensure()?;
        let mut params = vec![("name", Value::str(tool))];
        if !arguments.is_null() {
            params.push(("arguments", arguments));
        }
        if let Some(meta) = meta {
            params.push(("_meta", meta));
        }
        self.core.request(
            &conn,
            "tools/call",
            Value::obj(params),
            self.limits.call,
            Some(cancel),
        )
    }

    fn shutdown(&self) {
        let conn = {
            let mut inner = lock(&self.core.inner);
            inner.status.up = false;
            inner.conn.take()
        };
        if let Some(conn) = conn {
            conn.closing.store(true, Ordering::SeqCst);
            *lock(&conn.stdin) = None;
            conn.reap(self.limits.shutdown_grace);
            self.core
                .end(&conn, format!("{}: shut down", self.core.name));
        }
    }
}

impl Drop for StdioBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}
