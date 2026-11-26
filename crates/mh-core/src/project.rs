//! Project root discovery and command lookup.

use std::path::{Path, PathBuf};

/// The project root: `$MEM_HARNESS_PROJECT`, else the nearest directory
/// at or above `start` holding `.git`, else `start`.
pub fn root_from(start: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("MEM_HARNESS_PROJECT") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    start.to_path_buf()
}

pub fn root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    root_from(&cwd)
}

/// Find a program: an absolute or relative path as given, else each `PATH`
/// entry, else `~/.cargo/bin` (where the suite installs), so the harness
/// works even when a host starts it with a short `PATH`.
pub fn find_command(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exe = |dir: &Path| -> Option<PathBuf> {
        let c = dir.join(cmd);
        if c.is_file() {
            return Some(c);
        }
        if cfg!(windows) {
            let c = dir.join(format!("{cmd}.exe"));
            if c.is_file() {
                return Some(c);
            }
        }
        None
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if let Some(c) = exe(&dir) {
                return Some(c);
            }
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    exe(&Path::new(&home).join(".cargo").join("bin"))
}
