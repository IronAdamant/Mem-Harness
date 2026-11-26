//! Native reader of a Relatum store (`.swarmnotes/`), and the `relatum:`
//! evidence pointer. Formats follow Relatum SPEC.md sections 1 to 3 and
//! `Relatum/src/{grammar,meta,store}.rs`.

use std::fs;
use std::path::{Path, PathBuf};

const DIR_NAME: &str = ".swarmnotes";
const RULE_KINDS: &[&str] = &["constraint", "convention", "decision"];
const STATUSES: &[&str] = &["active", "draft", "superseded"];
const MAX_NOTE_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: String,
    pub kind: String,
    pub summary: String,
    /// `active`, `draft` or `superseded`.
    pub status: String,
    pub superseded_by: Option<String>,
    pub tags: Vec<String>,
    pub author: Option<String>,
    pub updated: Option<String>,
    pub body: String,
}

/// An id segment: `_?[a-z0-9][a-z0-9-]*`.
fn valid_segment(seg: &str) -> bool {
    let b = seg.as_bytes();
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let i = usize::from(b.first() == Some(&b'_'));
    i < b.len() && alnum(b[i]) && b[i + 1..].iter().all(|c| alnum(*c) || *c == b'-')
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.split('/').all(valid_segment)
}

/// `$SWARMNOTES_DIR`, else the nearest `.swarmnotes` directory at or above
/// `project`.
pub fn store_dir(project: &Path) -> Option<PathBuf> {
    store_dir_in(project, DIR_NAME, "SWARMNOTES_DIR")
}

/// The store folder `dir`, moved by the variable `env` (when set and not
/// empty), else the nearest `dir` at or above `project`.
pub fn store_dir_in(project: &Path, dir: &str, env: &str) -> Option<PathBuf> {
    if !env.is_empty() {
        if let Some(v) = std::env::var_os(env) {
            if !v.is_empty() {
                let p = PathBuf::from(v);
                return p.is_dir().then_some(p);
            }
        }
    }
    let mut cur = Some(project);
    while let Some(d) = cur {
        let cand = d.join(dir);
        if cand.is_dir() {
            return Some(cand);
        }
        cur = d.parent();
    }
    None
}

/// Parse a note file's text. Errors name what is damaged.
fn parse_note(id: &str, text: &str) -> Result<Note, String> {
    // As Relatum's own reader does: a leading BOM is dropped and CRLF (how
    // Relatum writes on Windows) reads as LF.
    let text = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let text = text.as_str();
    if !text.starts_with("---\n") {
        return Err("no frontmatter".to_string());
    }
    let rel = text[4..]
        .find("\n---\n")
        .ok_or("frontmatter is not closed")?;
    let block = &text[4..4 + rel];
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in block.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| {
            format!(
                "frontmatter line is not key: value: {}",
                clip(line.trim(), 40)
            )
        })?;
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        match fields.iter_mut().find(|(fk, _)| *fk == k) {
            Some(slot) => slot.1 = v,
            None => fields.push((k, v)),
        }
    }
    // The rules below are Adsertio 1.5's `unreadable` rules for a Relatum
    // note (Adsertio/src/relatum.rs `parse`), so a note the brief calls
    // damaged is exactly a note Adsertio refuses as proof.
    let get = |k: &str| {
        fields
            .iter()
            .find(|(fk, _)| fk == k)
            .map(|(_, v)| v.as_str())
    };
    let is_list = |v: &str| v.starts_with('[') && v.ends_with(']');
    // Relatum reads `[a, b]` as a list, never as a kind, summary or status.
    let scalar = |k: &str| get(k).filter(|v| !is_list(v));
    let kind = scalar("kind")
        .filter(|s| !s.is_empty())
        .ok_or("kind is missing")?;
    let summary = scalar("summary")
        .filter(|s| !s.is_empty())
        .ok_or("summary is missing")?;
    let status = match get("status") {
        None => "active",
        Some(_) => scalar("status").unwrap_or("[list]"),
    };
    if !STATUSES.contains(&status) {
        return Err(format!(
            "status \"{}\" is not active, draft or superseded",
            clip(status, 20)
        ));
    }
    let raw_by = get("superseded_by").unwrap_or("");
    let by = scalar("superseded_by").filter(|v| !v.is_empty());
    if status == "superseded" {
        match by {
            None => return Err("status is superseded but superseded_by is missing".into()),
            Some(b) if !valid_id(b) => {
                return Err(format!(
                    "superseded_by \"{}\" is not a note id",
                    clip(b, 60)
                ))
            }
            Some(_) => {}
        }
    } else if !raw_by.is_empty() && raw_by.replace(' ', "") != "[]" {
        return Err(format!(
            "status is {status} but superseded_by is set; remove the superseded_by line, or finish the supersede with relatum supersede"
        ));
    }
    // Tags are not judged here (that is `relatum check`'s job, as in Adsertio).
    let tags = match get("tags") {
        Some(t) if is_list(t) => t[1..t.len() - 1]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let opt = |k: &str| get(k).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(Note {
        id: id.to_string(),
        kind: kind.to_string(),
        summary: summary.to_string(),
        status: status.to_string(),
        superseded_by: opt("superseded_by"),
        tags,
        author: opt("author"),
        updated: opt("updated"),
        body: text[4 + rel + 5..].trim_start_matches('\n').to_string(),
    })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

fn read_file(store: &Path, path: &Path) -> Result<String, String> {
    let canon_store = store
        .canonicalize()
        .map_err(|e| format!("cannot resolve store: {e}"))?;
    let canon = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve note: {e}"))?;
    if !canon.starts_with(&canon_store) {
        return Err("note is a symlink that leaves the store".to_string());
    }
    let meta = fs::metadata(&canon).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("note is not a file".to_string());
    }
    if meta.len() > MAX_NOTE_BYTES {
        return Err("note is over 1 MiB".to_string());
    }
    let bytes = fs::read(&canon).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|_| "note is not valid UTF-8".to_string())
}

