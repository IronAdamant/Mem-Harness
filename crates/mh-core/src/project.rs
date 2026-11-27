//! Project root discovery and command lookup.

use std::path::{Path, PathBuf};

/// The project root: `$MEM_HARNESS_PROJECT` (made absolute against
/// `start`; it must be a folder), else the nearest directory at or above
/// `start` holding `.git`, else `start`. Symlinks are resolved, so one
/// project has one root however it was reached. On Windows the root is in
/// its usual form (`C:\repo`), not the `\\?\C:\repo` form `canonicalize`
/// returns, which other tools and the model mishandle (see [`plain`]).
pub fn root_from(start: &Path) -> Result<PathBuf, String> {
    let start = canonical(start).unwrap_or_else(|_| start.to_path_buf());
    if let Ok(p) = std::env::var("MEM_HARNESS_PROJECT") {
        if !p.trim().is_empty() {
            let given = start.join(p.trim());
            return match canonical(&given) {
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

/// `canonicalize`, then on Windows [`plain`].
pub fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    p.canonicalize().map(plain)
}

/// Windows: `\\?\C:\x` as `C:\x`, and `\\?\UNC\srv\share\x` as
/// `\\srv\share\x`, when the short form names the same file: shorter than
/// the 260-character limit that form has, and no `.` or `..` component or
/// name ending in a dot or space (which only the `\\?\` form keeps).
/// Anything else, and every path off Windows, is returned unchanged.
pub fn plain(p: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return p;
    }
    let Some(s) = p.to_str() else {
        return p;
    };
    let short = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
            rest.to_string()
        } else {
            return p;
        }
    } else {
        return p;
    };
    let odd = short
        .split('\\')
        .filter(|c| !c.is_empty())
        .skip(1)
        .any(|c| c == "." || c == ".." || c.ends_with('.') || c.ends_with(' '));
    if short.len() >= 260 || odd {
        return p;
    }
    PathBuf::from(short)
}

/// The user's home folder: on Windows `USERPROFILE` (where cargo and
/// PowerShell's `~` look), then `HOME`; elsewhere `HOME`, then
/// `USERPROFILE`. Empty values are skipped.
pub fn home() -> Option<PathBuf> {
    let (first, second) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var_os(first)
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os(second).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
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
    if let Some(home) = home() {
        dirs.push(home.join(".cargo").join("bin"));
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
/// extension, but not `path` itself (Windows does not start a file with no
/// extension; it is usually a Git Bash shell script, and would fail with
/// os error 193); elsewhere just `path`.
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
    if !cfg!(windows) {
        out.push(path.to_path_buf());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_commands_resolve_against_the_base() {
        let dir = std::env::temp_dir().join(format!("mh-core-find-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        // On Windows only a name with a PATHEXT extension is a command.
        let file = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(dir.join("bin").join(file), "x").unwrap();
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
            Some(dir.join("bin").join(file))
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
        assert_eq!(got, plain(dir.canonicalize().unwrap()));
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            got.display()
        );
        assert!(same(&got, &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn same(a: &Path, b: &Path) -> bool {
        a.canonicalize().unwrap() == b.canonicalize().unwrap()
    }

    #[test]
    #[cfg(windows)]
    fn plain_drops_the_verbatim_prefix_only_when_safe() {
        let p = |s: &str| plain(PathBuf::from(s)).display().to_string();
        assert_eq!(p(r"\\?\C:\Users\x"), r"C:\Users\x");
        assert_eq!(p(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(p(r"C:\already"), r"C:\already");
        // Only the verbatim form can name these; keep it.
        assert_eq!(p(r"\\?\C:\a\b."), r"\\?\C:\a\b.");
        assert_eq!(p(r"\\?\C:\a\..\b"), r"\\?\C:\a\..\b");
        assert_eq!(p(r"\\?\Volume{1234}\x"), r"\\?\Volume{1234}\x");
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(p(&long), long);
    }

    #[test]
    #[cfg(windows)]
    fn a_file_with_no_extension_is_not_a_windows_command() {
        let dir = std::env::temp_dir().join(format!("mh-core-noext-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("shim"), "#!/bin/sh\n").unwrap();
        assert_eq!(find_command("bin/shim", &dir), None);
        std::fs::write(dir.join("bin").join("shim.cmd"), "@echo off\r\n").unwrap();
        assert_eq!(
            find_command("bin/shim", &dir),
            Some(dir.join("bin").join("shim.cmd"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
