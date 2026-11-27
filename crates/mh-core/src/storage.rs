//! Where the backends keep their stores: outside the project, one folder
//! per project.
//!
//! A backend that writes a store into the repository by default (stele,
//! chisel, trammel, coordinationhub) is pointed at a folder of its own by
//! its `storage` setting ([`crate::config::StorageSpec`]): an environment
//! variable or a command-line flag the tool already reads. The folder is
//! `<base>/<project id>/<backend>`, where the base depends on what the
//! store holds:
//!
//! - **cache** (an index that can be rebuilt from the code): the OS cache
//!   folder, `~/Library/Caches/mem-harness` on macOS, `%LOCALAPPDATA%`
//!   `\mem-harness\cache` on Windows, `$XDG_CACHE_HOME/mem-harness` (or
//!   `~/.cache/mem-harness`) elsewhere. Backups skip it, and the system may
//!   clear it.
//! - **data** (plans, test results, issues, locks: work that would be
//!   lost): `~/.mem-harness/projects`, which nothing clears.
//!
//! `MEM_HARNESS_CACHE_DIR` and `MEM_HARNESS_DATA_DIR` (absolute paths)
//! replace either base. The project id is the project folder's name
//! (cleaned) and a hash of its full path, so two checkouts never share a
//! store and the same checkout always finds its own. Each project folder
//! holds `project.json` with the path, so [`list`] can tell which projects
//! no longer exist, and [`move_project`] carries a store along after a
//! rename.
//!
//! As a safety net for a tool that ignores its setting, [`ensure_git_exclude`]
//! lists the store folder names in the repository's `.git/info/exclude`,
//! which is local to the clone and never committed.

use mh_json::Value;
use std::path::{Path, PathBuf};

/// What a store holds, which decides its base folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cache,
    Data,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "cache" => Some(Kind::Cache),
            "data" => Some(Kind::Data),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Cache => "cache",
            Kind::Data => "data",
        }
    }
}

/// The file in each project folder that names the project.
pub const PROJECT_FILE: &str = "project.json";

fn env_dir(var: &str) -> Option<PathBuf> {
    let v = std::env::var_os(var).filter(|v| !v.is_empty())?;
    let p = PathBuf::from(v);
    p.is_absolute().then_some(p)
}

/// The base folder for `kind`; `None` without a home folder.
pub fn base(kind: Kind) -> Option<PathBuf> {
    match kind {
        Kind::Data => env_dir("MEM_HARNESS_DATA_DIR").or_else(|| {
            Some(
                crate::project::home()?
                    .join(".mem-harness")
                    .join("projects"),
            )
        }),
        Kind::Cache => env_dir("MEM_HARNESS_CACHE_DIR").or_else(os_cache),
    }
}

fn os_cache() -> Option<PathBuf> {
    if cfg!(windows) {
        let local = env_dir("LOCALAPPDATA")
            .or_else(|| Some(crate::project::home()?.join("AppData").join("Local")))?;
        return Some(local.join("mem-harness").join("cache"));
    }
    if cfg!(target_os = "macos") {
        return Some(
            crate::project::home()?
                .join("Library")
                .join("Caches")
                .join("mem-harness"),
        );
    }
    let xdg = env_dir("XDG_CACHE_HOME").or_else(|| Some(crate::project::home()?.join(".cache")))?;
    Some(xdg.join("mem-harness"))
}

/// FNV-1a, 64 bits.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The id of the project at `root`: its folder name, cleaned to
/// `[A-Za-z0-9._-]` and cut to 40 characters, a `-`, and 16 hex digits
/// hashed from the full path (lower-cased on Windows, where case does not
/// matter).
pub fn project_id(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let name = name.trim_start_matches('.');
    let name = if name.is_empty() { "project" } else { name };
    let mut path = root.to_string_lossy().into_owned();
    if cfg!(windows) {
        path = path.to_lowercase();
    }
    format!("{name}-{:016x}", fnv(path.as_bytes()))
}

/// The folder of project `root` under `kind`'s base.
pub fn project_dir(kind: Kind, root: &Path) -> Option<PathBuf> {
    Some(base(kind)?.join(project_id(root)))
}

/// The store folder of `backend` for project `root`.
pub fn store_dir(kind: Kind, root: &Path, backend: &str) -> Option<PathBuf> {
    Some(project_dir(kind, root)?.join(backend))
}

