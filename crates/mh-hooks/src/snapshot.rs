//! Seeing a turn's file changes through git, shell edits included.
//!
//! The transcript shows which tools ran, not what they changed: `sed -i`
//! run through Bash edits a file as surely as the Edit tool. So the hooks
//! keep a snapshot of the working tree's state as git sees it: `HEAD`,
//! every entry of `git status --porcelain=v1 -z --untracked-files=normal`,
//! and each listed path's size and modification time, hashed. The baseline
//! is written when a turn starts (UserPromptSubmit for the main agent,
//! SubagentStart for a sub-agent) and again after each Stop check, so one
//! set of changes is asked about once.
//!
//! The file is `<temp>/mem-harness/<session_id>[-<agent_id>].snap`, names
//! cleaned to `[A-Za-z0-9_-]`. It is written to a fresh temporary file and
//! renamed into place, so a planted symlink is replaced, never followed.
//! Not seen: ignored files, files outside the repository, and an edit
//! inside an untracked folder that git lists only as `dir/` (only the
//! folder's own time is read). Outside a git repository, or when git
//! fails or times out, there is no snapshot and the stop check falls back
//! to the tool names in the transcript.

use crate::run::CmdOut;
use mh_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Runner<'a> = dyn Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + Sync + 'a;

/// Snapshot files older than this are removed when a new one is written.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// The longest file-name part kept from a session or agent id.
const MAX_KEY: usize = 100;

fn clean(s: &str) -> String {
    s.chars()
        .take(MAX_KEY)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn dir() -> PathBuf {
    std::env::temp_dir().join("mem-harness")
}

/// The snapshot file of a session (`agent` empty) or one of its sub-agents.
/// `None` without a session id.
pub fn path_for(session: &str, agent: &str) -> Option<PathBuf> {
    let session = session.trim();
    if session.is_empty() {
        return None;
    }
    let agent = agent.trim();
    let name = if agent.is_empty() {
        clean(session)
    } else {
        format!("{}-{}", clean(session), clean(agent))
    };
    Some(dir().join(format!("{name}.snap")))
}

/// The snapshot file named by hook stdin: `session_id`, plus `agent_id`
/// for a sub-agent's events. `None` when an id it needs is missing.
pub fn path_from(v: &Value, subagent: bool) -> Option<PathBuf> {
    let field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim();
    if subagent {
        let agent = field("agent_id");
        if agent.is_empty() {
            return None;
        }
        path_for(field("session_id"), agent)
    } else {
        path_for(field("session_id"), "")
    }
}

/// FNV-1a, 64 bits: enough to tell two states of one tree apart.
fn hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The paths in `git status --porcelain=v1 -z` output: one per entry, and
/// a rename's or copy's source too (the next NUL-separated field).
fn status_paths(out: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        let Some(path) = entry.get(3..) else {
            continue;
        };
        paths.push(path);
        if entry.starts_with('R') || entry.starts_with('C') {
            if let Some(from) = fields.next() {
                paths.push(from);
            }
        }
    }
    paths
}

/// The working tree's state as git sees it, hashed; `None` outside a git
/// repository or when git fails. All git calls share `timeout`.
pub fn take(project: &Path, timeout: Duration, run: &Runner) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let git = |args: &[&str]| -> Option<CmdOut> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        // No optional locks: a status run must not take the index lock
        // from under the agent's own git commands.
        let mut all = vec!["--no-optional-locks"];
        all.extend_from_slice(args);
        run("git", &all, project, left).ok()
    };
    let status = git(&["status", "--porcelain=v1", "-z", "--untracked-files=normal"])?;
    if status.code != Some(0) {
        return None;
    }
    let top = git(&["rev-parse", "--show-toplevel"]).filter(|o| o.code == Some(0))?;
    let top = PathBuf::from(top.stdout.trim_end_matches(['\n', '\r']));
    // An unborn branch has no HEAD; that is a state too.
    let head = git(&["rev-parse", "-q", "--verify", "HEAD"])?;
    let mut text = format!("{}\0{}\0", head.code.unwrap_or(-1), head.stdout.trim());
    text.push_str(&status.stdout);
    for p in status_paths(&status.stdout) {
        let stamp = match std::fs::symlink_metadata(top.join(p)) {
            Ok(m) => {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{} {t}", m.len())
            }
            Err(_) => "gone".to_string(),
        };
        text.push('\0');
        text.push_str(p);
        text.push('\0');
        text.push_str(&stamp);
    }
    Some(hash(text.as_bytes()))
}

