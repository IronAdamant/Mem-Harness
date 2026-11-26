//! The stores the harness's tools write into, and creating the missing ones.
//!
//! Libellus and Adsertio expose no MCP tool that creates their store, so an
//! agent in a fresh project could not write a note or a claim without a
//! shell command. The harness owns setup: the brief names missing stores and
//! `harness_init` creates them in one call. An existing store is never
//! touched. Which stores exist, where they live and how each is created come
//! from `Config.stores`, so a new tool needs no code here, unless it treats a
//! symlinked or file store differently from Relatum (see `Links`).

use crate::run::CmdOut;
use mh_core::config::StoreSpec;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a store stands in a project.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// A folder the tool uses.
    Present,
    /// Nothing there; `init` creates it.
    Absent,
    /// Something is there that the tool refuses (a symlink or a file); the
    /// reason says what. `init` cannot fix it, and nothing is written.
    Refused(String),
}

/// How a tool treats a store path that is a symlink or a file. Each tool
/// finds its store its own way, and the harness must agree with it, or the
/// brief calls a working store refused (or offers to create one the tool
/// will not use). `StoreSpec` has no field for this, so the rules are kept
/// here, keyed by tool name; a tool not listed is read as Relatum reads its
/// store (`Links::Follow`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Links {
    /// Libellus: the walk up stops at the first entry of that name, and a
    /// symlink or file there (or at the env var's path) is refused.
    Refuse,
    /// Adsertio: the walk up passes a symlink or file and takes the first
    /// real folder; the env var's path must be a real folder.
    Skip,
    /// Relatum: a symlink to a folder is a store (followed); a file is
    /// walked past. One lookup, `mh_adapters::relatum::store_dir_in`, which
    /// the brief's rules section uses too.
    Follow,
}

fn links(tool: &str) -> Links {
    match tool {
        "libellus" => Links::Refuse,
        "adsertio" => Links::Skip,
        _ => Links::Follow,
    }
}

fn refusal(path: &Path, m: &std::fs::Metadata, st: &StoreSpec) -> String {
    let what = if m.file_type().is_symlink() {
        "a symlink"
    } else {
        "not a folder"
    };
    format!(
        "{} is {what}; {} refuses it. Replace it with a real folder{}",
        path.display(),
        st.tool,
        if st.env.is_empty() {
            String::new()
        } else {
            format!(" or set {}", st.env)
        }
    )
}

/// The env var's path (relative to `project`), when it is set and not blank.
fn env_path(project: &Path, st: &StoreSpec) -> Option<PathBuf> {
    if st.env.is_empty() {
        return None;
    }
    let v = std::env::var_os(&st.env)?;
    if v.is_empty() || v.to_str().is_some_and(|s| s.trim().is_empty()) {
        return None;
    }
    Some(project.join(v))
}

/// Find the store the way its tool does: the folder its env var names,
/// else a `dir` at or above `project` (see `Links`). `Ok(Some(path))` is
/// a store the tool uses, `Ok(None)` none, `Err` one it refuses.
pub fn locate(project: &Path, st: &StoreSpec) -> Result<Option<PathBuf>, String> {
    let rule = links(&st.tool);
    if rule == Links::Follow {
        return Ok(mh_adapters::relatum::store_dir_in(
            project, &st.dir, &st.env,
        ));
    }
    let real = |p: &Path| -> Result<Option<PathBuf>, String> {
        match std::fs::symlink_metadata(p) {
            Err(_) => Ok(None),
            Ok(m) if m.is_dir() => Ok(Some(p.to_path_buf())),
            Ok(m) => Err(refusal(p, &m, st)),
        }
    };
    if let Some(p) = env_path(project, st) {
        return real(&p);
    }
    for a in project.ancestors() {
        match real(&a.join(&st.dir)) {
            Ok(Some(p)) => return Ok(Some(p)),
            Err(e) if rule == Links::Refuse => return Err(e),
            _ => {}
        }
    }
    Ok(None)
}

/// Where the store `st` stands for `project`; see [`locate`].
pub fn state(project: &Path, st: &StoreSpec) -> State {
    match locate(project, st) {
        Ok(Some(_)) => State::Present,
        Ok(None) => State::Absent,
        Err(why) => State::Refused(why),
    }
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
            save_hint: String::new(),
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
