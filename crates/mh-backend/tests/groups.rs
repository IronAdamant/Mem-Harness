//! Process groups, status ordering, cancel during startup and the last
//! reply before an exit (from the second adversarial review), against tiny
//! `/bin/sh` servers. Unix only. Thread and CPU counts are in `threads.rs`.
#![cfg(unix)]

use mh_backend::stdio::{Limits, StdioBackend, MAX_BACKLOG};
use mh_backend::{Backend, BackendEvent, CallError, CancelToken, EventSink};
use mh_core::BackendSpec;
use mh_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

static DIRS: AtomicUsize = AtomicUsize::new(0);

fn dir(tag: &str) -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("mh-groups-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Answers `initialize` with whatever id it carries, reads `initialized`.
const HS: &str = r#"read l; id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"; read l; "#;

fn limits() -> Limits {
    Limits {
        handshake: Duration::from_secs(5),
        page: Duration::from_secs(5),
        call: Duration::from_secs(10),
        restart_block: Duration::ZERO,
        shutdown_grace: Duration::from_millis(200),
    }
}

fn spec_in(d: &Path, script: &str) -> BackendSpec {
    let script = script.replace("$DIR", &d.display().to_string());
    BackendSpec::stdio("sh", "/bin/sh", &["-c", &script])
}

fn sh_with(tag: &str, script: &str, l: Limits, sink: EventSink) -> (StdioBackend, PathBuf) {
    let d = dir(tag);
    let b = StdioBackend::new(&spec_in(&d, script), vec![], &d, sink).with_limits(l);
    (b, d)
}

fn sh(tag: &str, script: &str, l: Limits) -> (StdioBackend, PathBuf) {
    sh_with(tag, script, l, Arc::new(|_, _| {}))
}

fn call(b: &StdioBackend) -> Result<Value, CallError> {
    b.call_tool("x", Value::obj(vec![]), None, &CancelToken::new())
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Wait for `file` and return its trimmed text.
fn read_pid(file: &Path) -> String {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let s = std::fs::read_to_string(file).unwrap_or_default();
        if s.ends_with('\n') {
            return s.trim().to_string();
        }
        assert!(Instant::now() < until, "{} never written", file.display());
        thread::sleep(Duration::from_millis(20));
    }
}

fn gone_within(pid: &str, d: Duration) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if !alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// An old connection that ends late must not overwrite the status error
/// of a newer start that already failed.
#[test]
fn a_late_end_of_an_old_connection_keeps_the_newer_status() {
    let script = format!(
        r#"if [ -e $DIR/ran ]; then echo SECOND-START-FAILED >&2; exit 7; fi; touch $DIR/ran; {HS}read l; sleep 4 >/dev/null & echo FIRST-EXIT >&2; exit 3"#
    );
    let (b, _d) = sh("stale", &script, limits());
    let b = Arc::new(b);
    b.start().unwrap();
    let b2 = b.clone();
    let first = thread::spawn(move || call(&b2));
    thread::sleep(Duration::from_millis(200));
    let seen = match b.start() {
        Err(CallError::Unavailable(e)) => e,
        other => panic!("{other:?}"),
    };
    assert!(seen.contains("SECOND-START-FAILED"), "{seen}");
    let _ = first.join();
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(b.status().error.unwrap_or_default(), seen);
}

/// A call cancelled while the backend is starting returns at once; the
/// start goes on and a later call uses it.
#[test]
fn cancel_is_honoured_while_starting() {
    let l = Limits {
        handshake: Duration::from_secs(3),
        ..limits()
    };
    let (b, _d) = sh("cstart", &format!("sleep 1; {HS}exec sleep 30"), l);
    let b = Arc::new(b);
    let token = CancelToken::new();
    let (b2, t2) = (b.clone(), token.clone());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let t0 = Instant::now();
        let r = b2.call_tool("x", Value::obj(vec![]), None, &t2);
        let _ = tx.send((r, t0.elapsed()));
    });
    thread::sleep(Duration::from_millis(200));
    token.cancel();
    let (r, took) = rx.recv().unwrap();
    assert!(
        took < Duration::from_secs(1) && matches!(r, Err(CallError::Cancelled)),
        "cancelled at 200 ms; got {r:?} after {took:?}"
    );
    // The start was not thrown away.
    b.start().unwrap();
    assert_eq!(b.status().starts, 1);
}

/// A server launched through a wrapper that does not `exec` and ignores
/// stdin EOF is stopped with the wrapper on shutdown.
#[test]
fn shutdown_stops_the_server_behind_a_wrapper() {
    let script = format!("{HS}sh -c 'echo $$ > $DIR/pid; exec sleep 30' ; true");
    let (b, d) = sh("orphan", &script, limits());
    b.start().unwrap();
    let pid = read_pid(&d.join("pid"));
    b.shutdown();
    let survived = !gone_within(&pid, Duration::from_millis(500));
    let _ = std::process::Command::new("kill").arg(&pid).status();
    assert!(!survived, "server pid {pid} survived shutdown");
}

/// When the server exits on its own, what it started goes too, even a
/// process that ignores TERM (KILL after the grace).
#[test]
fn an_exit_stops_the_rest_of_the_group() {
    let script =
        format!("{HS}sh -c 'trap \"\" TERM; echo $$ > $DIR/pid; exec sleep 30' & read l; exit 3");
    let (b, d) = sh("exitgroup", &script, limits());
    b.start().unwrap();
    let pid = read_pid(&d.join("pid"));
    let r = call(&b);
    assert!(
        matches!(&r, Err(CallError::Unavailable(e)) if e.contains("exit status: 3")),
        "{r:?}"
    );
    let survived = !gone_within(&pid, Duration::from_secs(2));
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status();
    assert!(!survived, "pid {pid} outlived the server");
}

