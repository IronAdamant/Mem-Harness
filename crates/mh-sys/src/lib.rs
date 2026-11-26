//! The only place Mem-Harness calls the operating system directly.
//!
//! std cannot signal a process group, make a process a subreaper, or list
//! a process's descendants. These few calls are declared here by hand,
//! against the C library std already links, so the workspace still has no
//! dependencies. Every function is safe to call, and fails by returning
//! `false` or an empty list. Nothing here is unsafe outside this file.
//!
//! On platforms without these calls (Windows today) each function is a
//! no-op that returns `false` or nothing, and callers fall back to what
//! std offers (`Child::kill`).

/// `SIGTERM`: ask a process to exit. 15 on every Unix this builds for.
pub const SIGTERM: i32 = 15;
/// `SIGKILL`: end a process. 9 on every Unix this builds for.
pub const SIGKILL: i32 = 9;

#[cfg(unix)]
mod ffi {
    extern "C" {
        // int kill(pid_t pid, int sig); pid_t is a 32-bit int on Linux and macOS.
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
    #[cfg(target_os = "linux")]
    extern "C" {
        // int prctl(int option, ...);
        pub fn prctl(option: i32, ...) -> i32;
    }
}

/// Send `sig` to every process in the group `pgid`. `false` when there is
/// no such group (or on platforms without process groups).
pub fn signal_group(pgid: u32, sig: i32) -> bool {
    #[cfg(unix)]
    {
        let Ok(id) = i32::try_from(pgid) else {
            return false;
        };
        // 0 and 1 would signal our own group or every process we may signal.
        if id <= 1 {
            return false;
        }
        // SAFETY: kill(2) takes plain integers and touches no memory of ours.
        unsafe { ffi::kill(-id, sig) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, sig);
        false
    }
}

/// Send `sig` to the process `pid`. `false` when it is gone.
pub fn signal(pid: u32, sig: i32) -> bool {
    #[cfg(unix)]
    {
        let Ok(id) = i32::try_from(pid) else {
            return false;
        };
        if id <= 1 {
            return false;
        }
        // SAFETY: as above.
        unsafe { ffi::kill(id, sig) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
        false
    }
}

/// Is a process with this id alive (or a zombie not yet reaped)?
pub fn alive(pid: u32) -> bool {
    signal(pid, 0)
}

/// Linux only: make this process the reaper of its orphaned descendants,
/// so a grandchild that double-forks or calls `setsid` is re-parented to
/// us (and can be found with [`descendants`]) instead of to init.
/// `false` elsewhere, or when the call fails.
pub fn become_subreaper() -> bool {
    #[cfg(target_os = "linux")]
    {
        const PR_SET_CHILD_SUBREAPER: i32 = 36;
        // SAFETY: prctl with this option reads only the integer argument.
        unsafe { ffi::prctl(PR_SET_CHILD_SUBREAPER, 1u64, 0u64, 0u64, 0u64) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Every live descendant of `root` (children, their children, ...),
/// nearest first. Read from `ps`, so it is a snapshot: a process that
/// double-forks and is re-parented to init before the snapshot is not
/// found (on Linux, [`become_subreaper`] prevents that). Empty when `ps`
/// is missing or on platforms without it.
pub fn descendants(root: u32) -> Vec<u32> {
    let pairs = process_table();
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &(pid, ppid) in &pairs {
            if ppid == p && pid != root && !out.contains(&pid) {
                out.push(pid);
                frontier.push(pid);
            }
        }
    }
    out
}

/// `(pid, ppid)` for every process, from `ps -A -o pid=,ppid=`.
fn process_table() -> Vec<(u32, u32)> {
    if !cfg!(unix) {
        return Vec::new();
    }
    let ps = ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .unwrap_or("ps");
    let Ok(child) = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=,ppid="])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    // `ps` lists itself as our child; leave it out, or a caller would
    // signal a pid that is gone by then (and may have been reused).
    let own = child.id();
    let Ok(out) = child.wait_with_output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .filter(|&(pid, _)| pid != own)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn wait_gone(pid: u32) -> bool {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn refuses_our_own_group_and_init() {
        assert!(!signal_group(0, 0));
        assert!(!signal_group(1, 0));
        assert!(!signal(0, 0));
        assert!(!signal(1, 0));
        assert!(!signal_group(u32::MAX, 0));
    }

    #[test]
    fn group_signal_reaches_a_grandchild() {
        use std::os::unix::process::CommandExt;
        // The shell prints its background child's pid, then waits.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        assert!(alive(grandchild));
        assert!(descendants(child.id()).contains(&grandchild));
        // The listing's own `ps` is never reported as our descendant.
        let ours = descendants(std::process::id());
        assert!(ours.contains(&child.id()) && ours.contains(&grandchild));
        assert!(
            ours.iter().all(|&p| alive(p)),
            "a reported pid is gone: {ours:?}"
        );
        assert!(signal_group(child.id(), SIGKILL));
        let _ = child.wait();
        assert!(wait_gone(grandchild), "grandchild survived the group kill");
    }
}
