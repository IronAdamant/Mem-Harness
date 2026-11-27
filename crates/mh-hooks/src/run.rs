//! Run an external command with a timeout.
//!
//! The timeout covers the whole run, reading the output included. A child
//! that starts a background process can exit while that process still holds
//! its stdout or stderr open; the reads would then wait for the background
//! process. So the reads are abandoned at the deadline too, and on Unix the
//! child runs in its own process group, which is killed (TERM, then KILL
//! for anything that ignores it) on timeout or when
//! the reads outlast the deadline. Its descendants are listed before the
//! kill, so one that left the group (`setsid`) is signalled by pid too; on
//! Linux the hook process is a subreaper ([`adopt_orphans`]), so a process
//! that double-forked out of the tree is found as well. Signals go through
//! `mh_sys` (no `kill` binary needed). On macOS a process that double-forks
//! and whose middle process exits before the timeout is re-parented to
//! launchd and not found. On Windows the child is put in a job object as
//! soon as it starts, so everything it starts afterwards is in the job too,
//! and the job is ended where Unix kills the group. The hook process does
//! not hand its own stdin, stdout and stderr to the child
//! ([`mh_sys::stop_std_inheritance`], called at startup), so a process the
//! child leaves running cannot hold Claude Code's pipe open.

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

/// The pids of the children `run_command` is running now. Spawning and
/// every `ps` read in [`stragglers`] hold this lock, so a child being
/// started, or another thread's `ps`, is never mistaken for an orphan.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Variables every command [`run_command`] starts gets, set once by
/// [`set_child_env`].
static CHILD_ENV: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Give every command [`run_command`] starts these variables from now on
/// (the backends' store folders, `Config::storage_env`), so a tool run by
/// the brief reads the same store as its MCP server. The first call wins.
pub fn set_child_env(env: Vec<(String, String)>) {
    let _ = CHILD_ENV.set(env);
}

/// Set once [`adopt_orphans`] made this process a subreaper.
static SUBREAPER: AtomicBool = AtomicBool::new(false);

/// Linux: make this process the reaper of the orphans its commands leave,
/// so a process that double-forks out of a hook command's tree is
/// re-parented here (where a timeout can find it) instead of to init. Call
/// it only in a short-lived process (the hook process): orphans that exit
/// stay zombies until this process exits. A no-op elsewhere; idempotent.
pub fn adopt_orphans() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if mh_sys::become_subreaper() {
            SUBREAPER.store(true, Ordering::SeqCst);
        }
    });
}

/// Stop the child's process group (its id is the child's pid), which also
/// reaches any background process it started, and each of `others`
/// (descendants that left the group, with their parents, listed before
/// anything was signalled): TERM first, then KILL after [`TERM_GRACE`] for
/// anything that ignored it. Before the KILL the table is read again, and
/// a pid in `others` is sent KILL only when it is still there under the
/// same parent, or re-parented to init or to us (its parent exited on the
/// TERM): one that exited and was reaped may have had its pid reused. The
/// group itself needs no check, as its leader is our child, not yet
/// reaped. This only runs on the timeout path, so the short wait costs
/// nothing in a normal run. Uses `mh_sys`, so it needs no `kill` binary; a
/// no-op where there are no signals (Windows).
fn kill_group(pid: u32, others: &[(u32, u32)]) {
    let mut any = mh_sys::signal_group(pid, mh_sys::SIGTERM);
    for &(p, _) in others {
        any |= mh_sys::signal(p, mh_sys::SIGTERM);
    }
    // Signalling fails once no process is left.
    if !any {
        return;
    }
    std::thread::sleep(TERM_GRACE);
    mh_sys::signal_group(pid, mh_sys::SIGKILL);
    if others.is_empty() {
        return;
    }
    let now = mh_sys::ProcessTable::read();
    let me = std::process::id();
    for &(p, parent) in others {
        if now
            .parent(p)
            .is_some_and(|pp| pp == parent || pp == 1 || pp == me)
        {
            mh_sys::signal(p, mh_sys::SIGKILL);
        }
    }
}

/// How long a process group gets to exit on TERM before it is sent KILL.
const TERM_GRACE: Duration = Duration::from_millis(200);