/// Create the project folder under `kind` (if missing) and write its
/// `project.json` when it is missing or names another path.
pub fn record(kind: Kind, root: &Path) -> Result<PathBuf, String> {
    let dir = project_dir(kind, root).ok_or("no home folder to keep stores in")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let file = dir.join(PROJECT_FILE);
    let want = root.to_string_lossy().into_owned();
    if read_path(&file).as_deref() != Some(want.as_str()) {
        let body = Value::obj(vec![("path", Value::str(want))]).to_json() + "\n";
        std::fs::write(&file, body).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    }
    Ok(dir)
}

fn read_path(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let v = mh_json::parse(text.trim()).ok()?;
    v.get("path").and_then(Value::as_str).map(str::to_string)
}

/// One project folder found under a base.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    pub id: String,
    pub dir: PathBuf,
    /// The project path from `project.json`; empty when it is missing.
    pub path: String,
    /// The project path is still a folder.
    pub exists: bool,
    /// Bytes under the folder.
    pub bytes: u64,
}

fn size_of(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| size_of(&e.path())).sum())
        .unwrap_or(0)
}

/// Every project folder under both bases, sorted by kind then id.
pub fn list() -> Vec<Entry> {
    let mut out = Vec::new();
    for kind in [Kind::Data, Kind::Cache] {
        let Some(b) = base(kind) else { continue };
        let Ok(rd) = std::fs::read_dir(&b) else {
            continue;
        };
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let path = read_path(&dir.join(PROJECT_FILE)).unwrap_or_default();
            out.push(Entry {
                kind,
                id: e.file_name().to_string_lossy().into_owned(),
                exists: !path.is_empty() && Path::new(&path).is_dir(),
                bytes: size_of(&dir),
                path,
                dir,
            });
        }
    }
    out.sort_by(|a, b| (a.kind.as_str(), &a.id).cmp(&(b.kind.as_str(), &b.id)));
    out
}

/// The entries that `what` names: a project id, or a project path (as
/// given, and canonicalised when it still exists).
pub fn find(what: &str) -> Vec<Entry> {
    let canon = Path::new(what)
        .canonicalize()
        .ok()
        .map(|p| crate::project::plain(p).to_string_lossy().into_owned());
    let id_of_path = project_id(Path::new(canon.as_deref().unwrap_or(what)));
    list()
        .into_iter()
        .filter(|e| {
            e.id == what || e.id == id_of_path || e.path == what || Some(&e.path) == canon.as_ref()
        })
        .collect()
}

/// Move the stores of the project once at `old` to the project now at
/// `new` (an existing folder), under both bases. Returns one line per base
/// moved. Refuses when `new` already has stores, rather than mixing them.
pub fn move_project(old: &str, new: &Path) -> Result<Vec<String>, String> {
    let new = new
        .canonicalize()
        .map_err(|e| format!("{}: {e}", new.display()))?;
    let new = crate::project::plain(new);
    let from = find(old);
    if from.is_empty() {
        return Err(format!("no stores found for {old}"));
    }
    // Check every target before moving anything, so a refusal leaves
    // both projects as they were.
    let mut plan = Vec::new();
    for e in from {
        let to = project_dir(e.kind, &new).ok_or("no home folder")?;
        if to == e.dir {
            continue;
        }
        if to.exists() {
            return Err(format!(
                "{} already has {} stores at {}; remove them first (mem-harness projects forget)",
                new.display(),
                e.kind.as_str(),
                to.display()
            ));
        }
        plan.push((e, to));
    }
    let mut done = Vec::new();
    for (e, to) in plan {
        std::fs::rename(&e.dir, &to)
            .map_err(|err| format!("cannot move {} to {}: {err}", e.dir.display(), to.display()))?;
        record(e.kind, &new)?;
        done.push(format!(
            "{}: {} -> {}",
            e.kind.as_str(),
            e.dir.display(),
            to.display()
        ));
    }
    Ok(done)
}

/// Delete the stores `what` names (see [`find`]). Returns the folders removed.
pub fn forget(what: &str) -> Result<Vec<PathBuf>, String> {
    let found = find(what);
    if found.is_empty() {
        return Err(format!("no stores found for {what}"));
    }
    let mut out = Vec::new();
    for e in found {
        std::fs::remove_dir_all(&e.dir)
            .map_err(|err| format!("cannot remove {}: {err}", e.dir.display()))?;
        out.push(e.dir);
    }
    Ok(out)
}

const EXCLUDE_BEGIN: &str =
    "# >>> mem-harness: tool stores (managed by mem-harness; edits here are replaced)";
const EXCLUDE_END: &str = "# <<< mem-harness";

