//! The stores the harness's tools write into, and creating the missing ones.
//!
//! Libellus and Adsertio expose no MCP tool that creates their store, so an
//! agent in a fresh project could not write a note or a claim without a
//! shell command. The harness owns setup: the brief names missing stores and
//! `harness_init` creates them in one call. An existing store is never
//! touched. Which stores exist, where they live and how each is created come
//! from `Config.stores`, so a new tool needs no code here.

use crate::run::CmdOut;
use mh_core::config::StoreSpec;
use std::path::Path;
use std::time::Duration;

/// Where a store stands in a project.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// A real folder.
    Present,
    /// Nothing there; `init` creates it.
    Absent,
    /// Something is there that the tool refuses (a symlink or a file); the
    /// reason says what. `init` cannot fix it, and nothing is written.
    Refused(String),
}

fn at(path: &Path, st: &StoreSpec) -> Option<State> {
    let m = std::fs::symlink_metadata(path).ok()?;
    Some(if m.is_dir() {
        State::Present
    } else {
        let what = if m.file_type().is_symlink() {
            "a symlink"
        } else {
            "not a folder"
        };
        State::Refused(format!(
            "{} is {what}; {} refuses it. Replace it with a real folder{}",
            path.display(),
            st.tool,
            if st.env.is_empty() {
                String::new()
            } else {
                format!(" or set {}", st.env)
            }
        ))
    })
}

/// A store is the folder its env var names, else the nearest `dir` at or
/// above `project` (each tool walks up the same way and stops at the first
/// entry it finds, refusing a symlink or file there).
pub fn state(project: &Path, st: &StoreSpec) -> State {
    if !st.env.is_empty() {
        if let Ok(p) = std::env::var(&st.env) {
            if !p.trim().is_empty() {
                return at(&project.join(p), st).unwrap_or(State::Absent);
            }
        }
    }
    project
        .ancestors()
        .find_map(|a| at(&a.join(&st.dir), st))
        .unwrap_or(State::Absent)
}

pub fn exists(project: &Path, st: &StoreSpec) -> bool {
    state(project, st) == State::Present
}

/// The stores that are not usable, with their state (`Absent` or `Refused`).
pub fn missing<'a>(stores: &'a [StoreSpec], project: &Path) -> Vec<(&'a str, State)> {
    stores
        .iter()
        .map(|st| (st.tool.as_str(), state(project, st)))
        .filter(|(_, s)| *s != State::Present)
        .collect()
}

/// Create each missing store with its `init` command. Returns one
/// `(tool, outcome)` per store: `exists`, `created`, or `failed: <reason>`.
pub fn init<'a, F>(stores: &'a [StoreSpec], project: &Path, run: &F) -> Vec<(&'a str, String)>
where
    F: Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + ?Sized,
{
    stores
        .iter()
        .map(|st| {
            let tool = st.tool.as_str();
            match state(project, st) {
                State::Present => return (tool, "exists".to_string()),
                State::Refused(why) => return (tool, format!("failed: {why}")),
                State::Absent => {}
            }
            let Some((prog, args)) = st.init.split_first() else {
                return (tool, "failed: no init command configured".to_string());
            };
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let outcome = match run(prog, &args, project, Duration::from_secs(30)) {
                Ok(o) if o.code == Some(0) && exists(project, st) => "created".to_string(),
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
            (tool, outcome)
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

    /// Stores with env vars no test environment sets.
    fn specs() -> Vec<StoreSpec> {
        let s = |tool: &str, dir: &str| StoreSpec {
            tool: tool.into(),
            dir: dir.into(),
            env: format!("MH_TEST_UNSET_{}_DIR", tool.to_uppercase()),
            init: vec![format!("{tool}-cli"), "init".into()],
        };
        vec![
            s("relatum", ".swarmnotes"),
            s("libellus", ".libellus"),
            s("adsertio", ".adsertio"),
        ]
    }

    fn names(m: Vec<(&str, State)>) -> Vec<String> {
        m.into_iter().map(|(t, _)| t.to_string()).collect()
    }

    #[test]
    fn creates_only_missing_stores() {
        let p = tmp();
        let stores = specs();
        std::fs::create_dir(p.join(".swarmnotes")).unwrap();
        assert_eq!(names(missing(&stores, &p)), ["libellus", "adsertio"]);
        let calls = Mutex::new(Vec::new());
        let run = |prog: &str, a: &[&str], cwd: &Path, _: Duration| -> Result<CmdOut, String> {
            calls
                .lock()
                .unwrap()
                .push(format!("{prog} {}", a.join(" ")));
            if prog == "adsertio-cli" {
                return Ok(CmdOut {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "error: disk full\n".into(),
                });
            }
            let tool = prog.trim_end_matches("-cli");
            std::fs::create_dir(cwd.join(format!(".{tool}"))).unwrap();
            Ok(CmdOut {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        };
        let out = init(&stores, &p, &run);
        assert_eq!(
            *calls.lock().unwrap(),
            ["libellus-cli init", "adsertio-cli init"]
        );
        assert_eq!(out[0], ("relatum", "exists".to_string()));
        assert_eq!(out[1], ("libellus", "created".to_string()));
        assert_eq!(out[2], ("adsertio", "failed: error: disk full".to_string()));
        // A store above the project counts, as each tool walks up.
        let child = p.join("sub");
        std::fs::create_dir(&child).unwrap();
        assert_eq!(names(missing(&stores, &child)), ["adsertio"]);
    }

    #[test]
    fn env_var_names_the_store() {
        let p = tmp();
        let mut stores = specs();
        stores.truncate(1);
        stores[0].env = "MH_TEST_STORES_ENV_DIR".into();
        std::env::set_var("MH_TEST_STORES_ENV_DIR", p.join("elsewhere"));
        assert_eq!(state(&p, &stores[0]), State::Absent);
        std::fs::create_dir(p.join("elsewhere")).unwrap();
        assert_eq!(state(&p, &stores[0]), State::Present);
        std::env::remove_var("MH_TEST_STORES_ENV_DIR");
    }

    /// Libellus refuses a symlinked store (and will not walk past it), so
    /// a symlink is not a store, and init does not run over it.
    #[cfg(unix)]
    #[test]
    fn symlinked_store_is_refused_not_present() {
        let p = tmp();
        let stores = specs();
        let real = p.join("real");
        std::fs::create_dir(&real).unwrap();
        let proj = p.join("proj");
        std::fs::create_dir(&proj).unwrap();
        std::os::unix::fs::symlink(&real, proj.join(".libellus")).unwrap();
        // A real store above does not count: the tool stops at the symlink.
        std::fs::create_dir(p.join(".libellus")).unwrap();
        let st = state(&proj, &stores[1]);
        let State::Refused(why) = &st else {
            panic!("{st:?}")
        };
        assert!(why.contains("symlink") && why.contains("libellus"), "{why}");
        assert!(!exists(&proj, &stores[1]));
        let run = |_: &str, _: &[&str], _: &Path, _: Duration| -> Result<CmdOut, String> {
            panic!("init must not run over a refused store")
        };
        let only = &stores[1..2];
        let out = init(only, &proj, &run);
        assert!(out[0].1.starts_with("failed: ") && out[0].1.contains("symlink"));
    }
}