/// `keep_children` leaves what the server started running.
#[test]
fn keep_children_leaves_them_running() {
    let d = dir("keep");
    let mut spec = spec_in(
        &d,
        &format!("{HS}sh -c 'echo $$ > $DIR/pid; exec sleep 30' >/dev/null 2>&1 </dev/null & read l; exit 3"),
    );
    spec.keep_children = true;
    let b = StdioBackend::new(&spec, vec![], &d, Arc::new(|_, _| {})).with_limits(limits());
    b.start().unwrap();
    let pid = read_pid(&d.join("pid"));
    b.shutdown();
    thread::sleep(Duration::from_millis(800));
    let lived = alive(&pid);
    let _ = std::process::Command::new("kill").arg(&pid).status();
    assert!(lived, "keep_children: pid {pid} was stopped");
}

/// A big reply written just before the server exits arrives whole, even
/// when reading and parsing it takes longer than the post-exit drain.
#[test]
fn a_big_reply_just_before_exit_arrives() {
    let script = format!(
        r#"{HS}read l; id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{{"jsonrpc":"2.0","id":%s,"result":{{"x":[' "$id"; yes 1, | head -n 10000000 | tr -d '\n'; printf '1]}}}}\n'; exit 0"#
    );
    let (b, _d) = sh("bigexit", &script, limits());
    b.start().unwrap();
    let t0 = Instant::now();
    match call(&b) {
        Ok(v) => {
            let n = v.get("x").and_then(Value::as_array).map_or(0, Vec::len);
            assert_eq!(n, 10_000_001, "after {:?}", t0.elapsed());
        }
        Err(e) => panic!(
            "reply lost after {:?}: {}",
            t0.elapsed(),
            format!("{e:?}").chars().take(200).collect::<String>()
        ),
    }
}

/// The reader runs the event sink: a slow sink must not cost the reply
/// that follows a notification when the server exits right after.
#[test]
fn a_reply_behind_a_slow_sink_arrives() {
    let script = format!(
        r#"{HS}read l; id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{{"jsonrpc":"2.0","method":"notifications/progress","params":{{"progressToken":1,"progress":1}}}}\n{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"; exit 0"#
    );
    let sink: EventSink = Arc::new(|_, e| {
        if matches!(e, BackendEvent::Relay(_)) {
            thread::sleep(Duration::from_millis(1500));
        }
    });
    let (b, _d) = sh_with("slowsink", &script, limits(), sink);
    b.start().unwrap();
    let r = call(&b).unwrap();
    assert_eq!(r.get("ok"), Some(&Value::Bool(true)));
}

/// MCP forbids cancelling `initialize`: a handshake that times out ends the
/// connection without a `notifications/cancelled`.
#[test]
fn initialize_is_never_cancelled() {
    let l = Limits {
        handshake: Duration::from_millis(400),
        ..limits()
    };
    // A background `cat` logs everything written to stdin until it closes;
    // with `keep_children` it outlives the kill of the shell.
    let d = dir("noinitcancel");
    let mut spec = spec_in(
        &d,
        "exec 3<&0; (cat <&3; echo eof) >> $DIR/in & exec sleep 30",
    );
    spec.keep_children = true;
    let b = StdioBackend::new(&spec, vec![], &d, Arc::new(|_, _| {})).with_limits(l);
    let r = b.start();
    assert!(
        matches!(&r, Err(CallError::Unavailable(e)) if e.contains("initialize")),
        "{r:?}"
    );
    let file = d.join("in");
    let until = Instant::now() + Duration::from_secs(3);
    let text = loop {
        let t = std::fs::read_to_string(&file).unwrap_or_default();
        if t.contains("eof") || Instant::now() >= until {
            break t;
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(text.contains("eof"), "{text}");
    assert!(text.contains("\"initialize\""), "{text}");
    assert!(!text.contains("notifications/cancelled"), "{text}");
}

/// A server that stops reading its stdin while calls keep coming is
/// stopped once the queue holds more than `MAX_BACKLOG` bytes, instead of
/// the queue growing without bound.
#[test]
fn a_server_that_stops_reading_is_stopped() {
    let l = Limits {
        call: Duration::from_millis(20),
        ..limits()
    };
    // The call that trips the limit may time out before the connection has
    // ended (and the next call starts a new one), so watch the exit events.
    let exits = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let ex = exits.clone();
    let sink: EventSink = Arc::new(move |_, e| {
        if let BackendEvent::Exited(why) = e {
            ex.lock().unwrap().push(why);
        }
    });
    let (b, _d) = sh_with("backlog", &format!("{HS}exec sleep 30"), l, sink);
    b.start().unwrap();
    let arg = Value::obj(vec![("blob", Value::str("a".repeat(1 << 20)))]);
    let stopped = || {
        exits
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("stopped reading its stdin"))
    };
    for _ in 0..(MAX_BACKLOG >> 20) + 8 {
        match b.call_tool("x", arg.clone(), None, &CancelToken::new()) {
            Err(CallError::Timeout(_)) | Err(CallError::Unavailable(_)) => {}
            other => panic!("{other:?}"),
        }
        if stopped() {
            break;
        }
    }
    let until = Instant::now() + Duration::from_secs(3);
    while !stopped() && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(stopped(), "{:?}", exits.lock().unwrap());
}
