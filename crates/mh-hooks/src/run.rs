//! Run an external command with a timeout.
//!
//! The timeout covers the whole run, reading the output included. A child
//! that starts a background process can exit while that process still holds
//! its stdout or stderr open; the reads would then wait for the background
//! process. So the reads are abandoned at the deadline too, and on Unix the
//! child runs in its own process group, which is killed (`kill -TERM
//! -<pgid>`, std only) on timeout or when the reads outlast the deadline.
//! On Windows only the child itself is killed.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct CmdOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

const MAX_OUT: usize = 4 << 20;
const POLL: Duration = Duration::from_millis(10);
/// How long the reads may take to finish once the group was killed.
const GRACE: Duration = Duration::from_millis(200);

/// Output read so far by a background thread, and whether it reached the end.
#[derive(Default)]
struct Drain {
    buf: Mutex<Vec<u8>>,
    done: AtomicBool,
}

impl Drain {
    fn spawn<R: Read + Send + 'static>(r: Option<R>) -> Arc<Drain> {
        let d = Arc::new(Drain::default());
        let Some(mut r) = r else {
            d.done.store(true, Ordering::SeqCst);
            return d;
        };
        let t = Arc::clone(&d);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = t.buf.lock().unwrap_or_else(|e| e.into_inner());
                        let room = MAX_OUT.saturating_sub(b.len());
                        b.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            t.done.store(true, Ordering::SeqCst);
        });
        d
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn text(&self) -> String {
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&b).into_owned()
    }
}

/// Wait until both reads finished or `until` passed. True when finished.
fn wait_drains(a: &Drain, b: &Drain, until: Instant) -> bool {
    loop {
        if a.done() && b.done() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(unix)]
fn own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_cmd: &mut Command) {}

/// Signal the child's process group (its id is the child's pid), which
/// also reaches any background process it started.
#[cfg(unix)]
fn kill_group(pid: u32) {
    let kill = ["/bin/kill", "/usr/bin/kill"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .unwrap_or("kill");
    let _ = Command::new(kill)
        .arg("-TERM")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

fn kill_tree(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Run `program args` in `cwd`. The program is looked up with
/// `mh_core::project::find_command`. `Err` is a short reason: not found,
/// cannot start, or timed out (the child and its process group are
/// killed). A child that exits in time but leaves a background process
/// holding its output returns what was read by the deadline.
pub fn run_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<CmdOut, String> {
    let path = mh_core::project::find_command(program, cwd)
        .ok_or_else(|| format!("{program} not found on PATH or in ~/.cargo/bin; install it"))?;
    let mut cmd = Command::new(path);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    let out = Drain::spawn(child.stdout.take());
    let err = Drain::spawn(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                kill_tree(&mut child);
                return Err(format!("cannot wait for {program}: {e}"));
            }
        }
    };
    if !wait_drains(&out, &err, deadline) {
        // Something the child started still holds the pipes. Stop it and
        // keep what was read; the reader threads end when the pipes close.
        kill_group(child.id());
        wait_drains(&out, &err, Instant::now() + GRACE);
    }
    Ok(CmdOut {
        code: status.code(),
        stdout: out.text(),
        stderr: err.text(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runs_and_times_out() {
        let cwd = std::env::temp_dir();
        let ok = run_command("/bin/echo", &["hi"], &cwd, Duration::from_secs(5)).unwrap();
        assert_eq!(ok.stdout, "hi\n");
        assert_eq!(ok.code, Some(0));
        let t = Instant::now();
        let e = run_command("/bin/sleep", &["5"], &cwd, Duration::from_millis(100)).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(run_command(
            "definitely-not-a-program-xyz",
            &[],
            &cwd,
            Duration::from_secs(1)
        )
        .unwrap_err()
        .contains("not found"));
    }

    /// The child exits at once but a background process keeps stdout open:
    /// the deadline still holds, the output read so far is kept, and the
    /// background process is killed with the group.
    #[test]
    fn background_process_holding_the_pipe_does_not_outlast_the_timeout() {
        let cwd = std::env::temp_dir();
        let t = Instant::now();
        let r = run_command(
            "/bin/sh",
            &["-c", "sleep 30 & echo hi"],
            &cwd,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert_eq!(r.stdout, "hi\n");
        assert_eq!(r.code, Some(0));
    }

    /// A child that does not exit is killed with everything it started.
    #[test]
    fn timeout_kills_the_whole_group() {
        let cwd = std::env::temp_dir();
        let marker = cwd.join(format!("mh-run-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("(sleep 2; touch '{}') & sleep 30", marker.display());
        let t = Instant::now();
        let e = run_command(
            "/bin/sh",
            &["-c", &script],
            &cwd,
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "the background process outlived the kill");
    }
}
