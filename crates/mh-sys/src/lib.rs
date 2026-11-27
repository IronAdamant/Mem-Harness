//! The only place Mem-Harness calls the operating system directly.
//!
//! std cannot signal a process group, make a process a subreaper, or list
//! a process's descendants. These few calls are declared here by hand,
//! against the C library std already links, so the workspace still has no
//! dependencies. Every function is safe to call, and fails by returning
//! `false` or an empty list. Nothing here is unsafe outside this file.
//!
//! Windows has no process groups or signals. There a [`Job`] (a Windows
//! job object) does what a process group does on Unix: a child is put in
//! one as soon as it is started, every process it starts joins it, and
//! [`Job::terminate`] ends them all. [`stop_std_inheritance`] keeps our own
//! stdin, stdout and stderr from being handed to every process we start.
//! The Unix-only functions are no-ops on Windows and the Windows-only ones
//! are no-ops on Unix, each returning `false`, `None` or nothing, and
//! callers fall back to what std offers (`Child::kill`).

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

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
mod win {
    use std::ffi::c_void;

    pub type HANDLE = *mut c_void;
    pub type BOOL = i32;

    /// JOBOBJECT_BASIC_LIMIT_INFORMATION.
    #[repr(C)]
    #[derive(Default)]
    pub struct BasicLimit {
        pub per_process_user_time_limit: i64,
        pub per_job_user_time_limit: i64,
        pub limit_flags: u32,
        pub minimum_working_set_size: usize,
        pub maximum_working_set_size: usize,
        pub active_process_limit: u32,
        pub affinity: usize,
        pub priority_class: u32,
        pub scheduling_class: u32,
    }

    /// JOBOBJECT_EXTENDED_LIMIT_INFORMATION (IO_COUNTERS is six u64s).
    #[repr(C)]
    #[derive(Default)]
    pub struct ExtendedLimit {
        pub basic: BasicLimit,
        pub io: [u64; 6],
        pub process_memory_limit: usize,
        pub job_memory_limit: usize,
        pub peak_process_memory_used: usize,
        pub peak_job_memory_used: usize,
    }

    /// JOBOBJECT_BASIC_ACCOUNTING_INFORMATION.
    #[repr(C)]
    #[derive(Default)]
    pub struct Accounting {
        pub times: [i64; 4],
        pub total_page_fault_count: u32,
        pub total_processes: u32,
        pub active_processes: u32,
        pub total_terminated_processes: u32,
    }

    pub const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: i32 = 1;
    pub const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    pub const HANDLE_FLAG_INHERIT: u32 = 1;
    /// STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE: (DWORD)-10, -11, -12.
    pub const STD_HANDLES: [u32; 3] = [0xFFFF_FFF6, 0xFFFF_FFF5, 0xFFFF_FFF4];

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> HANDLE;
        pub fn SetInformationJobObject(
            job: HANDLE,
            class: i32,
            info: *mut c_void,
            len: u32,
        ) -> BOOL;
        pub fn QueryInformationJobObject(
            job: HANDLE,
            class: i32,
            info: *mut c_void,
            len: u32,
            returned: *mut u32,
        ) -> BOOL;
        pub fn AssignProcessToJobObject(job: HANDLE, process: HANDLE) -> BOOL;
        pub fn TerminateJobObject(job: HANDLE, exit_code: u32) -> BOOL;
        pub fn CloseHandle(handle: HANDLE) -> BOOL;
        pub fn GetStdHandle(which: u32) -> HANDLE;
        pub fn SetHandleInformation(handle: HANDLE, mask: u32, flags: u32) -> BOOL;
    }
}

/// Windows: a job object holding a child and everything it starts, the
/// stand-in for a Unix process group. Elsewhere [`Job::new`] returns `None`.
///
/// Put the child in with [`Job::assign`] right after spawning it. A process
/// the child starts in the moment before that is not in the job; every
/// later one is, however deep and whether or not its parent still runs.
pub struct Job {
    #[cfg(windows)]
    handle: win::HANDLE,
}

// SAFETY: a job handle is a reference to a kernel object; the calls made
// with it are thread-safe and the handle is closed only in `drop`.
unsafe impl Send for Job {}
// SAFETY: as above.
unsafe impl Sync for Job {}