/// The snapshot saved at `path`, if any.
pub fn load(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty() && s.len() <= 64).then(|| s.to_string())
}

#[cfg(unix)]
fn make_dir(d: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(d)
}

#[cfg(not(unix))]
fn make_dir(d: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(d)
}

/// Save `snap` at `path`: written to a new file, then renamed over the old
/// one. Old snapshots in the folder are removed. Errors are ignored: no
/// baseline only means the next check falls back to tool names.
pub fn save(path: &Path, snap: &str) {
    let Some(d) = path.parent() else {
        return;
    };
    if make_dir(d).is_err() {
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.subsec_nanos());
    let tmp = path.with_extension(format!("{}-{nanos}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(snap.as_bytes()));
    if written.is_err() || rename_retrying(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    prune(d);
}

/// `fs::rename`, retried for up to a second while the target is busy. On
/// Windows a virus scanner or indexer opens a freshly written file without
/// letting it be replaced, and the rename fails with "access denied" (os
/// error 5) or a sharing violation (32) until it lets go; CI machines do
/// not scan files, so this shows only on real ones.
fn rename_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut wait = std::time::Duration::from_millis(5);
    loop {
        match std::fs::rename(from, to) {
            Err(e)
                if cfg!(windows)
                    && (e.kind() == std::io::ErrorKind::PermissionDenied
                        || e.raw_os_error() == Some(32))
                    && std::time::Instant::now() + wait < until =>
            {
                std::thread::sleep(wait);
                wait = (wait * 2).min(std::time::Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

fn prune(d: &Path) {
    let Ok(entries) = std::fs::read_dir(d) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".snap") || name.ends_with(".tmp")) {
            continue;
        }
        let old = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Take a snapshot of `project` and save it as the baseline named by hook
/// stdin (see [`path_from`]). Does nothing without the ids or outside git.
pub fn record(project: &Path, stdin: &str, subagent: bool, timeout: Duration, run: &Runner) {
    let Ok(v) = mh_json::parse(stdin.trim()) else {
        return;
    };
    let Some(path) = path_from(&v, subagent) else {
        return;
    };
    if let Some(snap) = take(project, timeout, run) {
        save(&path, &snap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        let p = path_for("../../etc/x y", "a/b").unwrap();
        let name = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "______etc_x_y-a_b.snap");
        assert_eq!(p.parent().unwrap(), dir());
        assert!(path_for(" ", "").is_none());
        let v = mh_json::parse(r#"{"session_id":"s1","agent_id":"a1"}"#).unwrap();
        assert!(path_from(&v, true).unwrap().ends_with("s1-a1.snap"));
        assert!(path_from(&v, false).unwrap().ends_with("s1.snap"));
        let v = mh_json::parse(r#"{"session_id":"s1"}"#).unwrap();
        assert!(path_from(&v, true).is_none());
        assert_eq!(
            path_for(&"x".repeat(500), "")
                .unwrap()
                .file_name()
                .unwrap()
                .len(),
            MAX_KEY + 5
        );
    }

    #[test]
    fn porcelain_paths() {
        let out = " M src/a.rs\0R  new.rs\0old.rs\0?? dir/\0";
        assert_eq!(status_paths(out), ["src/a.rs", "new.rs", "old.rs", "dir/"]);
        assert!(status_paths("").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn save_replaces_a_planted_symlink() {
        let path = path_for(&format!("mh-test-link-{}", std::process::id()), "").unwrap();
        make_dir(path.parent().unwrap()).unwrap();
        let victim = std::env::temp_dir().join(format!("mh-snap-victim-{}", std::process::id()));
        std::fs::write(&victim, "keep").unwrap();
        let _ = std::fs::remove_file(&path);
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        save(&path, "abc");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(load(&path).as_deref(), Some("abc"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&victim);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// The snapshot is still replaced when another process (a virus
    /// scanner, say) holds it open for a moment without share-delete.
    #[test]
    fn save_waits_out_a_reader_that_blocks_replacing_the_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("mh-snap-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.snap");
        std::fs::write(&path, "old").unwrap();
        // FILE_SHARE_READ only: no FILE_SHARE_DELETE, so a rename over it fails.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });
        save(&path, "new");
        release.join().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
