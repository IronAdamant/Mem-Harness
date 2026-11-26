//! The stdio MCP client: one suite tool's MCP server run as a child
//! process, spoken to with newline-delimited JSON-RPC 2.0 on its stdin and
//! stdout (one message per line).
//!
//! Nothing is spawned until the first `start`, `list_tools` or `call_tool`.
//! A server that exits is restarted on the next use, unless it keeps failing
//! right after starting (see [`Limits::restart_block`]).
//!
//! # Threads per running server
//!
//! - **writer**: owns the child's stdin and writes the messages queued on
//!   an unbounded channel. Nobody else touches the pipe, so a server that
//!   stops reading can stall only this thread: callers enqueue and go on
//!   watching their deadline and cancel token, and shutdown never waits on
//!   the pipe (it kills the child after the grace period, which breaks the
//!   pipe and frees the writer).
//! - **reader**: reads stdout and routes replies to the waiting caller by
//!   id; answers server requests (ping) through the writer queue.
//! - **stderr**: keeps the last stderr lines and echoes them to our stderr
//!   prefixed `[<name>] `.
//! - **waiter**: polls `try_wait` and is the one that notices the exit and
//!   calls [`Core::end`], whether or not stdout reached EOF (a grandchild
//!   may hold it open). It also kills a child that closed stdout (or whose
//!   stdin broke) but lingers for more than [`LINGER`].
//!
//! # Connection states
//!
//! `closed` is set the moment anything shows the connection is unusable
//! (stdout EOF, exit, a write error, an over-long line, shutdown); from then
//! on `ensure` no longer hands it out and starts a fresh process. `ended` is
//! set by `Core::end`, which runs once per connection: it records the reason
//! in the status first and only then fails the waiting calls with that same
//! reason, so a caller's error always matches the status. A call that got
//! the connection just before `closed` was set is failed by `end` too.
//!
//! # Lock order
//!
//! `StdioBackend::start_lock` → `Core::inner` → at most one `Conn` lock.
//! Every `Conn` lock (`pending`, `out`, `child`, `tail`, `linger_from`,
//! `trouble`) is a leaf: it is held only for a short, non-blocking step and
//! never while taking another lock. No lock is ever held across a pipe
//! read or write.

use crate::{Backend, BackendEvent, BackendStatus, CallError, CancelToken, EventSink};
use mh_core::config::BASE_ENV;
use mh_core::BackendSpec;
use mh_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// The MCP protocol version sent in `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The longest stdout line (one JSON-RPC message) accepted from a server.
/// A longer one ends the connection.
pub const MAX_LINE: usize = 32 << 20;
/// How many stderr lines are kept for error messages.
const STDERR_LINES: usize = 20;
/// Each kept (and echoed) stderr line is cut to this many bytes.
const STDERR_LINE_MAX: usize = 4096;
/// An exit this soon after start counts towards the restart-storm guard.
const QUICK_EXIT: Duration = Duration::from_secs(5);
/// This many quick exits (or failed handshakes) in a row block restarts
/// for `restart_block`.
const STORM_EXITS: u32 = 3;
/// How often a waiting call checks its cancel token and deadline.
const POLL: Duration = Duration::from_millis(150);
/// How often the waiter thread checks whether the child has exited.
const WAIT_POLL: Duration = Duration::from_millis(25);
/// A child that closed stdout (or whose stdin broke) is killed if it is
/// still running this long after.
const LINGER: Duration = Duration::from_secs(2);
/// After the exit, how long to wait for the last stdout and stderr lines.
const DRAIN: Duration = Duration::from_secs(1);
/// The most `tools/list` pages followed before giving up.
const MAX_PAGES: usize = 1000;

