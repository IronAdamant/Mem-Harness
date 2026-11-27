//! StdioBackend against the fake MCP server in `src/bin/mh-fake-mcp.rs`.

use mh_backend::stdio::{Limits, StdioBackend};
use mh_backend::{Backend, BackendEvent, CallError, CancelToken, EventSink};
use mh_core::BackendSpec;
use mh_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FAKE: &str = env!("CARGO_BIN_EXE_mh-fake-mcp");

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A fresh directory per test (pid + counter: parallel runs never share one).
fn temp_dir(tag: &str) -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("mh-backend-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn spec(command: &str) -> BackendSpec {
    BackendSpec::stdio("fake", command, &["serve"])
}

type Events = Arc<Mutex<Vec<(String, BackendEvent)>>>;

struct Fixture {
    backend: StdioBackend,
    events: Events,
    log: PathBuf,
    _dir: PathBuf,
}

fn fixture(tag: &str, env: &[(&str, &str)], limits: Limits) -> Fixture {
    let dir = temp_dir(tag);
    let log = dir.join("fake.log");
    let mut env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.push(("FAKE_LOG".into(), log.display().to_string()));
    let events: Events = Arc::default();
    let ev = events.clone();
    let sink: EventSink = Arc::new(move |name, e| ev.lock().unwrap().push((name.to_string(), e)));
    let backend = StdioBackend::new(&spec(FAKE), env, &dir, sink).with_limits(limits);
    Fixture {
        backend,
        events,
        log,
        _dir: dir,
    }
}

fn quick() -> Limits {
    Limits {
        handshake: Duration::from_secs(10),
        page: Duration::from_secs(10),
        ..Limits::default()
    }
}

fn logged(f: &Fixture) -> Vec<Value> {
    std::fs::read_to_string(&f.log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| mh_json::parse(l).ok())
        .collect()
}

fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn first_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|t| t.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn unavailable(e: CallError) -> String {
    match e {
        CallError::Unavailable(s) => s,
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn handshake_fills_status_and_start_is_idempotent() {
    let f = fixture("hs", &[("FAKE_GARBAGE", "1")], quick());
    let st = f.backend.status();
    assert!(!st.up);
    assert_eq!(st.starts, 0, "new() must not spawn");

    // Many threads start at once: one process.
    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| f.backend.start().unwrap());
        }
    });
    f.backend.start().unwrap();
    let st = f.backend.status();
    assert!(st.up);
    assert_eq!(st.starts, 1);
    assert_eq!(st.error, None);
    assert_eq!(st.instructions.as_deref(), Some("fake instructions"));
    let info = st.server_info.unwrap();
    assert_eq!(info.get("name").and_then(Value::as_str), Some("fake-mcp"));
    assert_eq!(info.get("version").and_then(Value::as_str), Some("9.9.9"));
    assert_eq!(f.backend.name(), "fake");

    let log = logged(&f);
    let init = log
        .iter()
        .find(|m| m.get("method").and_then(Value::as_str) == Some("initialize"))
        .unwrap();
    let p = init.get("params").unwrap();
    assert_eq!(
        p.get("protocolVersion").and_then(Value::as_str),
        Some("2025-06-18")
    );
    assert_eq!(p.get("capabilities"), Some(&Value::obj(vec![])));
    assert_eq!(
        p.get("clientInfo")
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str),
        Some("mem-harness")
    );
    // `initialized` is queued, not waited for: the fake logs it a moment later.
    assert!(wait_for(|| logged(&f)
        .iter()
        .any(
            |m| m.get("method").and_then(Value::as_str) == Some("notifications/initialized")
        )));
    assert_eq!(
        log.iter()
            .filter(|m| m.get("method").and_then(Value::as_str) == Some("initialize"))
            .count(),
        1
    );
}

