//! The stores the harness's tools write into, and creating the missing ones.
//!
//! Libellus and Adsertio expose no MCP tool that creates their store, so an
//! agent in a fresh project could not write a note or a claim without a
//! shell command. The harness owns setup: the brief names missing stores and
//! `harness_init` creates them in one call. An existing store is never touched.

use crate::run::CmdOut;
use std::path::Path;
use std::time::Duration;

/// (tool, store folder, env var that moves it, CLI init argv).
pub const STORES: &[(&str, &str, &str, &[&str])] = &[
    ("relatum", ".swarmnotes", "SWARMNOTES_DIR", &["init"]),
    ("libellus", ".libellus", "LIBELLUS_DIR", &["init"]),
    ("adsertio", ".adsertio", "ADSERTIO_DIR", &["init"]),
];

/// A store exists when its env var names a folder, or the folder is at or
/// above `project` (each tool walks up the same way).
pub fn exists(project: &Path, dir: &str, env: &str) -> bool {
    if let Ok(p) = std::env::var(env) {
        if !p.trim().is_empty() {
            return Path::new(&p).is_dir();
        }
    }
    project.ancestors().any(|a| a.join(dir).is_dir())
}

pub fn missing(project: &Path) -> Vec<&'static str> {
    STORES
        .iter()
        .filter(|(_, dir, env, _)| !exists(project, dir, env))
        .map(|(tool, ..)| *tool)
        .collect()
}

/// Create each missing store with the tool's own `init`. Returns one
/// `(tool, outcome)` per store: `exists`, `created`, or `failed: <reason>`.
pub fn init<F>(project: &Path, run: &F) -> Vec<(&'static str, String)>
where
    F: Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + ?Sized,
{
    STORES
        .iter()
        .map(|(tool, dir, env, argv)| {
            if exists(project, dir, env) {
                return (*tool, "exists".to_string());
            }
            let outcome = match run(tool, argv, project, Duration::from_secs(30)) {
                Ok(o) if o.code == Some(0) && exists(project, dir, env) => "created".to_string(),
                Ok(o) => {
                    let why = o
                        .stderr
                        .lines()
                        .chain(o.stdout.lines())
                        .find(|l| !l.trim().is_empty());
                    format!("failed: {}", why.unwrap_or("no output").trim())
                }
                Err(e) => format!("failed: {e}"),
            };
            (*tool, outcome)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mh-stores-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn creates_only_missing_stores() {
        let p = tmp();
        std::fs::create_dir(p.join(".swarmnotes")).unwrap();
        if STORES
            .iter()
            .any(|(_, _, env, _)| std::env::var_os(env).is_some())
        {
            return; // the environment points the tools elsewhere
        }
        assert_eq!(missing(&p), ["libellus", "adsertio"]);
        let calls = Mutex::new(Vec::new());
        let run = |prog: &str, _: &[&str], cwd: &Path, _: Duration| -> Result<CmdOut, String> {
            calls.lock().unwrap().push(prog.to_string());
            if prog == "adsertio" {
                return Ok(CmdOut {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "error: disk full\n".into(),
                });
            }
            std::fs::create_dir(cwd.join(format!(".{prog}"))).unwrap();
            Ok(CmdOut {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        };
        let out = init(&p, &run);
        assert_eq!(*calls.lock().unwrap(), ["libellus", "adsertio"]);
        assert_eq!(out[0], ("relatum", "exists".to_string()));
        assert_eq!(out[1], ("libellus", "created".to_string()));
        assert_eq!(out[2], ("adsertio", "failed: error: disk full".to_string()));
        // A store above the project counts, as each tool walks up.
        let child = p.join("sub");
        std::fs::create_dir(&child).unwrap();
        assert_eq!(missing(&child), ["adsertio"]);
    }
}
