//! Run an external command with a timeout.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct CmdOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

const MAX_OUT: u64 = 4 << 20;

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = r {
            let _ = r.take(MAX_OUT).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Run `program args` in `cwd`. The program is looked up with
/// `mh_core::project::find_command`. `Err` is a short reason: not found,
/// cannot start, or timed out (the child is killed).
pub fn run_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<CmdOut, String> {
    let path = mh_core::project::find_command(program, cwd)
        .ok_or_else(|| format!("{program} not found on PATH or in ~/.cargo/bin; install it"))?;
    let mut child = Command::new(path)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(format!("cannot wait for {program}: {e}")),
        }
    };
    Ok(CmdOut {
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
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
}