/// What else to stop with a child, each with its parent: its descendants
/// (`child` still running; read before any signal, so one that called
/// `setsid` is found) and, when this process is a subreaper
/// ([`adopt_orphans`]), the orphans re-parented to us: our direct children
/// that are not a running command, with their own descendants. All of it
/// comes from one `ps` snapshot, so a process a healthy command starts
/// meanwhile is never taken for an orphan. Unix only.
fn stragglers(child: Option<u32>) -> Vec<(u32, u32)> {
    if !cfg!(unix) {
        return Vec::new();
    }
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    let table = mh_sys::ProcessTable::read();
    let mut v: Vec<u32> = child.map(|c| table.descendants(c)).unwrap_or_default();
    if SUBREAPER.load(Ordering::SeqCst) {
        for orphan in table.children(std::process::id()) {
            if running.contains(&orphan) {
                continue;
            }
            for p in std::iter::once(orphan).chain(table.descendants(orphan)) {
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
    }
    v.into_iter()
        .filter_map(|p| table.parent(p).map(|pp| (p, pp)))
        .collect()
}

fn kill_tree(child: &mut Child, job: Option<&mh_sys::Job>) {
    if let Some(j) = job {
        j.terminate();
    }
    let others = stragglers(Some(child.id()));
    kill_group(child.id(), &others);
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
    if let Some(env) = CHILD_ENV.get() {
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
    }
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let (mut child, job) = {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        let child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {program}: {e}"))?;
        // Windows: the stand-in for the process group. Not kill-on-close:
        // as on Unix, a background process that lets go of the pipes is
        // left running when the command ends in time.
        let job = mh_sys::Job::new(false).filter(|j| j.assign(&child));
        running.push(child.id());
        (child, job)
    };
    let result = finish(program, &mut child, job.as_ref(), timeout);
    RUNNING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|p| *p != child.id());
    result
}

/// Wait for a started child under the deadline; see [`run_command`].
fn finish(
    program: &str,
    child: &mut Child,
    job: Option<&mh_sys::Job>,
    timeout: Duration,
) -> Result<CmdOut, String> {
    let out = Drain::spawn(child.stdout.take());
    let err = Drain::spawn(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(child, job);
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                kill_tree(child, job);
                return Err(format!("cannot wait for {program}: {e}"));
            }
        }
    };
    if !wait_drains(&out, &err, deadline) {
        // Something the child started still holds the pipes. Stop it and
        // keep what was read; the reader threads end when the pipes close.
        // The child is gone, so only orphans adopted on Linux can be
        // found outside its group.
        if let Some(j) = job {
            j.terminate();
        }
        kill_group(child.id(), &stragglers(None));
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

    /// A background process that ignores TERM is still stopped (by KILL).
    #[test]
    fn a_process_ignoring_term_is_killed() {
        let cwd = std::env::temp_dir();
        let marker = cwd.join(format!("mh-run-noterm-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "(trap '' TERM; sleep 2; touch '{}') & sleep 30",
            marker.display()
        );
        let e = run_command(
            "/bin/sh",
            &["-c", &script],
            &cwd,
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            !marker.exists(),
            "a process that ignored TERM outlived the kill"
        );
    }

    /// A descendant that left the process group (`setsid`) is found before
    /// the kill and stopped by pid.
    #[test]
    fn a_process_that_left_the_group_is_killed() {
        let Some(perl) = ["/usr/bin/perl", "/bin/perl"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
        else {
            return;
        };
        let cwd = std::env::temp_dir();
        let marker = cwd.join(format!("mh-run-setsid-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "{perl} -MPOSIX -e 'POSIX::setsid(); sleep 2; open(F, \">\", \"{}\")' & sleep 30",
            marker.display()
        );
        let e = run_command(
            "/bin/sh",
            &["-c", &script],
            &cwd,
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            !marker.exists(),
            "a process in its own session outlived the kill"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// The child exits at once but a background process keeps stderr open:
    /// the deadline still holds, and the output read so far is kept.
    #[test]
    fn background_process_holding_the_pipe_does_not_outlast_the_timeout() {
        let cwd = std::env::temp_dir();
        let t = Instant::now();
        let r = run_command(
            "cmd",
            &["/c", "start /b ping -n 30 127.0.0.1 >nul & echo hi"],
            &cwd,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert_eq!(r.stdout.trim(), "hi");
        assert_eq!(r.code, Some(0));
    }

    /// A child that does not exit is stopped with everything it started:
    /// killing `cmd` alone would leave the background process to write the
    /// marker.
    #[test]
    fn timeout_kills_the_whole_tree() {
        let dir = std::env::temp_dir().join(format!("mh-run-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let script = dir.join("tree.cmd");
        std::fs::write(
            &script,
            format!(
                "@start /b cmd /c \"ping -n 3 127.0.0.1 >nul & echo x > {}\"\r\n@ping -n 30 127.0.0.1 >nul\r\n",
                marker.display()
            ),
        )
        .unwrap();
        let t = Instant::now();
        let e = run_command(
            "cmd",
            &["/c", &script.display().to_string()],
            &dir,
            Duration::from_millis(700),
        )
        .unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!marker.exists(), "the background process outlived the kill");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
