//! Regression tests for pipe and process edge cases (from an adversarial
//! review), against tiny `/bin/sh` servers. Unix only.
#![cfg(unix)]

use mh_backend::stdio::{Limits, StdioBackend};
use mh_backend::{Backend, CallError, CancelToken, EventSink};
use mh_core::BackendSpec;
use mh_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// Reply to `initialize` with the id it carries (a restarted server gets a
/// new one) and read `notifications/initialized`.
const HANDSHAKE: &str = r#"read l; id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"; read l; "#;

fn sh(tag: &str, script: &str, limits: Limits) -> StdioBackend {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("mh-pipes-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!("{HANDSHAKE}{script}");
    let spec = BackendSpec::stdio("sh", "/bin/sh", &["-c", &script]);
    let sink: EventSink = Arc::new(|_, _| {});
    StdioBackend::new(&spec, vec![], &dir, sink).with_limits(limits)
}

fn limits(call_ms: u64) -> Limits {
    Limits {
        handshake: Duration::from_secs(5),
        page: Duration::from_secs(5),
        call: Duration::from_millis(call_ms),
        restart_block: Duration::from_secs(30),
        shutdown_grace: Duration::from_millis(200),
    }
}

fn big_args(bytes: usize) -> Value {
    Value::obj(vec![("blob", Value::str("a".repeat(bytes)))])
}

/// A server that stops reading stdin: a call with more arguments than the
/// pipe holds must still honour its cancel token and limit.
#[test]
fn cancel_and_limit_fire_while_the_pipe_is_full() {
    let b = Arc::new(sh("wblock", "exec sleep 8", limits(300)));
    b.start().unwrap();
    let token = CancelToken::new();
    let (tx, rx) = mpsc::channel();
    let (b2, t2) = (b.clone(), token.clone());
    thread::spawn(move || {
        let t0 = Instant::now();
        let r = b2.call_tool("x", big_args(1 << 20), None, &t2);
        let _ = tx.send((r, t0.elapsed()));
    });
    thread::sleep(Duration::from_millis(200));
    token.cancel();
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok((r, took)) => assert!(
            matches!(r, Err(CallError::Cancelled) | Err(CallError::Timeout(_))),
            "{r:?} after {took:?}"
        ),
        Err(_) => panic!("call with a 300 ms limit, cancelled at 200 ms, still blocked after 3 s"),
    }
    // A second call (and its cancel notice) queue behind; the limit fires.
    let t0 = Instant::now();
    let r = b.call_tool("y", big_args(1 << 20), None, &CancelToken::new());
    assert!(matches!(r, Err(CallError::Timeout(_))), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(2));
}