#[test]
fn list_tools_follows_every_page() {
    let f = fixture("pages", &[("FAKE_PAGES", "3")], quick());
    let tools = f.backend.list_tools().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["t0a", "t0b", "t1a", "t1b", "t2a", "t2b"]);
    let cursors: Vec<String> = logged(&f)
        .iter()
        .filter(|m| m.get("method").and_then(Value::as_str) == Some("tools/list"))
        .map(|m| {
            m.get("params")
                .and_then(|p| p.get("cursor"))
                .map(|c| c.to_json())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(cursors, ["", "\"p1\"", "\"p2\""]);
}

#[test]
fn concurrent_calls_get_their_own_replies() {
    let f = fixture("conc", &[], quick());
    let tools = f.backend.list_tools().unwrap();
    assert_eq!(tools.len(), 6);
    thread::scope(|s| {
        for i in 0..12i64 {
            let b = &f.backend;
            s.spawn(move || {
                // Later calls answer first.
                let args = Value::obj(vec![
                    ("n", Value::int(i)),
                    ("delay_ms", Value::int((12 - i) * 15)),
                ]);
                let r = b
                    .call_tool("echo", args, None, &CancelToken::new())
                    .unwrap();
                assert_eq!(
                    r.get("arguments")
                        .and_then(|a| a.get("n"))
                        .and_then(Value::as_i64),
                    Some(i)
                );
            });
        }
    });
    // Ids are ours, unique, and never reused.
    let ids: Vec<String> = logged(&f)
        .iter()
        .filter_map(|m| m.get("id").map(|i| i.to_json()))
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len());
}

#[test]
fn meta_and_arguments_pass_through() {
    let f = fixture("meta", &[], quick());
    let meta = Value::obj(vec![("progressToken", Value::Num("1.50".into()))]);
    let args = Value::obj(vec![("x", Value::Num("1e3".into()))]);
    let r = f
        .backend
        .call_tool(
            "echo",
            args.clone(),
            Some(meta.clone()),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(r.get("meta"), Some(&meta));
    assert_eq!(r.get("arguments"), Some(&args));
    assert_eq!(first_text(&r), "{\"x\":1e3}");
}

#[test]
fn cancel_tells_the_server_and_returns_cancelled() {
    let f = fixture("cancel", &[], quick());
    f.backend.start().unwrap();
    let token = CancelToken::new();
    let t2 = token.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        t2.cancel();
    });
    let t0 = Instant::now();
    let r = f.backend.call_tool(
        "slow",
        Value::obj(vec![("ms", Value::int(20_000))]),
        None,
        &token,
    );
    canceller.join().unwrap();
    assert_eq!(r, Err(CallError::Cancelled));
    assert!(t0.elapsed() < Duration::from_secs(3));

    let call_id = logged(&f)
        .iter()
        .find(|m| m.get("method").and_then(Value::as_str) == Some("tools/call"))
        .and_then(|m| m.get("id").cloned())
        .unwrap();
    assert!(wait_for(|| logged(&f).iter().any(|m| {
        m.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
            && m.get("params").and_then(|p| p.get("requestId")) == Some(&call_id)
            && m.get("params")
                .and_then(|p| p.get("reason"))
                .and_then(Value::as_str)
                == Some("cancelled by client")
    })));
    // The connection is still good.
    let r = f
        .backend
        .call_tool("echo", Value::obj(vec![]), None, &CancelToken::new())
        .unwrap();
    assert_eq!(first_text(&r), "{}");
}

#[test]
fn call_timeout_is_reported() {
    let limits = Limits {
        call: Duration::from_millis(400),
        ..quick()
    };
    let f = fixture("timeout", &[], limits);
    let r = f.backend.call_tool(
        "slow",
        Value::obj(vec![("ms", Value::int(20_000))]),
        None,
        &CancelToken::new(),
    );
    assert!(
        matches!(r, Err(CallError::Timeout(ref s)) if s.contains("tools/call")),
        "{r:?}"
    );
}

#[test]
fn rpc_errors_map_to_rpc() {
    let f = fixture("rpc", &[], quick());
    let r = f
        .backend
        .call_tool("fail", Value::obj(vec![]), None, &CancelToken::new());
    assert_eq!(
        r,
        Err(CallError::Rpc {
            code: -32000,
            message: "boom".into(),
            data: Some(Value::obj(vec![("why", Value::str("asked to fail"))])),
        })
    );
    let r = f
        .backend
        .call_tool("nope", Value::obj(vec![]), None, &CancelToken::new());
    assert!(
        matches!(
            r,
            Err(CallError::Rpc {
                code: -32602,
                data: None,
                ..
            })
        ),
        "{r:?}"
    );
}

