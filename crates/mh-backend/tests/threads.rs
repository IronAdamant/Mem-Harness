//! Thread and CPU use of the whole test process, so these run one at a
//! time in their own test binary. Unix reads them from `/proc` or `ps`,
//! Windows from PowerShell (about 300 ms a reading).

use mh_backend::stdio::{Limits, StdioBackend};
use mh_backend::{Backend, CancelToken};
use mh_core::BackendSpec;
use mh_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FAKE: &str = env!("CARGO_BIN_EXE_mh-fake-mcp");
static DIRS: AtomicUsize = AtomicUsize::new(0);
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn dir(tag: &str) -> std::path::PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("mh-threads-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(unix)]
const HS: &str = r#"read l; id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"; read l; "#;

/// Windows: `field` (`Threads.Count`, `TotalProcessorTime.TotalSeconds`) of
/// this process, from .NET through PowerShell.
#[cfg(windows)]
fn this_process(field: &str) -> f64 {
    let script = format!(
        // Invariant culture: a decimal point, not a comma, whatever the locale.
        "[Diagnostics.Process]::GetProcessById({}).{field}.ToString([Globalization.CultureInfo]::InvariantCulture)",
        std::process::id()
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse()
        .unwrap_or_else(|_| panic!("powershell gave {s:?} for {field}"))
}

fn limits() -> Limits {
    Limits {
        handshake: Duration::from_secs(5),
        page: Duration::from_secs(5),
        call: Duration::from_secs(10),
        restart_block: Duration::ZERO,
        shutdown_grace: Duration::from_millis(200),
    }
}

/// Threads in this process.
#[cfg(unix)]
fn threads() -> usize {
    if let Ok(rd) = std::fs::read_dir("/proc/self/task") {
        return rd.count();
    }
    let out = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count() - 1
}

/// Threads in this process.
#[cfg(windows)]
fn threads() -> usize {
    this_process("Threads.Count") as usize
}

/// The thread count once it has stopped falling (up to 3 s).
fn settle() -> usize {
    let mut last = threads();
    for _ in 0..15 {
        thread::sleep(Duration::from_millis(200));
        let now = threads();
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

fn fake() -> StdioBackend {
    let d = dir("fake");
    let spec = BackendSpec::stdio("fake", FAKE, &["serve"]);
    StdioBackend::new(&spec, vec![], &d, Arc::new(|_, _| {})).with_limits(limits())
}

#[test]
fn plain_restarts_do_not_leak_threads() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let b = fake();
    b.start().unwrap();
    let before = settle();
    for _ in 0..15 {
        let _ = b.call_tool("crash", Value::obj(vec![]), None, &CancelToken::new());
        b.start().unwrap();
    }
    let after = settle();
    assert!(after <= before + 2, "before {before}, after {after}");
}

/// A server that leaves a background process holding its stdout and
/// stderr: the group stop ends it, so the reader and stderr threads of
/// each old connection finish.
#[test]
fn a_grandchild_holding_the_pipes_does_not_leak_threads() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("grand");
    #[cfg(unix)]
    let spec = {
        let script = format!("{HS}sleep 60 & read l; exit 3");
        BackendSpec::stdio("sh", "/bin/sh", &["-c", &script])
    };
    #[cfg(windows)]
    let spec = BackendSpec::stdio(
        "sh",
        FAKE,
        &["script", "hs", "spawn:60000:oe:bg:-", "read", "exit:3"],
    );
    let b = StdioBackend::new(&spec, vec![], &d, Arc::new(|_, _| {})).with_limits(limits());
    b.start().unwrap();
    let before = settle();
    for _ in 0..10 {
        let _ = b.call_tool("x", Value::obj(vec![]), None, &CancelToken::new());
        b.start().unwrap();
    }
    b.shutdown();
    let after = settle();
    assert!(after <= before + 2, "before {before}, after {after}");
}

/// CPU used by 9 idle started backends. Printed; the bound is loose (the
/// waiter polls every 250 ms when idle, so this is far below it).
#[test]
fn idle_cpu_of_nine_backends() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let backends: Vec<StdioBackend> = (0..9).map(|_| fake()).collect();
    for b in &backends {
        b.start().unwrap();
    }
    #[cfg(windows)]
    let cpu = || this_process("TotalProcessorTime.TotalSeconds");
    #[cfg(unix)]
    let cpu = || {
        let out = std::process::Command::new("ps")
            .args(["-o", "time=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // [[dd-]hh:]mm:ss[.cc]
        let mut secs = 0.0;
        for part in s.split(':') {
            secs = secs * 60.0 + part.parse::<f64>().unwrap_or(0.0);
        }
        secs
    };
    thread::sleep(Duration::from_millis(500));
    let c0 = cpu();
    let t0 = Instant::now();
    thread::sleep(Duration::from_secs(3));
    let used = cpu() - c0;
    let pct = used / t0.elapsed().as_secs_f64() * 100.0;
    eprintln!("9 idle backends: {used:.3} s CPU in 3 s = {pct:.2}% of a core");
    assert!(pct < 10.0, "{pct:.2}%");
}