/// Same server: shutdown must not wait behind the stalled writer.
#[test]
fn shutdown_does_not_wait_on_a_stalled_writer() {
    let b = Arc::new(sh("sdblock", "exec sleep 8", limits(60_000)));
    b.start().unwrap();
    let b2 = b.clone();
    let call =
        thread::spawn(move || b2.call_tool("x", big_args(1 << 20), None, &CancelToken::new()));
    thread::sleep(Duration::from_millis(300));
    let (tx, rx) = mpsc::channel();
    let b3 = b.clone();
    thread::spawn(move || {
        let t0 = Instant::now();
        b3.shutdown();
        let _ = tx.send(t0.elapsed());
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(took) => assert!(took < Duration::from_secs(2), "shutdown took {took:?}"),
        Err(_) => panic!("shutdown (grace 200 ms) still blocked after 3 s"),
    }
    let r = call.join().unwrap();
    assert!(matches!(r, Err(CallError::Unavailable(_))), "{r:?}");
}

/// The server exits while a process it started keeps its stdout open:
/// the exit is noticed anyway.
#[test]
fn exit_is_noticed_while_a_grandchild_holds_stdout() {
    let b = sh("grand", "read l; sleep 6 & exit 3", limits(4_000));
    b.start().unwrap();
    let t0 = Instant::now();
    let r = b.call_tool("x", Value::obj(vec![]), None, &CancelToken::new());
    let took = t0.elapsed();
    let st = b.status();
    match &r {
        Err(CallError::Unavailable(e)) => {
            assert!(e.contains("exit status: 3"), "{e}");
            assert_eq!(st.error.as_deref(), Some(e.as_str()));
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert!(!st.up);
}

/// Between stdout EOF and the end of the connection, a new call must not
/// get the dead connection's raw write error: it restarts the server or
/// fails with the reason the status records.
#[test]
fn a_call_in_the_exit_window_matches_the_status() {
    // The grandchild keeps stderr (not stdout) open, so stdout ends at once
    // but the stderr wait runs its full time.
    let b = Arc::new(sh(
        "window",
        "read l; sleep 4 >/dev/null & exit 3",
        limits(10_000),
    ));
    b.start().unwrap();
    let b2 = b.clone();
    let first =
        thread::spawn(move || b2.call_tool("x", Value::obj(vec![]), None, &CancelToken::new()));
    thread::sleep(Duration::from_millis(300));
    let r = b.call_tool("y", Value::obj(vec![]), None, &CancelToken::new());
    let st = b.status();
    let _ = first.join();
    match &r {
        Err(CallError::Unavailable(e)) => {
            assert_eq!(
                st.error.as_deref(),
                Some(e.as_str()),
                "caller saw {e:?}, status (up = {}) says {:?}",
                st.up,
                st.error
            );
            // The restarted server finished its handshake, got the call and
            // exited on it: not a handshake timeout.
            assert!(!e.contains("initialize"), "{e}");
            assert!(e.contains("exit status: 3"), "{e}");
        }
        other => panic!("expected the dead server to be restarted or reported, got {other:?}"),
    }
    assert_eq!(st.starts, 2, "the second call restarted the server");
}

/// A stdout line over the cap ends the connection with a clear reason
/// instead of being buffered whole.
#[test]
fn an_over_long_stdout_line_ends_the_connection() {
    let script = r#"read l; printf '{"jsonrpc":"2.0","id":2,"result":{"x":"'; head -c 67108864 /dev/zero | tr '\0' a; printf '"}}\n'; exec sleep 5"#;
    let b = sh("bigline", script, limits(20_000));
    let t0 = Instant::now();
    let r = b.call_tool("x", Value::obj(vec![]), None, &CancelToken::new());
    match &r {
        Err(CallError::Unavailable(e)) => {
            assert!(e.contains("longer than 32 MiB"), "{e}");
            assert_eq!(b.status().error.as_deref(), Some(e.as_str()));
        }
        other => panic!("expected Unavailable, got {}", clip(&format!("{other:?}"))),
    }
    assert!(t0.elapsed() < Duration::from_secs(10));
}

/// A long stderr line is kept cut, not whole.
#[test]
fn long_stderr_lines_are_cut() {
    let script = r#"head -c 1000000 /dev/zero | tr '\0' e >&2; echo >&2; exit 4"#;
    let b = sh("bigerr", script, limits(5_000));
    let e = match b.start() {
        // The handshake may finish before the exit.
        Ok(()) => match b.call_tool("x", Value::obj(vec![]), None, &CancelToken::new()) {
            Err(CallError::Unavailable(e)) => e,
            other => panic!("{other:?}"),
        },
        Err(CallError::Unavailable(e)) => e,
        Err(other) => panic!("{other:?}"),
    };
    assert!(e.contains("[line cut]"), "{}", clip(&e));
    assert!(e.len() < 8192, "reason is {} bytes", e.len());
}

/// A server that sends a ping while its stdin is full does not wedge the
/// reader: replies to other calls still arrive.
#[test]
fn ping_replies_do_not_block_the_reader() {
    // Never reads again; pings us 2000 times (each answer is queued), then
    // answers request 2.
    let script = r#"i=0; while [ $i -lt 2000 ]; do echo '{"jsonrpc":"2.0","id":"p'$i'","method":"ping"}'; i=$((i+1)); done; echo '{"jsonrpc":"2.0","id":2,"result":{"ok":true}}'; exec sleep 5"#;
    let b = sh("ping", script, limits(5_000));
    let r = b
        .call_tool("x", big_args(1 << 20), None, &CancelToken::new())
        .unwrap();
    assert_eq!(r.get("ok"), Some(&Value::Bool(true)));
}

fn clip(s: &str) -> String {
    s.chars().take(300).collect()
}