/// The git folder shared by every worktree of the repository at or above
/// `root` (`.git`, or the common folder a worktree's `.git` file points
/// to). Read from files only; `None` outside git.
pub fn git_common_dir(root: &Path) -> Option<PathBuf> {
    for dir in root.ancestors() {
        let dot = dir.join(".git");
        let Ok(m) = std::fs::metadata(&dot) else {
            continue;
        };
        if m.is_dir() {
            return Some(dot);
        }
        // A worktree or submodule: `gitdir: <path>`, and in that folder a
        // `commondir` file naming the shared one (relative to it).
        let text = std::fs::read_to_string(&dot).ok()?;
        let git = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
        let git = dir.join(git);
        return Some(match std::fs::read_to_string(git.join("commondir")) {
            Ok(c) => git.join(c.trim()),
            Err(_) => git,
        });
    }
    None
}

/// Make sure `<git common dir>/info/exclude` lists `names` (as `name/`
/// lines, matching at any depth) in a block marked as the harness's. The
/// block is rewritten only when it differs; lines outside it are kept as
/// they are. Returns whether the file changed; `Ok(false)` outside git.
pub fn ensure_git_exclude(root: &Path, names: &[String]) -> Result<bool, String> {
    let Some(git) = git_common_dir(root) else {
        return Ok(false);
    };
    let file = git.join("info").join("exclude");
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    let mut kept: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in old.lines() {
        if line == EXCLUDE_BEGIN {
            inside = true;
            continue;
        }
        if inside {
            if line == EXCLUDE_END {
                inside = false;
            }
            continue;
        }
        kept.push(line);
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    let mut new = kept.join("\n");
    if !new.is_empty() {
        new.push_str("\n\n");
    }
    if !names.is_empty() {
        new.push_str(EXCLUDE_BEGIN);
        new.push('\n');
        for n in names {
            new.push_str(n.trim_end_matches('/'));
            new.push_str("/\n");
        }
        new.push_str(EXCLUDE_END);
        new.push('\n');
    }
    if new == old {
        return Ok(false);
    }
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    std::fs::write(&file, new).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mh-storage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn ids_are_stable_readable_and_distinct() {
        let a = project_id(Path::new("/work/my repo"));
        assert!(
            a.starts_with("my_repo-") && a.len() == "my_repo-".len() + 16,
            "{a}"
        );
        assert_eq!(a, project_id(Path::new("/work/my repo")));
        assert_ne!(a, project_id(Path::new("/other/my repo")));
        assert!(project_id(Path::new("/x/.hidden")).starts_with("hidden-"));
        assert!(project_id(Path::new("/")).starts_with("project-"));
        let long = project_id(Path::new(&format!("/x/{}", "a".repeat(100))));
        assert_eq!(long.len(), 40 + 1 + 16);
    }

    #[test]
    fn exclude_block_is_added_once_and_replaced() {
        let d = tmp("exclude");
        std::fs::create_dir_all(d.join(".git").join("info")).unwrap();
        let file = d.join(".git/info/exclude");
        std::fs::write(&file, "# user line\n*.log\n").unwrap();
        let names = vec![".chisel".to_string(), ".trammel/".to_string()];
        assert!(ensure_git_exclude(&d, &names).unwrap());
        assert!(
            !ensure_git_exclude(&d, &names).unwrap(),
            "rewritten unchanged"
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# user line\n*.log\n\n"), "{text}");
        assert!(text.contains("\n.chisel/\n.trammel/\n"), "{text}");
        assert!(ensure_git_exclude(&d, &[".stele-context".to_string()]).unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains(".chisel/") && text.contains(".stele-context/"),
            "{text}"
        );
        assert_eq!(text.matches(EXCLUDE_BEGIN).count(), 1);
        // A subfolder of the repository finds the same file.
        std::fs::create_dir_all(d.join("sub")).unwrap();
        assert!(!ensure_git_exclude(&d.join("sub"), &[".stele-context".to_string()]).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_worktree_uses_the_common_git_folder() {
        let d = tmp("worktree");
        let common = d.join("main/.git");
        let wt_git = common.join("worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = d.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let got = git_common_dir(&wt).unwrap().canonicalize().unwrap();
        assert_eq!(got, common.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_git_no_exclude() {
        let d = tmp("nogit");
        // Only meaningful when the temp folder is not inside a repository.
        if git_common_dir(&d).is_none() {
            assert!(!ensure_git_exclude(&d, &[".x".to_string()]).unwrap());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