/// Read one note. `Ok(None)` when no such note exists; `Err` for a bad id,
/// a symlink out of the store, or damaged frontmatter.
pub fn read_note(store: &Path, id: &str) -> Result<Option<Note>, String> {
    if !valid_id(id) {
        return Err(format!(
            "\"{}\" is not a note id (segments match _?[a-z0-9][a-z0-9-]*)",
            clip(id, 60)
        ));
    }
    let path = store.join(format!("{id}.md"));
    if fs::symlink_metadata(&path).is_err() {
        return Ok(None);
    }
    let text = read_file(store, &path).map_err(|e| format!("{id}: {e}"))?;
    parse_note(id, &text)
        .map(Some)
        .map_err(|e| format!("{id}: {e}"))
}

fn walk(
    store: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<Note>,
    damaged: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk(store, &path, &format!("{prefix}{name}/"), out, damaged)?;
        } else if let Some(stem) = name.strip_suffix(".md") {
            if ft.is_symlink() && !path.is_file() {
                continue;
            }
            let id = format!("{prefix}{stem}");
            if !valid_id(&id) {
                continue;
            }
            match read_file(store, &path).and_then(|text| parse_note(&id, &text)) {
                Ok(note) => out.push(note),
                Err(e) if e.contains("leaves the store") => continue,
                Err(e) => damaged.push((id, e)),
            }
        }
    }
    Ok(())
}

/// Every note in the store, sorted by id. A damaged note is an error naming
/// it; a symlink that leaves the store is skipped.
pub fn list(store: &Path) -> Result<Vec<Note>, String> {
    let scan = scan(store)?;
    match scan.damaged.into_iter().next() {
        Some((id, why)) => Err(format!("{id}: {why}")),
        None => Ok(scan.notes),
    }
}

/// Every readable note, plus every damaged one with the reason, both sorted
/// by id. `Err` only when the store itself cannot be read: one bad note never
/// hides the others.
#[derive(Debug, Default)]
pub struct Scan {
    pub notes: Vec<Note>,
    pub damaged: Vec<(String, String)>,
}

pub fn scan(store: &Path) -> Result<Scan, String> {
    if !store.is_dir() {
        return Err(format!("{} is not a directory", store.display()));
    }
    let mut s = Scan::default();
    walk(store, store, "", &mut s.notes, &mut s.damaged)?;
    s.notes.sort_by(|a, b| a.id.cmp(&b.id));
    s.damaged.sort();
    Ok(s)
}

/// The rules for the brief: active constraint, convention and decision
/// notes (newest `updated` first, then id), and the damaged notes, which are
/// not rules until fixed.
#[derive(Debug, Default)]
pub struct Rules {
    pub rules: Vec<Note>,
    pub damaged: Vec<(String, String)>,
}

