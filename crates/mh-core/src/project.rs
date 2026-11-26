//! Project root discovery and command lookup.

use std::path::{Path, PathBuf};

/// The project root: `$MEM_HARNESS_PROJECT` (made absolute against
/// `start`; it must be a folder), else the nearest directory at or above
/// `start` holding `.git`, else `start`. Symlinks are resolved, so one
/// project has one root however it was reached.
pub fn root_from(start: &Path) -> Result<PathBuf, String> {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    if let Ok(p) = std::env::var("MEM_HARNESS_PROJECT") {
        if !p.trim().is_empty() {
            let given = start.join(p.trim());
            return match given.canonicalize() {
                Ok(c) if c.is_dir() => Ok(c),
                Ok(c) => Err(format!(
                    "MEM_HARNESS_PROJECT {} is not a folder",
                    c.display()
                )),
                Err(e) => Err(format!("MEM_HARNESS_PROJECT {}: {e}", given.display())),
            };
        }
    }
    let mut dir = Some(start.as_path());
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Ok(d.to_path_buf());
        }
        dir = d.parent();
    }
    Ok(start)
}

pub fn root() -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the current folder: {e}"))?;
    root_from(&cwd)
}

/// Find a program. A path with a folder in it (`bin/tool`, `/usr/bin/x`)
/// is taken relative to `base` (the project root), so it names the same
/// file the child is started with. A bare name is looked up in each `PATH`
/// entry, then `~/.cargo/bin` (where the suite installs), so the harness
/// works even when a host starts it with a short `PATH`. On Windows each
/// `PATHEXT` extension is tried first, so `.exe` and `.cmd` shims are found.
/// The result is absolute when `base` is.
pub fn find_command(cmd: &str, base: &Path) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.components().count() > 1 {
        let full = base.join(p);
        return candidates(&full).into_iter().find(|c| runnable(c));
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        dirs.push(Path::new(&home).join(".cargo").join("bin"));
    }
    dirs.iter()
        .filter(|d| d.is_absolute())
        .flat_map(|d| candidates(&d.join(cmd)))
        .find(|c| runnable(c))
}

/// A file the OS would start: on Unix it needs an execute bit, so a
/// non-executable file earlier on `PATH` is skipped, as `execvp` does.
fn runnable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The files `path` may mean: on Windows `path` plus each `PATHEXT`
/// extension, then `path` itself; elsewhere just `path`.
fn candidates(path: &Path) -> Vec<PathBuf> {
    if !cfg!(windows) || path.extension().is_some() {
        return vec![path.to_path_buf()];
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let mut out: Vec<PathBuf> = exts
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| {
            let mut s = path.as_os_str().to_os_string();
            s.push(e.to_ascii_lowercase());
            PathBuf::from(s)
        })
        .collect();
    out.push(path.to_path_buf());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_commands_resolve_against_the_base() {
        let dir = std::env::temp_dir().join(format!("mh-core-find-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("tool"), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let t = dir.join("bin").join("tool");
            std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.join("bin").join("plain"), "x").unwrap();
            assert_eq!(find_command("bin/plain", &dir), None, "not executable");
        }
        assert_eq!(
            find_command("bin/tool", &dir),
            Some(dir.join("bin").join("tool"))
        );
        assert_eq!(find_command("bin/none", &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_is_canonical_and_git_bounded() {
        let dir = std::env::temp_dir().join(format!("mh-core-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("a").join("b")).unwrap();
        let got = root_from(&dir.join("a").join("b")).unwrap();
        assert_eq!(got, dir.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