/// Time limits. The defaults suit the suite tools; [`StdioBackend::new`]
/// takes `handshake`, `page` and `call` from the spec, and tests shorten
/// them with [`StdioBackend::with_limits`].
#[derive(Clone, Debug)]
pub struct Limits {
    /// Wait for the `initialize` reply.
    pub handshake: Duration,
    /// Wait for each `tools/list` page.
    pub page: Duration,
    /// Overall cap on one `tools/call` (some suite calls block for minutes).
    pub call: Duration,
    /// After [`STORM_EXITS`] failed starts or exits in a row, each within
    /// 5 s of starting, refuse to restart for this long.
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

impl Limits {
    /// The defaults, with the spec's `handshake_s` (handshake and each
    /// `tools/list` page) and `call_s`.
    pub fn from_spec(spec: &BackendSpec) -> Limits {
        Limits {
            handshake: Duration::from_secs(spec.handshake_s),
            page: Duration::from_secs(spec.handshake_s),
            call: Duration::from_secs(spec.call_s),
            ..Limits::default()
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
    /// The writer thread's queue; `None` once stdin is being closed.
    /// `Sender::send` never blocks, so this is held only for a push.
    out: Mutex<Option<Sender<Vec<u8>>>>,
    child: Mutex<Child>,
    pending: Mutex<Pending>,
    tail: Mutex<VecDeque<String>>,
    stdout_done: AtomicBool,
    stderr_done: AtomicBool,
    started: Instant,
    /// The handshake finished.
    ready: AtomicBool,
    /// Unusable: no new calls are routed here (see the module docs).
    closed: AtomicBool,
    /// When stdout closed or stdin broke; the waiter kills the child if it
    /// is still running `LINGER` after this.
    linger_from: Mutex<Option<Instant>>,
    /// We killed it.
    killed: AtomicBool,
    /// What went wrong besides the exit itself (write error, over-long
    /// line); the first one is put in the exit reason.
    trouble: Mutex<Option<String>>,
    /// We are stopping it on purpose (shutdown): no storm count, no event.
    closing: AtomicBool,
    /// `end` has run.
    ended: AtomicBool,
}

impl Conn {
    /// Queue one message as one line for the writer thread. Never blocks.
    /// A message queued after stdin closed is dropped: the connection is
    /// ending, and `end` fails whoever waits for a reply.
    fn send(&self, msg: &Value) {
        let mut line = msg.to_json().into_bytes();
        line.push(b'\n');
        if let Some(tx) = lock(&self.out).as_ref() {
            let _ = tx.send(line);
        }
    }

    /// Stop writing: the writer thread drops stdin once it is past any
    /// write in progress (which a kill ends).
    fn close_stdin(&self) {
        self.closed.store(true, Ordering::SeqCst);
        *lock(&self.out) = None;
    }

    fn note_trouble(&self, what: String) {
        lock(&self.trouble).get_or_insert(what);
    }

    /// Mark unusable and let the waiter kill it if it does not exit soon.
    fn linger(&self) {
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.linger_from).get_or_insert_with(Instant::now);
    }

    /// Kill the child unless it has already exited. Does not wait.
    fn kill(&self) {
        let mut child = lock(&self.child);
        if let Ok(None) = child.try_wait() {
            self.killed.store(true, Ordering::SeqCst);
            let _ = child.kill();
        }
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

    /// Wait up to `grace` for the process to exit, then kill it. Holds the
    /// child lock only for each `try_wait` (and the kill).
    fn reap(&self, grace: Duration) -> String {
        let deadline = Instant::now() + grace;
        loop {
            {
                let mut child = lock(&self.child);
                match child.try_wait() {
                    Ok(Some(st)) => return st.to_string(),
                    Ok(None) if Instant::now() >= deadline => {
                        self.killed.store(true, Ordering::SeqCst);
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
        self.ready.load(Ordering::SeqCst)
            && !self.closed.load(Ordering::SeqCst)
            && !self.ended.load(Ordering::SeqCst)
    }
}

struct Inner {
    conn: Option<Arc<Conn>>,
    status: BackendStatus,
    /// Failed starts and exits in a row that came within `QUICK_EXIT` of
    /// their start.
    quick_exits: u32,
    /// When the storm guard tripped.
    blocked_at: Option<Instant>,
    /// Start attempts so far, and the error of the last one (None if it worked).
    attempts: u64,
    last_attempt: Option<CallError>,
}

/// State shared with the connection threads.
struct Core {
    name: String,
    sink: EventSink,
    next_id: AtomicU64,
    inner: Mutex<Inner>,
}

impl Core {
    /// Send a request and wait for its reply. Never blocks on the pipe: the
    /// message is queued, and the deadline and cancel token are checked
    /// every `POLL` from the start.
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
            // `end` sets `dead` and drains `waiting` under this lock, so
            // either we see `dead` here or `end` fails our entry.
            let mut p = lock(&conn.pending);
            if let Some(why) = &p.dead {
                return Err(CallError::Unavailable(why.clone()));
            }
            p.waiting.insert(id, tx);
        }
        conn.send(&Value::obj(vec![
            ("jsonrpc", Value::str("2.0")),
            ("id", Value::int(id as i64)),
            ("method", Value::str(method)),
            ("params", params),
        ]));
        let deadline = Instant::now() + limit;
        loop {
            let wait = POLL.min(deadline.saturating_duration_since(Instant::now()));
            match rx.recv_timeout(wait) {
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

    /// Stop waiting for `id` and tell the server (queued, never blocks).
    fn abandon(&self, conn: &Conn, id: u64, reason: &str) {
        lock(&conn.pending).waiting.remove(&id);
        conn.send(&notification(
            "notifications/cancelled",
            Value::obj(vec![
                ("requestId", Value::int(id as i64)),
                ("reason", Value::str(reason)),
            ]),
        ));
    }

    /// Route one message from the server. Runs on the reader thread; any
    /// answer goes through the writer queue, so it never blocks.
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
                conn.send(&reply);
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

    /// The connection is over: stop it, update the status and the storm
    /// guard, then fail every waiting call with `reason` and report the
    /// exit. Runs once per connection; takes no lock while holding another
    /// and never waits on the process or its pipes.
    fn end(&self, conn: &Arc<Conn>, reason: String) {
        conn.closed.store(true, Ordering::SeqCst);
        if conn.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        // New calls fail from here on. Waiting calls are failed only after
        // the status says why, so a caller that sees the error and then
        // reads the status finds the same reason there.
        let waiting: Vec<_> = {
            let mut p = lock(&conn.pending);
            p.dead = Some(reason.clone());
            p.waiting.drain().map(|(_, tx)| tx).collect()
        };
        conn.close_stdin();
        // The waiter thread reaps it.
        conn.kill();
        let closing = conn.closing.load(Ordering::SeqCst);
        {
            let mut inner = lock(&self.inner);
            let current = inner.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn));
            if current {
                inner.conn = None;
                inner.status.up = false;
            }
            if !closing {
                // An old connection ending late must not overwrite the
                // status of a newer one that is running.
                if current || inner.conn.is_none() {
                    inner.status.error = Some(reason.clone());
                }
                // A start that never finished its handshake counts however
                // long it took (a server that hangs at startup).
                if !conn.ready.load(Ordering::SeqCst) || conn.started.elapsed() < QUICK_EXIT {
                    inner.quick_exits += 1;
                    if inner.quick_exits >= STORM_EXITS {
                        inner.blocked_at = Some(Instant::now());
                    }
                } else {
                    inner.quick_exits = 0;
                }
            }
        }
        for tx in waiting {
            let _ = tx.send(Err(CallError::Unavailable(reason.clone())));
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

/// Read one line (with its `\n`, if any) into `buf`, keeping at most `keep`
/// bytes. `Ok(None)` at EOF with nothing read; `Ok(Some(true))` when the
/// line was longer than `keep`. With `drain`, the rest of an over-long line
/// is read and dropped; without it, reading stops at the limit.
fn read_line_capped(
    r: &mut impl BufRead,
    buf: &mut Vec<u8>,
    keep: usize,
    drain: bool,
) -> io::Result<Option<bool>> {
    buf.clear();
    let mut any = false;
    let mut over = false;
    loop {
        let (used, done) = {
            let avail = match r.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if avail.is_empty() {
                return Ok(any.then_some(over));
            }
            any = true;
            let (chunk, done) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (&avail[..=i], true),
                None => (avail, false),
            };
            let room = keep.saturating_sub(buf.len());
            if chunk.len() > room {
                over = true;
                buf.extend_from_slice(&chunk[..room]);
            } else {
                buf.extend_from_slice(chunk);
            }
            (chunk.len(), done)
        };
        r.consume(used);
        if done || (over && !drain) {
            return Ok(Some(over));
        }
    }
}

fn writer_loop(conn: Arc<Conn>, mut stdin: ChildStdin, rx: Receiver<Vec<u8>>) {
    for line in rx {
        if conn.closed.load(Ordering::SeqCst) {
            break;
        }
        if let Err(e) = stdin.write_all(&line).and_then(|()| stdin.flush()) {
            conn.note_trouble(format!("could not be written to ({e})"));
            conn.linger();
            break;
        }
    }
    // Dropping stdin here closes the pipe.
}

fn reader_loop(core: Arc<Core>, conn: Arc<Conn>, stdout: impl Read) {
    let mut r = BufReader::with_capacity(64 * 1024, stdout);
    let mut buf = Vec::new();
    loop {
        match read_line_capped(&mut r, &mut buf, MAX_LINE, false) {
            Ok(None) | Err(_) => break,
            Ok(Some(false)) => {}
            Ok(Some(true)) => {
                let what = format!("sent a stdout line longer than {} MiB", MAX_LINE >> 20);
                eprintln!("[{}] `{}` {what}; stopping it", core.name, conn.label);
                conn.note_trouble(what);
                conn.closed.store(true, Ordering::SeqCst);
                conn.kill();
                break;
            }
        }
        if conn.ended.load(Ordering::SeqCst) {
            break;
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
    conn.stdout_done.store(true, Ordering::SeqCst);
    conn.linger();
}

fn stderr_loop(name: String, conn: Arc<Conn>, stderr: impl Read) {
    let mut r = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        let over = match read_line_capped(&mut r, &mut buf, STDERR_LINE_MAX, true) {
            Ok(None) | Err(_) => break,
            Ok(Some(over)) => over,
        };
        let mut line = String::from_utf8_lossy(&buf).trim_end().to_string();
        if over {
            line.push_str(" [line cut]");
        }
        eprintln!("[{name}] {line}");
        let mut t = lock(&conn.tail);
        if t.len() == STDERR_LINES {
            t.pop_front();
        }
        t.push_back(line);
    }
    conn.stderr_done.store(true, Ordering::SeqCst);
}

/// Notices the exit (independently of stdout EOF) and ends the connection.
fn waiter_loop(core: Arc<Core>, conn: Arc<Conn>) {
    let status = loop {
        {
            let mut child = lock(&conn.child);
            match child.try_wait() {
                Ok(Some(st)) => break st.to_string(),
                Ok(None) => {}
                Err(e) => break format!("exit status unknown: {e}"),
            }
        }
        let lingered = lock(&conn.linger_from).is_some_and(|at| at.elapsed() >= LINGER);
        if lingered {
            conn.kill();
        }
        thread::sleep(WAIT_POLL);
    };
    conn.closed.store(true, Ordering::SeqCst);
    if conn.ended.load(Ordering::SeqCst) {
        return;
    }
    // Let the reader route the last replies and the stderr thread catch the
    // last lines (the reason it exited). A grandchild holding a pipe open
    // only delays this by `DRAIN`.
    let until = Instant::now() + DRAIN;
    while !(conn.stdout_done.load(Ordering::SeqCst) && conn.stderr_done.load(Ordering::SeqCst))
        && Instant::now() < until
    {
        thread::sleep(Duration::from_millis(10));
    }
    let killed = if conn.killed.load(Ordering::SeqCst) {
        ", killed"
    } else {
        ""
    };
    let trouble = lock(&conn.trouble).clone();
    let reason = match trouble {
        Some(t) => format!(
            "{}: `{}` {t}; it exited ({status}{killed}){}",
            core.name,
            conn.label,
            conn.tail_text()
        ),
        None => format!(
            "{}: `{}` exited ({status}{killed}){}",
            core.name,
            conn.label,
            conn.tail_text()
        ),
    };
    core.end(&conn, reason);
}

/// An MCP client for one stdio MCP server.
pub struct StdioBackend {
    core: Arc<Core>,
    command: String,
    args: Vec<String>,
    /// `spec.env`, applied before `env`.
    spec_env: Vec<(String, String)>,
    env: Vec<(String, String)>,
    /// `spec.env_pass`: when set, the child's environment starts empty.
    env_pass: Option<Vec<String>>,
    project: PathBuf,
    limits: Limits,
    start_lock: Mutex<()>,
}

impl StdioBackend {
    /// `env` is the extra environment for the process (normally
    /// `Config::backend_env`: `spec.env` plus the computed disable variable),
    /// applied after `spec.env`. With `spec.env_pass` set, the child gets
    /// only [`BASE_ENV`] and the listed variables from our environment, then
    /// `spec.env` and `env`; otherwise it inherits ours. Time limits come
    /// from `spec.handshake_s` and `spec.call_s` (see [`Limits::from_spec`]).
    /// Spawns nothing.
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
            spec_env: spec.env.clone(),
            env,
            env_pass: spec.env_pass.clone(),
            project: project.to_path_buf(),
            limits: Limits::from_spec(spec),
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
                        "{}: failed {STORM_EXITS} times in a row within {} s of starting (or never finished starting); not restarting for another {} s. Fix the cause and retry. Last error: {last}",
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
                // If it is already `closed` (exited right after the
                // handshake), calls on it wait for `end` and get its reason.
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
                self.core.end(&conn, why);
                // `end` is once-only; report what it recorded.
                let why = lock(&conn.pending).dead.clone().unwrap_or_default();
                let e = CallError::Unavailable(why);
                lock(&self.core.inner).last_attempt = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Start the process and its four threads.
    fn spawn(&self) -> Result<Arc<Conn>, String> {
        let name = &self.core.name;
        let path = mh_core::project::find_command(&self.command, &self.project).ok_or_else(|| {
            format!(
                "{name}: cannot find `{}` on PATH or ~/.cargo/bin; install it or set `command` in harness.json",
                self.command
            )
        })?;
        let mut cmd = Command::new(&path);
        cmd.args(&self.args);
        if let Some(pass) = &self.env_pass {
            cmd.env_clear();
            for k in BASE_ENV
                .iter()
                .copied()
                .chain(pass.iter().map(String::as_str))
            {
                if let Some(v) = std::env::var_os(k) {
                    cmd.env(k, v);
                }
            }
        }
        let mut child = cmd
            .envs(self.spec_env.iter().map(|(k, v)| (k, v)))
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
        let stdin = child.stdin.take().ok_or("stdin was not piped")?;
        let stdout = child.stdout.take().ok_or("stdout was not piped")?;
        let stderr = child.stderr.take().ok_or("stderr was not piped")?;
        let (tx, rx) = mpsc::channel();
        let conn = Arc::new(Conn {
            label: self.label(),
            out: Mutex::new(Some(tx)),
            child: Mutex::new(child),
            pending: Mutex::new(Pending::default()),
            tail: Mutex::new(VecDeque::with_capacity(STDERR_LINES)),
            stdout_done: AtomicBool::new(false),
            stderr_done: AtomicBool::new(false),
            started: Instant::now(),
            ready: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            linger_from: Mutex::new(None),
            killed: AtomicBool::new(false),
            trouble: Mutex::new(None),
            closing: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        let thread = |role: &str| thread::Builder::new().name(format!("mh-{name}-{role}"));
        let c = conn.clone();
        let writer = thread("stdin").spawn(move || writer_loop(c, stdin, rx));
        let (n, c) = (name.clone(), conn.clone());
        let errs = thread("stderr").spawn(move || stderr_loop(n, c, stderr));
        let (core, c) = (self.core.clone(), conn.clone());
        let reader = thread("stdout").spawn(move || reader_loop(core, c, stdout));
        // The waiter goes last: it is the one that calls `end`, and the
        // caller holds the state lock, so if any thread failed to start the
        // connection is torn down here without `Core::end`.
        let started = writer.and(errs).and(reader).and_then(|_| {
            let (core, c) = (self.core.clone(), conn.clone());
            thread("wait").spawn(move || waiter_loop(core, c))
        });
        if let Err(e) = started {
            conn.closing.store(true, Ordering::SeqCst);
            conn.ended.store(true, Ordering::SeqCst);
            conn.close_stdin();
            conn.reap(Duration::ZERO);
            return Err(format!("{name}: cannot start a connection thread: {e}"));
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
        ));
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

    /// Never waits on the pipes: stdin is closed as soon as the writer is
    /// free, and the child is killed after `shutdown_grace` either way.
    fn shutdown(&self) {
        let conn = {
            let mut inner = lock(&self.core.inner);
            inner.status.up = false;
            inner.conn.take()
        };
        if let Some(conn) = conn {
            conn.closing.store(true, Ordering::SeqCst);
            conn.close_stdin();
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