#[test]
fn notifications_reach_the_sink() {
    let f = fixture("notify", &[], quick());
    let meta = Value::obj(vec![("progressToken", Value::str("tok-7"))]);
    f.backend
        .call_tool(
            "notify",
            Value::obj(vec![]),
            Some(meta),
            &CancelToken::new(),
        )
        .unwrap();
    let ev = f.events.lock().unwrap();
    assert!(ev.iter().all(|(n, _)| n == "fake"));
    assert!(ev
        .iter()
        .any(|(_, e)| matches!(e, BackendEvent::ToolsChanged)));
    let relayed: Vec<&Value> = ev
        .iter()
        .filter_map(|(_, e)| {
            if let BackendEvent::Relay(v) = e {
                Some(v)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(relayed.len(), 2, "progress and message only: {relayed:?}");
    let progress = relayed[0];
    assert_eq!(
        progress.get("method").and_then(Value::as_str),
        Some("notifications/progress")
    );
    assert_eq!(progress.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
    assert_eq!(
        progress
            .get("params")
            .and_then(|p| p.get("progressToken"))
            .and_then(Value::as_str),
        Some("tok-7")
    );
    assert_eq!(
        relayed[1].get("method").and_then(Value::as_str),
        Some("notifications/message")
    );
}

#[test]
fn server_requests_are_answered() {
    let f = fixture("ping", &[], quick());
    let r = f
        .backend
        .call_tool("ping", Value::obj(vec![]), None, &CancelToken::new())
        .unwrap();
    let ping = r.get("ping").unwrap();
    assert_eq!(ping.get("id").and_then(Value::as_str), Some("srv-ping"));
    assert_eq!(ping.get("result"), Some(&Value::obj(vec![])));
    let other = r.get("other").unwrap();
    assert_eq!(
        other
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64),
        Some(-32601)
    );
}

/// Unix prints `exit status: N`, Windows `exit code: N`.
fn exited_with(e: &str, code: i32) -> bool {
    e.contains(&format!("exit status: {code}")) || e.contains(&format!("exit code: {code}"))
}

#[test]
fn crash_fails_the_call_with_stderr_and_the_next_call_respawns() {
    let f = fixture("crash", &[], quick());
    f.backend.start().unwrap();
    let e = unavailable(
        f.backend
            .call_tool("crash", Value::obj(vec![]), None, &CancelToken::new())
            .unwrap_err(),
    );
    assert!(e.contains("fake: crashing now"), "{e}");
    assert!(exited_with(&e, 3), "{e}");
    let st = f.backend.status();
    assert!(!st.up);
    assert_eq!(st.error.as_deref(), Some(e.as_str()));
    assert!(wait_for(|| f.events.lock().unwrap().iter().any(
        |(_, ev)| matches!(ev, BackendEvent::Exited(s) if s == &e)
    )));

    let r = f
        .backend
        .call_tool(
            "echo",
            Value::obj(vec![("again", Value::Bool(true))]),
            None,
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(first_text(&r), "{\"again\":true}");
    let st = f.backend.status();
    assert!(st.up);
    assert_eq!(st.starts, 2);
    // The second process got fresh ids, not the first one's again.
    let inits: Vec<String> = logged(&f)
        .iter()
        .filter(|m| m.get("method").and_then(Value::as_str) == Some("initialize"))
        .map(|m| m.get("id").unwrap().to_json())
        .collect();
    assert_eq!(inits.len(), 2);
    assert_ne!(inits[0], inits[1]);
}

#[test]
fn startup_exit_reports_the_stderr_line() {
    let msg = "FAKE_DISABLE: unknown group \"zzz\" (known: core, locks)";
    let f = fixture("startexit", &[("FAKE_STARTUP_EXIT", msg)], quick());
    let e = unavailable(f.backend.start().unwrap_err());
    assert!(e.contains(msg), "{e}");
    assert!(exited_with(&e, 2), "{e}");
    assert!(
        e.contains("mh-fake-mcp serve") || e.contains("serve"),
        "{e}"
    );
    let st = f.backend.status();
    assert!(!st.up);
    assert!(st.error.unwrap().contains(msg));
    // It never came up, so no Exited event.
    assert!(f.events.lock().unwrap().is_empty());
}

#[test]
fn restart_storm_is_refused_for_a_while() {
    let limits = Limits {
        restart_block: Duration::from_millis(1500),
        ..quick()
    };
    let f = fixture(
        "storm",
        &[("FAKE_STARTUP_EXIT", "fake: bad config")],
        limits,
    );
    for _ in 0..3 {
        let e = unavailable(f.backend.start().unwrap_err());
        assert!(e.contains("fake: bad config"), "{e}");
    }
    assert_eq!(f.backend.status().starts, 3);
    let e = unavailable(
        f.backend
            .call_tool("echo", Value::Null, None, &CancelToken::new())
            .unwrap_err(),
    );
    assert!(e.contains("not restarting"), "{e}");
    assert!(e.contains("fake: bad config"), "{e}");
    assert_eq!(f.backend.status().starts, 3, "no spawn while blocked");

    thread::sleep(Duration::from_millis(1600));
    let e = unavailable(f.backend.start().unwrap_err());
    assert!(!e.contains("not restarting"), "{e}");
    assert_eq!(f.backend.status().starts, 4);
}

#[test]
fn missing_command_says_how_to_fix_it() {
    let dir = temp_dir("missing");
    let sink: EventSink = Arc::new(|_, _| {});
    let b = StdioBackend::new(&spec("mh-no-such-tool-xyz"), vec![], &dir, sink);
    let e = unavailable(b.start().unwrap_err());
    assert!(
        e.contains("cannot find `mh-no-such-tool-xyz` on PATH or ~/.cargo/bin; install it or set `command` in harness.json"),
        "{e}"
    );
    assert_eq!(b.status().starts, 0);
    assert_eq!(b.status().error.as_deref(), Some(e.as_str()));
    // Through the trait's builder too.
    let b = mh_backend::build(
        &spec("mh-no-such-tool-xyz"),
        vec![],
        &dir,
        Arc::new(|_, _| {}),
    )
    .unwrap();
    assert!(b.list_tools().is_err());
}

#[test]
fn shutdown_fails_calls_in_flight_and_kills_a_stubborn_server() {
    let limits = Limits {
        shutdown_grace: Duration::from_millis(300),
        ..quick()
    };
    let f = fixture("shutdown", &[("FAKE_IGNORE_EOF", "1")], limits);
    f.backend.start().unwrap();
    let t0 = Instant::now();
    thread::scope(|s| {
        let call = s.spawn(|| {
            f.backend.call_tool(
                "slow",
                Value::obj(vec![("ms", Value::int(20_000))]),
                None,
                &CancelToken::new(),
            )
        });
        // Shut down only once the call is in flight at the server.
        assert!(wait_for(|| logged(&f)
            .iter()
            .any(
                |m| m.get("method").and_then(Value::as_str) == Some("tools/call")
            )));
        f.backend.shutdown();
        let e = unavailable(call.join().unwrap().unwrap_err());
        assert!(e.contains("shut down") || e.contains("exited"), "{e}");
    });
    assert!(t0.elapsed() < Duration::from_secs(5));
    let st = f.backend.status();
    assert!(!st.up);
    assert_eq!(st.error, None, "a shutdown is not an error");
    assert!(f
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|(_, e)| !matches!(e, BackendEvent::Exited(_))));
    // A later call starts it again.
    f.backend
        .call_tool("echo", Value::obj(vec![]), None, &CancelToken::new())
        .unwrap();
    assert_eq!(f.backend.status().starts, 2);
}

fn log_lines(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| mh_json::parse(l).ok())
        .collect()
}

fn has_event(log: &Path, event: &str) -> bool {
    log_lines(log)
        .iter()
        .any(|m| m.get("event").and_then(Value::as_str) == Some(event))
}

/// The fake's pid, from its `started` log line.
fn fake_pid(log: &Path) -> Option<i64> {
    log_lines(log)
        .iter()
        .find(|m| m.get("event").and_then(Value::as_str) == Some("started"))
        .and_then(|m| m.get("pid").and_then(Value::as_i64))
}

/// Is a process with this pid still there (`kill -0`)? A zombie counts;
/// the backend reaps its children.
#[cfg(unix)]
fn alive(pid: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Is a process with this pid still there (`tasklist`)?
#[cfg(windows)]
fn alive(pid: i64) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stderr(std::process::Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

/// Drop the backend and check the server is gone.
fn assert_drop_stops(f: Fixture) {
    f.backend.start().unwrap();
    let log = f.log.clone();
    assert!(!has_event(&log, "stdin-eof"));
    let pid = fake_pid(&log).unwrap();
    let t0 = Instant::now();
    drop(f.backend);
    assert!(t0.elapsed() < Duration::from_secs(3));
    // Drop closed the server's stdin ...
    assert!(wait_for(|| has_event(&log, "stdin-eof")));
    // ... and the server is gone (killed, if it ignores EOF).
    assert!(wait_for(|| !alive(pid)), "pid {pid} still running");
}

#[test]
fn drop_stops_the_server() {
    assert_drop_stops(fixture("drop", &[], quick()));
}

#[test]
fn drop_kills_a_server_that_ignores_eof() {
    let limits = Limits {
        shutdown_grace: Duration::from_millis(200),
        ..quick()
    };
    assert_drop_stops(fixture("dropkill", &[("FAKE_IGNORE_EOF", "1")], limits));
}

/// A wrapper that starts the fake as its own child (not `exec`), the way
/// `npx`, a `.cmd` shim or `sh -c` does: the server is a grandchild.
fn wrapped(tag: &str, env: &[(&str, &str)], limits: Limits) -> Fixture {
    let mut f = fixture(tag, env, quick());
    let dir = f._dir.clone();
    #[cfg(windows)]
    let wrapper = {
        let w = dir.join("wrap.cmd");
        std::fs::write(&w, format!("@\"{FAKE}\" %*\r\n")).unwrap();
        w
    };
    #[cfg(unix)]
    let wrapper = {
        use std::os::unix::fs::PermissionsExt;
        let w = dir.join("wrap.sh");
        std::fs::write(&w, format!("#!/bin/sh\n'{FAKE}' \"$@\"\nexit $?\n")).unwrap();
        std::fs::set_permissions(&w, std::fs::Permissions::from_mode(0o755)).unwrap();
        w
    };
    let mut env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.push(("FAKE_LOG".into(), f.log.display().to_string()));
    let ev = f.events.clone();
    let sink: EventSink = Arc::new(move |name, e| ev.lock().unwrap().push((name.to_string(), e)));
    f.backend = StdioBackend::new(&spec(&wrapper.display().to_string()), env, &dir, sink)
        .with_limits(limits);
    f
}

/// The server behind a wrapper is stopped with it, even when it ignores
/// EOF (on Windows this needs the job object; killing the wrapper alone
/// would leave it running). The grace leaves the fake behind the wrapper
/// time to read the EOF and log it before the kill: at 200 ms a loaded
/// machine sometimes killed it first, and the EOF check failed.
#[test]
fn drop_stops_a_server_behind_a_wrapper() {
    let limits = Limits {
        shutdown_grace: Duration::from_millis(1000),
        ..quick()
    };
    assert_drop_stops(wrapped("wrapdrop", &[("FAKE_IGNORE_EOF", "1")], limits));
}

/// A server behind a wrapper that never answers `initialize` is stopped
/// when the handshake times out, not left running. The limit leaves the
/// wrapper time to start the fake, which must log its pid first: at 500 ms
/// a slow start was cut short before that, and the test failed about one
/// run in five on a loaded machine.
#[test]
fn a_hung_server_behind_a_wrapper_is_stopped_at_the_handshake_timeout() {
    let limits = Limits {
        handshake: Duration::from_millis(2000),
        restart_block: Duration::ZERO,
        ..quick()
    };
    let f = wrapped("wraphang", &[("FAKE_STARTUP_HANG", "1")], limits);
    assert!(f.backend.start().is_err());
    let pid = fake_pid(&f.log).expect("the fake logged its start");
    assert!(wait_for(|| !alive(pid)), "pid {pid} still running");
}

#[test]
fn a_server_that_hangs_at_startup_trips_the_storm_guard() {
    let limits = Limits {
        handshake: Duration::from_millis(300),
        restart_block: Duration::from_secs(30),
        ..quick()
    };
    let f = fixture("hang", &[("FAKE_STARTUP_HANG", "1")], limits);
    for _ in 0..3 {
        let e = unavailable(f.backend.start().unwrap_err());
        assert!(e.contains("initialize failed"), "{e}");
    }
    assert_eq!(f.backend.status().starts, 3);
    let e = unavailable(f.backend.start().unwrap_err());
    assert!(e.contains("not restarting"), "{e}");
    assert_eq!(f.backend.status().starts, 3, "no spawn while blocked");
}

#[test]
fn limits_come_from_the_spec() {
    let dir = temp_dir("speclimits");
    let mut s = spec(FAKE);
    s.call_s = 1;
    s.handshake_s = 5;
    let b = StdioBackend::new(&s, vec![], &dir, Arc::new(|_, _| {}));
    let t0 = Instant::now();
    let r = b.call_tool(
        "slow",
        Value::obj(vec![("ms", Value::int(20_000))]),
        None,
        &CancelToken::new(),
    );
    assert!(
        matches!(r, Err(CallError::Timeout(ref s)) if s.contains("within 1 s")),
        "{r:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(4));
}

fn env_of(b: &StdioBackend, names: &[&str]) -> Value {
    let names = Value::Array(names.iter().map(|n| Value::str(*n)).collect());
    b.call_tool(
        "env",
        Value::obj(vec![("names", names)]),
        None,
        &CancelToken::new(),
    )
    .unwrap()
    .get("vars")
    .cloned()
    .unwrap()
}

#[test]
fn env_pass_restricts_the_environment() {
    // Cargo sets both for test processes; neither is in BASE_ENV.
    let passed = "CARGO_MANIFEST_DIR";
    let other = "CARGO_PKG_NAME";
    let here = |k: &str| std::env::var(k).map(Value::str).unwrap_or(Value::Null);
    assert!(std::env::var(passed).is_ok() && std::env::var(other).is_ok());
    let names = [
        passed,
        other,
        "PATH",
        "SPEC_VAR",
        "EXTRA_VAR",
        "MH_NOT_SET_ANYWHERE",
    ];
    let dir = temp_dir("envpass");
    let extra = vec![("EXTRA_VAR".to_string(), "from-env-arg".to_string())];

    // Inherit (env_pass None): ours are all there, plus spec.env and env.
    let mut s = spec(FAKE);
    s.env = vec![("SPEC_VAR".into(), "from-spec".into())];
    let b = StdioBackend::new(&s, extra.clone(), &dir, Arc::new(|_, _| {}));
    let v = env_of(&b, &names);
    assert_eq!(v.get(passed), Some(&here(passed)));
    assert_eq!(v.get(other), Some(&here(other)));
    assert_eq!(v.get("SPEC_VAR"), Some(&Value::str("from-spec")));
    assert_eq!(v.get("EXTRA_VAR"), Some(&Value::str("from-env-arg")));

    // Restricted: BASE_ENV, the listed variables we have, spec.env and env.
    s.env_pass = Some(vec![passed.into(), "MH_NOT_SET_ANYWHERE".into()]);
    let b = StdioBackend::new(&s, extra, &dir, Arc::new(|_, _| {}));
    let v = env_of(&b, &names);
    assert_eq!(v.get(passed), Some(&here(passed)));
    assert_eq!(v.get(other), Some(&Value::Null), "{v:?}");
    assert_eq!(v.get("PATH"), Some(&here("PATH")));
    assert_eq!(v.get("SPEC_VAR"), Some(&Value::str("from-spec")));
    assert_eq!(v.get("EXTRA_VAR"), Some(&Value::str("from-env-arg")));
    assert_eq!(v.get("MH_NOT_SET_ANYWHERE"), Some(&Value::Null));
}

#[test]
fn unknown_kind_lists_the_known_ones() {
    let mut s = spec(FAKE);
    s.kind = "http-mcp".into();
    let e = mh_backend::build(&s, vec![], &temp_dir("kind"), Arc::new(|_, _| {}))
        .err()
        .unwrap();
    assert!(e.contains("unknown kind \"http-mcp\""), "{e}");
    assert!(e.contains("known: stdio-mcp"), "{e}");
}

/// A reply that does not parse fails its call at once (it used to be
/// skipped, and the call waited out its whole limit).
#[test]
fn an_unparseable_reply_fails_its_call_at_once() {
    let f = fixture("deep", &[], quick());
    let t = Instant::now();
    let r = f
        .backend
        .call_tool("deep", Value::obj(vec![]), None, &CancelToken::new());
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    match r {
        Err(CallError::Rpc { code, message, .. }) => {
            assert_eq!(code, -32700);
            assert!(message.contains("not valid JSON"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // The connection is still usable.
    assert!(f
        .backend
        .call_tool("echo", Value::obj(vec![]), None, &CancelToken::new())
        .is_ok());
}