impl Job {
    /// A new, empty job. With `kill_on_close`, whatever is still in it when
    /// the `Job` is dropped (or this process exits) is ended then. `None`
    /// off Windows, or when the job cannot be made.
    pub fn new(kill_on_close: bool) -> Option<Job> {
        #[cfg(windows)]
        {
            // SAFETY: both pointers may be null: no security attributes, no name.
            let handle = unsafe { win::CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let job = Job { handle };
            if kill_on_close {
                let mut info = win::ExtendedLimit::default();
                info.basic.limit_flags = win::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                // SAFETY: `info` is a live JOBOBJECT_EXTENDED_LIMIT_INFORMATION
                // of the size given.
                let ok = unsafe {
                    win::SetInformationJobObject(
                        job.handle,
                        win::JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                        (&mut info as *mut win::ExtendedLimit).cast(),
                        std::mem::size_of::<win::ExtendedLimit>() as u32,
                    )
                };
                if ok == 0 {
                    return None;
                }
            }
            Some(job)
        }
        #[cfg(not(windows))]
        {
            let _ = kill_on_close;
            None
        }
    }

    /// Put a running child in the job. `false` when it has exited or the
    /// call fails.
    pub fn assign(&self, child: &std::process::Child) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: both handles are open: the job's for as long as
            // `self`, the child's process handle for as long as `child`.
            unsafe { win::AssignProcessToJobObject(self.handle, child.as_raw_handle().cast()) != 0 }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            false
        }
    }

    /// End every process in the job. `false` when the call fails.
    pub fn terminate(&self) -> bool {
        #[cfg(windows)]
        {
            // SAFETY: the handle is open for as long as `self`.
            unsafe { win::TerminateJobObject(self.handle, 1) != 0 }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// How many processes in the job are still running (0 when unknown).
    pub fn active(&self) -> u32 {
        #[cfg(windows)]
        {
            let mut info = win::Accounting::default();
            // SAFETY: `info` is a live JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
            // of the size given; the returned-length pointer may be null.
            let ok = unsafe {
                win::QueryInformationJobObject(
                    self.handle,
                    win::JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                    (&mut info as *mut win::Accounting).cast(),
                    std::mem::size_of::<win::Accounting>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                0
            } else {
                info.active_processes
            }
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed only here.
        #[cfg(windows)]
        unsafe {
            win::CloseHandle(self.handle);
        }
    }
}

/// Windows: stop handing our own stdin, stdout and stderr to every process
/// we start. std starts children with handle inheritance on, so otherwise a
/// child, and anything it leaves running, holds a copy of the pipes our
/// parent reads, and the parent sees no end of our output until the last of
/// them exits. Pipes std makes for a child (`Stdio::piped`, `inherit`) are
/// made inheritable by std itself, so they still work. Call once at
/// startup. `false` off Windows or when a call fails.
pub fn stop_std_inheritance() -> bool {
    #[cfg(windows)]
    {
        let mut all = true;
        for which in win::STD_HANDLES {
            // SAFETY: GetStdHandle takes a constant. A null or invalid
            // result is skipped; SetHandleInformation only changes a flag.
            unsafe {
                let h = win::GetStdHandle(which);
                if h.is_null() || h as isize == -1 {
                    continue;
                }
                if win::SetHandleInformation(h, win::HANDLE_FLAG_INHERIT, 0) == 0 {
                    all = false;
                }
            }
        }
        all
    }
    #[cfg(not(windows))]
    {
        false
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
/// is missing or on platforms without it. To ask several questions of one
/// moment, read a [`ProcessTable`] once.
pub fn descendants(root: u32) -> Vec<u32> {
    ProcessTable::read().descendants(root)
}

/// One `ps` snapshot of every process and its parent. Answers taken from
/// one table agree with each other; answers from two `ps` runs need not
/// (a process can start or exit between them).
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    pairs: Vec<(u32, u32)>,
}

impl ProcessTable {
    /// Read the table now; empty where there is no `ps`.
    pub fn read() -> ProcessTable {
        ProcessTable {
            pairs: process_table(),
        }
    }

    /// The parent of `pid`, if `pid` is listed.
    pub fn parent(&self, pid: u32) -> Option<u32> {
        self.pairs
            .iter()
            .find(|&&(p, _)| p == pid)
            .map(|&(_, pp)| pp)
    }

    /// The direct children of `pid`.
    pub fn children(&self, pid: u32) -> Vec<u32> {
        self.pairs
            .iter()
            .filter(|&&(_, pp)| pp == pid)
            .map(|&(p, _)| p)
            .collect()
    }

    /// Every descendant of `root`, nearest first.
    pub fn descendants(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut frontier = vec![root];
        while let Some(p) = frontier.pop() {
            for &(pid, ppid) in &self.pairs {
                if ppid == p && pid != root && !out.contains(&pid) {
                    out.push(pid);
                    frontier.push(pid);
                }
            }
        }
        out
    }
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

    /// Tests that start processes, one at a time: one test lists every
    /// descendant of the test process, and another's children exiting
    /// meanwhile would make that list stale.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
    }

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
        let _one = serial();
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

    /// One table answers parent, children and descendants consistently.
    #[test]
    fn one_table_answers_every_question() {
        let _one = serial();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        let t = ProcessTable::read();
        let me = std::process::id();
        assert_eq!(t.parent(child.id()), Some(me));
        assert_eq!(t.parent(grandchild), Some(child.id()));
        assert!(t.children(me).contains(&child.id()));
        assert_eq!(t.children(child.id()), [grandchild]);
        let d = t.descendants(me);
        assert!(d.contains(&child.id()) && d.contains(&grandchild));
        assert_eq!(t.parent(u32::MAX), None);
        let _ = child.kill();
        let _ = child.wait();
        signal(grandchild, SIGKILL);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn wait_for(mut ok: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A background process the child starts is in the job, and ends with it.
    #[test]
    fn terminate_reaches_a_background_grandchild() {
        let job = Job::new(true).expect("job");
        let mut child = Command::new("cmd")
            .args([
                "/c",
                "start /b ping -n 30 127.0.0.1 >nul & ping -n 30 127.0.0.1 >nul",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(job.assign(&child));
        assert!(wait_for(|| job.active() >= 3), "active {}", job.active());
        assert!(job.terminate());
        let _ = child.wait();
        assert!(wait_for(|| job.active() == 0), "left {}", job.active());
    }

    /// Dropping a kill-on-close job ends what is left in it.
    #[test]
    fn drop_ends_what_is_left() {
        let job = Job::new(true).expect("job");
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        assert!(job.assign(&child));
        drop(job);
        let start = Instant::now();
        let _ = child.wait();
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn std_handles_stop_being_inherited() {
        // May be false when a std handle is a console or absent; it must not panic.
        let _ = stop_std_inheritance();
    }
}