pub fn active_rules(store: &Path) -> Result<Rules, String> {
    let Scan { notes, damaged } = scan(store)?;
    let mut rules: Vec<Note> = notes
        .into_iter()
        .filter(|n| n.status == "active" && RULE_KINDS.contains(&n.kind.as_str()))
        .collect();
    rules.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    Ok(Rules { rules, damaged })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mh-adapters-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn put(store: &Path, id: &str, text: &str) {
        let p = store.join(format!("{id}.md"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn note(kind: &str, summary: &str, extra: &str) -> String {
        format!("---\nkind: {kind}\nsummary: {summary}\n{extra}---\n\nbody text\n")
    }

    fn fixture() -> (PathBuf, PathBuf) {
        let project = tmp();
        let store = project.join(".swarmnotes");
        put(
            &store,
            "net/retry",
            &note(
                "decision",
                "Use backoff.",
                "author: parent\nupdated: 2026-10-03\ntags: [net, retry]\n",
            ),
        );
        put(
            &store,
            "_shared/style",
            &note("convention", "Use fmt.", "# comment\nupdated: 2026-11-01\n"),
        );
        put(
            &store,
            "net/old",
            &note(
                "constraint",
                "Old rule.",
                "status: superseded\nsuperseded_by: net/retry\n",
            ),
        );
        put(
            &store,
            "net/wip",
            &note("constraint", "Draft rule.", "status: draft\n"),
        );
        put(&store, "net/found", &note("finding", "A finding.", ""));
        put(&store, "net/zzz", &note("constraint", "No date.", ""));
        (project, store)
    }

    #[test]
    fn reads_a_note() {
        let (_p, store) = fixture();
        let n = read_note(&store, "net/retry").unwrap().unwrap();
        assert_eq!(n.kind, "decision");
        assert_eq!(n.status, "active");
        assert_eq!(n.tags, vec!["net", "retry"]);
        assert_eq!(n.author.as_deref(), Some("parent"));
        assert_eq!(n.updated.as_deref(), Some("2026-10-03"));
        assert_eq!(n.body, "body text\n");
        assert!(read_note(&store, "net/none").unwrap().is_none());
        assert!(read_note(&store, "../x").is_err());
        assert!(read_note(&store, "Net/Retry").is_err());
    }

    #[test]
    fn lists_and_skips() {
        let (_p, store) = fixture();
        fs::write(store.join("net/readme.txt"), "x").unwrap();
        put(&store, ".hidden/n", &note("decision", "h", ""));
        fs::write(store.join("net/.tmp.md"), "garbage").unwrap();
        let ids: Vec<String> = list(&store).unwrap().into_iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            [
                "_shared/style",
                "net/found",
                "net/old",
                "net/retry",
                "net/wip",
                "net/zzz"
            ]
        );
    }

    #[test]
    fn rules_sorted_and_filtered() {
        let (_p, store) = fixture();
        let ids: Vec<String> = active_rules(&store)
            .unwrap()
            .rules
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(ids, ["_shared/style", "net/retry", "net/zzz"]);
    }

    #[test]
    fn damaged_note_is_an_error() {
        let (_p, store) = fixture();
        put(&store, "bad/one", "no frontmatter here\n");
        put(&store, "bad/two", "---\nkind: decision\n---\nb\n");
        put(
            &store,
            "bad/three",
            "---\nkind decision\nsummary: s\n---\nb\n",
        );
        put(
            &store,
            "bad/four",
            "---\nkind: decision\nsummary: s\nstatus: weird\n---\nb\n",
        );
        for id in ["bad/one", "bad/two", "bad/three", "bad/four"] {
            assert!(read_note(&store, id).is_err(), "{id}");
        }
        let e = list(&store).unwrap_err();
        assert!(e.starts_with("bad/"), "{e}");
        // One damaged note never hides the good rules.
        let r = active_rules(&store).unwrap();
        assert_eq!(r.rules.len(), 3);
        let ids: Vec<&str> = r.damaged.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["bad/four", "bad/one", "bad/three", "bad/two"]);
    }

    #[test]
    fn damaged_exactly_where_adsertio_is_unreadable() {
        let (_p, store) = fixture();
        for (id, extra, why) in [
            (
                "x/reverted",
                "superseded_by: net/retry\n",
                "superseded_by is set",
            ),
            (
                "x/draft-target",
                "status: draft\nsuperseded_by: net/retry\n",
                "superseded_by is set",
            ),
            (
                "x/list-target",
                "superseded_by: [net/retry]\n",
                "superseded_by is set",
            ),
            (
                "x/no-target",
                "status: superseded\n",
                "superseded_by is missing",
            ),
            (
                "x/bad-target",
                "status: superseded\nsuperseded_by: Bad Id\n",
                "not a note id",
            ),
            ("x/list-status", "status: [active]\n", "is not active"),
        ] {
            put(&store, id, &note("constraint", "r", extra));
            let e = read_note(&store, id).unwrap_err();
            assert!(e.contains(why), "{id}: {e}");
        }
        put(&store, "x/list-kind", "---\nkind: [a]\nsummary: s\n---\n");
        assert!(read_note(&store, "x/list-kind")
            .unwrap_err()
            .contains("kind is missing"));
        // Not damaged: a blank or empty-list target, and tags that are not a list.
        for (id, extra) in [
            ("y/blank", "superseded_by:\n"),
            ("y/empty-list", "superseded_by: [ ]\n"),
            ("y/odd-tags", "tags: net\n"),
        ] {
            put(&store, id, &note("constraint", "r", extra));
            assert_eq!(
                read_note(&store, id).unwrap().unwrap().status,
                "active",
                "{id}"
            );
        }
    }

    #[test]
    fn store_dir_walks_up() {
        let (p, store) = fixture();
        let deep = p.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        if std::env::var_os("SWARMNOTES_DIR").is_none() {
            assert_eq!(store_dir(&deep), Some(store));
            assert_eq!(store_dir(&tmp()), None);
        }
    }

    #[test]
    fn reads_windows_line_endings_and_bom() {
        let project = tmp();
        let store = project.join(".swarmnotes");
        put(
            &store,
            "win/rule",
            "\u{feff}---\r\nkind: constraint\r\nsummary: CRLF rule.\r\n---\r\n\r\nbody\r\n",
        );
        let n = read_note(&store, "win/rule").unwrap().unwrap();
        assert_eq!(n.summary, "CRLF rule.");
        assert_eq!(active_rules(&store).unwrap().rules.len(), 1);
    }
}
