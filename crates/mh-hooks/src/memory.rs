//! Saving memory around a compact and at the end of a turn.
//!
//! A hook is a program, not the model, so it cannot write down what only the
//! model knows. These hooks do the three things a hook can do:
//!
//! - PreCompact: its stdout becomes instructions for the compact summary. It
//!   asks the summary to list what was decided but never saved, under one
//!   fixed heading.
//! - PostCompact: it receives the summary, and saves the lines under that
//!   heading as Libellus notes, so they outlive this session too.
//! - Stop and SubagentStop: when the turn changed files but saved no note,
//!   rule or claim, it keeps the agent going once with a request to save
//!   them. `stop_hook_active` stops it from asking twice.

use crate::run::CmdOut;
use mh_core::config::{BriefConfig, StoreSpec};
use mh_core::{identity::subagent_name, Config};
use mh_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// The heading the compact summary is asked to use.
pub const UNRECORDED_HEADING: &str = "## Not yet recorded";
/// The heading's words, matched at any level, numbered or bold.
const UNRECORDED_LABEL: &str = "not yet recorded";
/// The tag on notes saved from a compact summary.
const FROM_COMPACT: &str = "from-compact";

/// Libellus note kinds, in the order the summary is asked to use them.
const KINDS: &[&str] = &["decision", "dead-end", "question", "finding", "context"];
const MAX_ITEMS: usize = 20;
const MAX_SUMMARY: usize = 200;
/// How much of the end of a transcript is read to find the current turn.
const TAIL_BYTES: u64 = 8 << 20;

/// PreCompact stdout: instructions for the compact summary.
pub fn pre_compact_instructions() -> String {
    format!(
        "Mem-Harness: end the summary with a section headed exactly \"{UNRECORDED_HEADING}\". \
List in it every decision, rule, convention, dead end, open question and unchecked result \
from this conversation that was NOT saved with relatum_new, libellus_note or adsertio_claim. \
One line each, written \"- <kind>: <what, naming the file or symbol>\", where kind is one of \
decision, dead-end, question, finding. Rules, notes and claims that were saved come back on \
their own after the compact, so leave them out. If nothing is unsaved, write \"- none\"."
    )
}

/// A section label in a summary line.
#[derive(Debug, PartialEq)]
enum Label {
    /// A `#`..`######` heading of that level.
    Heading(usize, String),
    /// A numbered (`8. Title:`) or bold (`**Title**`) line standing alone.
    Plain(String),
}

/// The text of a label with bold markers and a trailing colon removed.
fn label_text(s: &str) -> String {
    let t = s.trim().trim_end_matches(':').trim();
    let t = t
        .strip_prefix("**")
        .and_then(|x| x.strip_suffix("**"))
        .or_else(|| t.strip_prefix("__").and_then(|x| x.strip_suffix("__")))
        .unwrap_or(t);
    t.trim().trim_end_matches(':').trim().to_string()
}

/// `8. ` or `8) ` at the start of `t`, returning the rest.
fn strip_number(t: &str) -> Option<&str> {
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &t[digits..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    rest.starts_with(char::is_whitespace).then(|| rest.trim())
}

fn is_bold(t: &str) -> bool {
    let t = t.trim_end_matches(':').trim_end();
    (t.starts_with("**") && t.ends_with("**") && t.len() > 4)
        || (t.starts_with("__") && t.ends_with("__") && t.len() > 4)
}

/// The label a trimmed line is, if any. A numbered line is a label only
/// when it ends in a colon or is bold (`1. decision: x` is an item).
fn label(t: &str) -> Option<Label> {
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &t[hashes..];
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            let rest = rest.trim();
            let rest = strip_number(rest).unwrap_or(rest);
            return Some(Label::Heading(hashes, label_text(rest)));
        }
    }
    if let Some(rest) = strip_number(t) {
        if rest.ends_with(':') || is_bold(rest) {
            return Some(Label::Plain(label_text(rest)));
        }
        return None;
    }
    is_bold(t).then(|| Label::Plain(label_text(t)))
}

/// Control characters become spaces, then runs of whitespace one space:
/// Libellus refuses a summary with a control character.
fn sanitize(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `(kind, summary)` lines under the "Not yet recorded" label in
/// `summary`: a heading at any level, a numbered (`8. Not yet recorded:`)
/// or bold (`**Not yet recorded**`) line, any case, colon optional. Lines
/// in ``` fences are skipped. A section opened by a heading ends at a
/// heading of the same or a higher level, or at a numbered or bold label;
/// one opened by a numbered or bold label ends at any label.
pub fn unrecorded_items(summary: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // None: outside. Some(level): inside; level 7 for a non-heading label.
    let mut inside: Option<usize> = None;
    let mut fence: Option<char> = None;
    for line in summary.lines() {
        let t = line.trim();
        if let Some(f) = fence {
            if t.starts_with(&format!("{f}{f}{f}")) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = t.chars().next();
            continue;
        }
        if let Some(l) = label(t) {
            let (level, text) = match l {
                Label::Heading(n, s) => (n, s),
                Label::Plain(s) => (7, s),
            };
            let opens = text.eq_ignore_ascii_case(UNRECORDED_LABEL);
            match inside {
                _ if opens => inside = Some(level),
                Some(open) if level == 7 || level <= open => inside = None,
                _ => {}
            }
            continue;
        }
        if inside.is_none() {
            continue;
        }
        let Some(item) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
            .or_else(|| strip_number(t))
        else {
            continue;
        };
        let item = sanitize(item);
        if item.is_empty() || item.trim_end_matches('.').eq_ignore_ascii_case("none") {
            continue;
        }
        let (kind, text) = match item.split_once(':') {
            Some((k, rest)) => {
                let k = k
                    .trim()
                    .trim_matches('*')
                    .to_ascii_lowercase()
                    .replace(' ', "-");
                if KINDS.contains(&k.as_str()) {
                    (k, rest.trim_start_matches('*').trim())
                } else {
                    ("context".to_string(), item.as_str())
                }
            }
            None => ("context".to_string(), item.as_str()),
        };
        if text.is_empty() {
            continue;
        }
        out.push((kind, clip(text, MAX_SUMMARY)));
        if out.len() == MAX_ITEMS {
            break;
        }
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t.truncate(t.trim_end().len());
    t.push('…');
    t
}

type Runner<'a> = dyn Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + Sync + 'a;

/// The configured store of `tool`, or its usual place when the config has
/// none.
fn store_of(cfg: &Config, tool: &str, dir: &str, env: &str) -> StoreSpec {
    cfg.stores
        .iter()
        .find(|s| s.tool == tool)
        .cloned()
        .unwrap_or_else(|| StoreSpec {
            tool: tool.to_string(),
            dir: dir.to_string(),
            env: env.to_string(),
            init: vec![tool.to_string(), "init".to_string()],
        })
}

fn first_line(o: &CmdOut) -> String {
    o.stderr
        .lines()
        .chain(o.stdout.lines())
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .trim()
        .to_string()
}

/// The summaries of the agent's notes tagged `from-compact` that equal one
/// of `wanted`. `libellus ls` lists every note's summary but not its tags,
/// so only the notes whose summary matches are read with `libellus get`.
fn saved_from_compact(
    project: &Path,
    agent: &str,
    wanted: &[&str],
    timeout: Duration,
    run: &Runner,
) -> Result<Vec<String>, String> {
    let args = ["ls", "--agent", agent, "--limit", "1000000"];
    let o = run("libellus", &args, project, timeout)?;
    if o.code != Some(0) {
        return Err(format!("libellus ls failed: {}", first_line(&o)));
    }
    let v = mh_json::parse(o.stdout.trim())
        .map_err(|e| format!("libellus ls output is not JSON ({})", e.msg))?;
    let mut found = Vec::new();
    for n in v
        .get("notes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let (Some(id), Some(summary)) = (
            n.get("id").and_then(Value::as_str),
            n.get("summary").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !wanted.contains(&summary) || found.iter().any(|f| f == summary) {
            continue;
        }
        let g = run(
            "libellus",
            &["get", "--agent", agent, "--id", id],
            project,
            timeout,
        )?;
        let tagged = g.code == Some(0)
            && mh_json::parse(g.stdout.trim())
                .ok()
                .and_then(|v| {
                    v.get("tags")
                        .and_then(Value::as_array)
                        .map(|t| t.iter().any(|x| x.as_str() == Some(FROM_COMPACT)))
                })
                .unwrap_or(false);
        if tagged {
            found.push(summary.to_string());
        }
    }
    Ok(found)
}

fn notes_json(items: &[(String, String)]) -> String {
    Value::Array(
        items
            .iter()
            .map(|(kind, text)| {
                Value::obj(vec![
                    ("kind", Value::str(kind.as_str())),
                    ("summary", Value::str(text.as_str())),
                    ("tags", Value::Array(vec![Value::str(FROM_COMPACT)])),
                ])
            })
            .collect(),
    )
    .to_json()
}

/// PostCompact: save the unrecorded lines of `compact_summary` as Libellus
/// notes for the main agent. Lines already saved by an earlier compact (a
/// `from-compact` note with the same summary) are skipped. When Libellus
/// refuses the batch, each line is retried alone so one bad line does not
/// lose the rest. Returns a short report for stderr (empty when there was
/// nothing to do); `Err` only when nothing could be saved. Never blocks
/// anything.
pub fn post_compact(
    cfg: &Config,
    project: &Path,
    stdin: &str,
    run: &Runner,
) -> Result<String, String> {
    if !cfg.brief.save_unrecorded_on_compact {
        return Ok(String::new());
    }
    let v = mh_json::parse(stdin.trim())
        .map_err(|e| format!("post-compact stdin is not JSON ({})", e.msg))?;
    let summary = v
        .get("compact_summary")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut items = unrecorded_items(summary);
    // The same line twice in one summary is one note.
    let mut seen: Vec<String> = Vec::new();
    items.retain(|(_, s)| {
        let new = !seen.contains(s);
        seen.push(s.clone());
        new
    });
    if items.is_empty() {
        return Ok(String::new());
    }
    let store = store_of(cfg, "libellus", ".libellus", "LIBELLUS_DIR");
    if !crate::stores::exists(project, &store) {
        return Ok(format!(
            "{} unrecorded item(s) not saved: no Libellus store here (run mem-harness init)",
            items.len()
        ));
    }
    let timeout = Duration::from_secs(cfg.brief.command_timeout_s.max(1));
    let agent = cfg.main_agent.as_str();
    let mut report = Vec::new();
    let wanted: Vec<&str> = items.iter().map(|(_, s)| s.as_str()).collect();
    match saved_from_compact(project, agent, &wanted, timeout, run) {
        Ok(dups) if !dups.is_empty() => {
            items.retain(|(_, s)| !dups.contains(s));
            report.push(format!(
                "skipped {} item(s) already saved by an earlier compact",
                dups.len()
            ));
        }
        Ok(_) => {}
        Err(e) => report.push(format!("could not check for earlier notes ({e})")),
    }
    if items.is_empty() {
        return Ok(report.join("; "));
    }
    let save = |batch: &[(String, String)]| -> Result<(), String> {
        let notes = notes_json(batch);
        let args = ["note", "--agent", agent, "--notes", notes.as_str()];
        match run("libellus", &args, project, timeout) {
            Ok(o) if o.code == Some(0) => Ok(()),
            Ok(o) => Err(first_line(&o)),
            Err(e) => Err(e),
        }
    };
    let (saved, failed) = match save(&items) {
        Ok(()) => (items.len(), Vec::new()),
        Err(e) if items.len() == 1 => (0, vec![e]),
        Err(_) => {
            let mut ok = 0;
            let mut failed = Vec::new();
            for it in &items {
                match save(std::slice::from_ref(it)) {
                    Ok(()) => ok += 1,
                    Err(e) => failed.push(e),
                }
            }
            (ok, failed)
        }
    };
    if saved == 0 {
        return Err(format!("libellus note failed: {}", failed[0]));
    }
    report.insert(
        0,
        format!("saved {saved} unrecorded item(s) as Libellus notes tagged {FROM_COMPACT}"),
    );
    if !failed.is_empty() {
        report.push(format!("{} item(s) refused: {}", failed.len(), failed[0]));
    }
    Ok(report.join("; "))
}

/// What the current turn of a transcript did.
#[derive(Debug, Default, PartialEq)]
pub struct Turn {
    pub edited: bool,
    pub saved: bool,
}

/// Tags that open text Claude Code writes into the transcript as a user
/// entry but that no one typed: background-task events, the output of a
/// local command or shell escape, and an interrupt marker.
const NOT_PROMPT_TAGS: &[&str] = &[
    "<task-notification>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<bash-stdout>",
    "<bash-stderr>",
    "<agent-message",
    "<teammate-message",
    "[Request interrupted by user",
];

/// Text left once `<system-reminder>` blocks are removed.
fn without_reminders(s: &str) -> String {
    let mut rest = s;
    let mut out = String::new();
    while let Some(i) = rest.find("<system-reminder>") {
        out.push_str(&rest[..i]);
        match rest[i..].find("</system-reminder>") {
            Some(j) => rest = &rest[i + j + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_prompt_text(s: &str) -> bool {
    let t = without_reminders(s);
    let t = t.trim_start();
    !t.is_empty() && !NOT_PROMPT_TAGS.iter().any(|p| t.starts_with(p))
}

/// Whether a user entry is a prompt someone typed, which starts a turn.
/// Not prompts: meta entries, the compact summary, entries whose origin is
/// not a human (task notifications, peer and coordinator messages), tool
/// results, and text that is only system reminders or one of
/// [`NOT_PROMPT_TAGS`].
fn is_prompt(v: &Value, content: &Value) -> bool {
    let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    if flag("isMeta") || flag("isCompactSummary") {
        return false;
    }
    if let Some(kind) = v
        .get("origin")
        .and_then(|o| o.get("kind"))
        .and_then(Value::as_str)
    {
        if kind != "human" {
            return false;
        }
    }
    match content {
        Value::Str(s) => is_prompt_text(s),
        Value::Array(items) => items
            .iter()
            .any(|i| match i.get("type").and_then(Value::as_str) {
                Some("text") => is_prompt_text(i.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("image") => true,
                _ => false,
            }),
        _ => false,
    }
}

/// Read the turn that ends a transcript: everything after the last prompt
/// (see [`is_prompt`]). Edit and save tools come from `brief`.
pub fn last_turn(jsonl: &str, brief: &BriefConfig) -> Turn {
    let mut turn = Turn::default();
    for line in jsonl.lines() {
        let is_user = line.contains("\"type\":\"user\"");
        if !is_user && !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(v) = mh_json::parse(line) else {
            continue;
        };
        let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                if is_prompt(&v, content) {
                    turn = Turn::default();
                }
            }
            Some("assistant") => {
                for item in content.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let base = name.rsplit("__").next().unwrap_or(name);
                    turn.edited |= brief.edit_tools.iter().any(|t| t == name);
                    turn.saved |= brief.save_tools.iter().any(|t| t == base);
                }
            }
            _ => {}
        }
    }
    turn
}

/// The last `TAIL_BYTES` of a file, starting at a line boundary.
fn read_tail(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("cannot open transcript: {e}"))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))
        .map_err(|e| format!("cannot read transcript: {e}"))?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read transcript: {e}"))?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

/// What the nudge says each store's tool is for, keyed by store tool.
fn nudge_use(tool: &str, agent: &str) -> Option<String> {
    Some(match tool {
        "libellus" => {
            format!("libellus_note (agent=\"{agent}\") for your own decisions and dead ends")
        }
        "relatum" => "relatum_new for a rule every agent must follow".to_string(),
        "adsertio" => "adsertio_claim for a result".to_string(),
        _ => return None,
    })
}

/// The request sent back to an agent that is about to stop, naming only
/// the save tools whose store exists (`present`, store tool names).
/// `None` when no such tool is available.
pub fn nudge_text(agent: &str, present: &[&str]) -> Option<String> {
    let uses: Vec<String> = ["libellus", "relatum", "adsertio"]
        .into_iter()
        .filter(|t| present.contains(t))
        .filter_map(|t| nudge_use(t, agent))
        .collect();
    if uses.is_empty() {
        return None;
    }
    Some(format!(
        "Mem-Harness: this turn changed files but saved nothing to memory, so it will be lost at the next compact. \
If the turn settled a decision, rule, convention or dead end, or produced a result worth checking later, save it now: \
{}. If nothing is worth keeping, finish without another tool call.",
        uses.join(", ")
    ))
}

/// Stop / SubagentStop: the hook's stdout. `{}` lets the agent stop; a
/// `decision: block` keeps it going once with [`nudge_text`].
pub fn stop(cfg: &Config, project: &Path, stdin: &str, subagent: bool) -> Result<String, String> {
    let allow = Ok("{}".to_string());
    if !cfg.brief.nudge_on_stop {
        return allow;
    }
    let v =
        mh_json::parse(stdin.trim()).map_err(|e| format!("stop stdin is not JSON ({})", e.msg))?;
    if v.get("stop_hook_active").and_then(Value::as_bool) == Some(true) {
        return allow;
    }
    // A project with no memory store has not opted in; the brief already
    // says how to set it up.
    let present: Vec<&str> = cfg
        .stores
        .iter()
        .filter(|s| crate::stores::exists(project, s))
        .map(|s| s.tool.as_str())
        .collect();
    if present.is_empty() {
        return allow;
    }
    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let path = if subagent {
        Some(field("agent_transcript_path")).filter(|p| !p.is_empty())
    } else {
        None
    }
    .unwrap_or_else(|| field("transcript_path"));
    if path.is_empty() {
        return allow;
    }
    let turn = last_turn(&read_tail(Path::new(&path))?, &cfg.brief);
    if !turn.edited || turn.saved {
        return allow;
    }
    let agent = if subagent {
        let (kind, id) = (field("agent_type"), field("agent_id"));
        if kind.is_empty() && id.is_empty() {
            cfg.main_agent.clone()
        } else {
            subagent_name(&kind, &id)
        }
    } else {
        cfg.main_agent.clone()
    };
    let Some(reason) = nudge_text(&agent, &present) else {
        return allow;
    };
    Ok(Value::obj(vec![
        ("decision", Value::str("block")),
        ("reason", Value::str(reason)),
    ])
    .to_json())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mh-memory-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pre_compact_names_the_heading_and_kinds() {
        let t = pre_compact_instructions();
        assert!(t.contains(UNRECORDED_HEADING) && t.contains("dead-end") && t.contains("- none"));
        assert!(t.len() < 10_000);
    }

    #[test]
    fn unrecorded_lines_are_read_from_their_section_only() {
        let s = "# Summary\n- decision: not this one\n\n## Not yet recorded\n\
- decision: keep the cache in src/a.rs\n- Dead End: retrying the lock\n* question: why?\n\
- a line with no kind\n- rule: odd kind: kept whole\n- none\n- \n\n## Next\n- finding: not this\n";
        let items = unrecorded_items(s);
        assert_eq!(
            items,
            vec![
                ("decision".into(), "keep the cache in src/a.rs".into()),
                ("dead-end".into(), "retrying the lock".into()),
                ("question".into(), "why?".into()),
                ("context".into(), "a line with no kind".into()),
                ("context".into(), "rule: odd kind: kept whole".into()),
            ]
        );
        assert!(unrecorded_items("## Not yet recorded\n- none\n").is_empty());
        assert!(unrecorded_items("no heading\n- decision: x\n").is_empty());
        let long = format!("## Not yet recorded\n- finding: {}\n", "é".repeat(300));
        assert_eq!(unrecorded_items(&long)[0].1.chars().count(), MAX_SUMMARY);
        let many = (0..40)
            .map(|i| format!("- finding: f{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            unrecorded_items(&format!("{UNRECORDED_HEADING}\n{many}")).len(),
            MAX_ITEMS
        );
    }

    #[test]
    fn post_compact_saves_notes_only_with_a_store() {
        if std::env::var_os("LIBELLUS_DIR").is_some() {
            return; // the environment points Libellus elsewhere
        }
        let dir = temp("post");
        let cfg = Config::builtin();
        let calls = Mutex::new(Vec::<Vec<String>>::new());
        let run = |p: &str, a: &[&str], _: &Path, _: Duration| {
            let mut v = vec![p.to_string()];
            v.extend(a.iter().map(|s| s.to_string()));
            calls.lock().unwrap().push(v);
            Ok(CmdOut {
                code: Some(0),
                stdout: if a[0] == "ls" {
                    r#"{"notes":[]}"#
                } else {
                    "{}"
                }
                .into(),
                stderr: String::new(),
            })
        };
        let stdin = r#"{"trigger":"auto","compact_summary":"x\n## Not yet recorded\n- decision: use \"y\"\n"}"#;
        let r = post_compact(&cfg, &dir, stdin, &run).unwrap();
        assert!(r.contains("no Libellus store"), "{r}");
        assert!(calls.lock().unwrap().is_empty());
        std::fs::create_dir_all(dir.join(".libellus")).unwrap();
        let r = post_compact(&cfg, &dir, stdin, &run).unwrap();
        assert!(r.starts_with("saved 1"), "{r}");
        assert_eq!(
            &calls.lock().unwrap()[0][..4],
            ["libellus", "ls", "--agent", "main"]
        );
        let call = calls.lock().unwrap()[1].clone();
        assert_eq!(&call[..4], ["libellus", "note", "--agent", "main"]);
        let notes = mh_json::parse(&call[5]).unwrap();
        let n = &notes.as_array().unwrap()[0];
        assert_eq!(n.get("summary").and_then(Value::as_str), Some("use \"y\""));
        assert_eq!(n.get("kind").and_then(Value::as_str), Some("decision"));
        assert_eq!(
            post_compact(&cfg, &dir, r#"{"compact_summary":"nothing"}"#, &run).unwrap(),
            ""
        );
        assert!(post_compact(&cfg, &dir, "nope", &run).is_err());
        let mut off = cfg.clone();
        off.brief.save_unrecorded_on_compact = false;
        assert_eq!(post_compact(&off, &dir, stdin, &run).unwrap(), "");
    }

    #[test]
    fn unrecorded_heading_variants() {
        let kinds =
            |s: &str| -> Vec<String> { unrecorded_items(s).into_iter().map(|(_, t)| t).collect() };
        for s in [
            "## Summary\n### Not yet recorded\n- decision: a\n",
            "# Not yet recorded\n- decision: a\n",
            "###### not YET recorded:\n- decision: a\n",
            "Summary:\n8. Not yet recorded:\n- decision: a\n9. Optional Next Step:\n- finding: no\n",
            "**Not yet recorded**\n- decision: a\n**Next**\n- finding: no\n",
            "**Not yet recorded:**\n- decision: a\n",
            "8. **Not yet recorded**\n- decision: a\n## Next\n- finding: no\n",
            "## 8. Not yet recorded\r\n- decision: a\r\n## 9. Next\r\n- finding: no\r\n",
            "## Not yet recorded\n1. decision: a\n",
        ] {
            assert_eq!(kinds(s), ["a"], "{s:?}");
        }
        // A deeper heading stays inside; the same or a higher level ends it.
        let s = "## Not yet recorded\n- decision: a\n### Details\n- finding: b\n## Next\n- finding: no\n";
        assert_eq!(kinds(s), ["a", "b"]);
        let s = "### Not yet recorded\n- decision: a\n# Top\n- finding: no\n";
        assert_eq!(kinds(s), ["a"]);
        // A `#` line in a fence is code, not a heading; fenced bullets are not items.
        let s = "## Not yet recorded\n- decision: a\n```sh\n# run this\n- not an item\n```\n- finding: b\n";
        assert_eq!(kinds(s), ["a", "b"]);
        let s = "```\n## Not yet recorded\n- decision: no\n```\n";
        assert!(kinds(s).is_empty());
        // A bold item or a numbered item is not a section label.
        let s = "## Not yet recorded\n- **decision**: a\n1. finding: b\n";
        assert_eq!(kinds(s), ["a", "b"]);
        // "Not yet recorded" in the middle of a heading is another section.
        assert!(kinds("## Things not yet recorded here\n- decision: a\n").is_empty());
    }

    #[test]
    fn control_characters_are_cleaned_before_clipping() {
        let s = "## Not yet recorded\n- finding: col\tseparated\u{7}  output\u{1b}[0m\n";
        assert_eq!(
            kinds_of(s),
            [("finding".into(), "col separated output [0m".into())]
        );
        // Clipping never leaves a trailing space before the ellipsis.
        let long = format!(
            "## Not yet recorded\n- finding: {} tail\n",
            "a ".repeat(150)
        );
        let t = &unrecorded_items(&long)[0].1;
        assert!(!t.ends_with(" …") && t.ends_with('…') && t.chars().count() <= MAX_SUMMARY);
    }

    fn kinds_of(s: &str) -> Vec<(String, String)> {
        unrecorded_items(s)
    }

    /// A Libellus stand-in: `ls` lists summaries, `get` returns tags, and
    /// `note` refuses the whole batch when any summary contains "BAD".
    struct FakeLibellus {
        notes: Mutex<Vec<(String, String, Vec<String>)>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeLibellus {
        fn new() -> FakeLibellus {
            FakeLibellus {
                notes: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn run(&self, _p: &str, a: &[&str], _: &Path, _: Duration) -> Result<CmdOut, String> {
            self.calls.lock().unwrap().push(a[0].to_string());
            let ok = |s: String| {
                Ok(CmdOut {
                    code: Some(0),
                    stdout: s,
                    stderr: String::new(),
                })
            };
            let mut notes = self.notes.lock().unwrap();
            match a[0] {
                "ls" => ok(Value::obj(vec![(
                    "notes",
                    Value::Array(
                        notes
                            .iter()
                            .map(|(id, s, _)| {
                                Value::obj(vec![("id", Value::str(id)), ("summary", Value::str(s))])
                            })
                            .collect(),
                    ),
                )])
                .to_json()),
                "get" => {
                    let n = notes.iter().find(|n| n.0 == a[4]).unwrap();
                    let tags = n.2.iter().map(Value::str).collect();
                    ok(Value::obj(vec![("tags", Value::Array(tags))]).to_json())
                }
                "note" => {
                    let batch = mh_json::parse(a[4]).unwrap();
                    let batch = batch.as_array().unwrap();
                    let sum = |n: &Value| {
                        n.get("summary")
                            .and_then(Value::as_str)
                            .unwrap()
                            .to_string()
                    };
                    if batch.iter().any(|n| sum(n).contains("BAD")) {
                        return Ok(CmdOut {
                            code: Some(1),
                            stdout: String::new(),
                            stderr: "error: nothing written: notes[0]: bad".into(),
                        });
                    }
                    for n in batch {
                        let tags = n
                            .get("tags")
                            .and_then(Value::as_array)
                            .map(|t| {
                                t.iter()
                                    .filter_map(Value::as_str)
                                    .map(String::from)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let id = format!("n{}", notes.len());
                        notes.push((id, sum(n), tags));
                    }
                    ok("{}".into())
                }
                other => panic!("{other}"),
            }
        }

        fn summaries(&self) -> Vec<String> {
            self.notes
                .lock()
                .unwrap()
                .iter()
                .map(|n| n.1.clone())
                .collect()
        }
    }

    fn compact(summary: &str) -> String {
        Value::obj(vec![("compact_summary", Value::str(summary))]).to_json()
    }

    #[test]
    fn repeated_compacts_do_not_duplicate_notes() {
        if std::env::var_os("LIBELLUS_DIR").is_some() {
            return;
        }
        let dir = temp("dedupe");
        std::fs::create_dir_all(dir.join(".libellus")).unwrap();
        let cfg = Config::builtin();
        let lib = FakeLibellus::new();
        let run = |p: &str, a: &[&str], c: &Path, t: Duration| lib.run(p, a, c, t);
        // The same note written by hand (no from-compact tag) is not a duplicate.
        lib.notes
            .lock()
            .unwrap()
            .push(("hand".into(), "by hand".into(), vec![]));
        let s = compact("## Not yet recorded\n- decision: a\n- finding: a\n- finding: by hand\n");
        let r = post_compact(&cfg, &dir, &s, &run).unwrap();
        assert!(r.starts_with("saved 2 "), "{r}");
        assert_eq!(lib.summaries(), ["by hand", "a", "by hand"]);
        let s = compact("## Not yet recorded\n- decision: a\n- question: new\n");
        let r = post_compact(&cfg, &dir, &s, &run).unwrap();
        assert!(r.starts_with("saved 1 ") && r.contains("skipped 1"), "{r}");
        let r = post_compact(&cfg, &dir, &s, &run).unwrap();
        assert_eq!(r, "skipped 2 item(s) already saved by an earlier compact");
        assert_eq!(lib.summaries(), ["by hand", "a", "by hand", "new"]);
    }

    #[test]
    fn one_refused_item_does_not_lose_the_rest() {
        if std::env::var_os("LIBELLUS_DIR").is_some() {
            return;
        }
        let dir = temp("retry");
        std::fs::create_dir_all(dir.join(".libellus")).unwrap();
        let cfg = Config::builtin();
        let lib = FakeLibellus::new();
        let run = |p: &str, a: &[&str], c: &Path, t: Duration| lib.run(p, a, c, t);
        let s = compact("## Not yet recorded\n- decision: a\n- finding: BAD one\n- finding: c\n");
        let r = post_compact(&cfg, &dir, &s, &run).unwrap();
        assert!(
            r.starts_with("saved 2 ") && r.contains("1 item(s) refused"),
            "{r}"
        );
        assert_eq!(lib.summaries(), ["a", "c"]);
        assert_eq!(
            *lib.calls.lock().unwrap(),
            ["ls", "note", "note", "note", "note"]
        );
        // Nothing saved at all is an error.
        let s = compact("## Not yet recorded\n- finding: BAD\n");
        assert!(post_compact(&cfg, &dir, &s, &run)
            .unwrap_err()
            .contains("bad"));
    }

    #[test]
    fn nudge_names_only_tools_with_a_store() {
        let all = nudge_text("main", &["relatum", "libellus", "adsertio"]).unwrap();
        assert!(all.contains("libellus_note (agent=\"main\")") && all.contains("relatum_new"));
        let some = nudge_text("main", &["relatum"]).unwrap();
        assert!(
            some.contains("relatum_new")
                && !some.contains("libellus")
                && !some.contains("adsertio")
        );
        assert!(nudge_text("main", &["other"]).is_none());
    }

    #[test]
    fn stop_nudge_skips_tools_without_a_store() {
        if std::env::var_os("LIBELLUS_DIR").is_some()
            || std::env::var_os("SWARMNOTES_DIR").is_some()
        {
            return;
        }
        let dir = temp("stop-stores");
        let cfg = Config::builtin();
        let t = dir.join("t.jsonl");
        std::fs::write(&t, lines(&[PROMPT, EDIT, RESULT])).unwrap();
        std::fs::create_dir_all(dir.join(".swarmnotes")).unwrap();
        let input = Value::obj(vec![(
            "transcript_path",
            Value::str(t.display().to_string()),
        )])
        .to_json();
        let out = mh_json::parse(&stop(&cfg, &dir, &input, false).unwrap()).unwrap();
        let reason = out.get("reason").and_then(Value::as_str).unwrap();
        assert!(
            reason.contains("relatum_new") && !reason.contains("libellus_note"),
            "{reason}"
        );
    }

    #[test]
    fn injected_user_entries_do_not_start_a_turn() {
        let notes = [
            r#"{"type":"user","origin":{"kind":"task-notification","producer":"session-task"},"message":{"role":"user","content":"<task-notification>\n<task-id>b1</task-id>\n</task-notification>"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>b1</task-id>\n</task-notification>"}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"task-notification"},"message":{"content":"[SYSTEM NOTIFICATION - NOT USER INPUT]"}}"#,
            r#"{"type":"user","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"content":"This session is being continued from a previous conversation."}}"#,
            r#"{"type":"user","isMeta":true,"origin":{"kind":"peer"},"message":{"content":"Another Claude session sent a message:"}}"#,
            r#"{"type":"user","origin":{"kind":"coordinator"},"message":{"content":"The coordinator sent a message while you were working"}}"#,
            r#"{"type":"user","message":{"content":"<system-reminder>\nremember\n</system-reminder>\n"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"user","message":{"content":"<local-command-stdout>Bye!</local-command-stdout>"}}"#,
            r#"{"type":"user","message":{"content":"<bash-stdout>ok</bash-stdout><bash-stderr></bash-stderr>"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        ];
        for n in notes {
            assert!(lt(&lines(&[PROMPT, EDIT, RESULT, n])).edited, "{n}");
        }
        let prompts = [
            r#"{"type":"user","origin":{"kind":"human"},"message":{"content":"<pasted_content id=\"1\">x</pasted_content>"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/review</command-name>"}}"#,
            r#"{"type":"user","message":{"content":"<system-reminder>x</system-reminder>\nnow do this"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"image","source":{}}]}}"#,
        ];
        for p in prompts {
            assert!(!lt(&lines(&[PROMPT, EDIT, RESULT, p])).edited, "{p}");
        }
    }

    #[test]
    fn edit_and_save_tools_come_from_config() {
        let mut b = Config::builtin().brief;
        b.edit_tools = vec!["Patch".into()];
        b.save_tools = vec!["keep".into()];
        let patch =
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Patch"}]}}"#;
        let keep = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__x__keep"}]}}"#;
        assert_eq!(
            last_turn(&lines(&[PROMPT, EDIT, SAVE]), &b),
            Turn::default()
        );
        assert_eq!(
            last_turn(&lines(&[PROMPT, patch, keep]), &b),
            Turn {
                edited: true,
                saved: true
            }
        );
    }

    const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"do it"}}"#;
    const EDIT: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Edit","input":{}}]}}"#;
    const RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}}"#;
    const SAVE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"2","name":"mcp__mem-harness__libellus_note","input":{}}]}}"#;
    const META: &str = r#"{"type":"user","isMeta":true,"message":{"content":"<reminder>"}}"#;

    fn lt(s: &str) -> Turn {
        last_turn(s, &Config::builtin().brief)
    }

    fn lines(ls: &[&str]) -> String {
        ls.join("\n") + "\n"
    }

    #[test]
    fn last_turn_starts_at_the_last_prompt() {
        assert_eq!(
            lt(&lines(&[PROMPT, EDIT, RESULT])),
            Turn {
                edited: true,
                saved: false
            }
        );
        assert_eq!(
            lt(&lines(&[PROMPT, EDIT, RESULT, SAVE])),
            Turn {
                edited: true,
                saved: true
            }
        );
        assert_eq!(lt(&lines(&[PROMPT, EDIT, RESULT, PROMPT])), Turn::default());
        assert_eq!(
            lt(&lines(&[PROMPT, EDIT, META])),
            Turn {
                edited: true,
                saved: false
            }
        );
        assert!(lt(&lines(&[PROMPT, "not json", EDIT])).edited);
        let other = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__relatum__relatum_new"}]}}"#;
        assert!(lt(&lines(&[PROMPT, EDIT, other])).saved);
    }

    #[test]
    fn stop_blocks_once_and_only_when_needed() {
        let dir = temp("stop");
        let cfg = Config::builtin();
        let t = dir.join("t.jsonl");
        std::fs::write(&t, lines(&[PROMPT, EDIT, RESULT])).unwrap();
        let input = |active: bool| {
            Value::obj(vec![
                ("transcript_path", Value::str(t.display().to_string())),
                ("stop_hook_active", Value::Bool(active)),
            ])
            .to_json()
        };
        // No store at all: the project has not opted in.
        assert_eq!(stop(&cfg, &dir, &input(false), false).unwrap(), "{}");
        std::fs::create_dir_all(dir.join(".libellus")).unwrap();
        let out = mh_json::parse(&stop(&cfg, &dir, &input(false), false).unwrap()).unwrap();
        assert_eq!(out.get("decision").and_then(Value::as_str), Some("block"));
        assert!(out
            .get("reason")
            .and_then(Value::as_str)
            .unwrap()
            .contains("agent=\"main\""));
        assert_eq!(stop(&cfg, &dir, &input(true), false).unwrap(), "{}");
        let mut off = cfg.clone();
        off.brief.nudge_on_stop = false;
        assert_eq!(stop(&off, &dir, &input(false), false).unwrap(), "{}");
        std::fs::write(&t, lines(&[PROMPT, EDIT, RESULT, SAVE])).unwrap();
        assert_eq!(stop(&cfg, &dir, &input(false), false).unwrap(), "{}");

        // A sub-agent is judged on its own transcript and named its own way.
        let a = dir.join("agent.jsonl");
        std::fs::write(&a, lines(&[PROMPT, EDIT])).unwrap();
        let sub = Value::obj(vec![
            ("transcript_path", Value::str(t.display().to_string())),
            ("agent_transcript_path", Value::str(a.display().to_string())),
            ("agent_type", Value::str("Explore")),
            ("agent_id", Value::str("agent-a1b2c3d4e5f6")),
            ("stop_hook_active", Value::Bool(false)),
        ])
        .to_json();
        let out = mh_json::parse(&stop(&cfg, &dir, &sub, true).unwrap()).unwrap();
        assert!(out
            .get("reason")
            .and_then(Value::as_str)
            .unwrap()
            .contains("agent=\"explore-c3d4e5f6\""));
        assert!(stop(&cfg, &dir, "nope", false).is_err());
        assert!(stop(&cfg, &dir, r#"{"transcript_path":"/no/such/file"}"#, false).is_err());
    }

    #[test]
    fn tail_starts_on_a_line_boundary() {
        let dir = temp("tail");
        let p = dir.join("big.jsonl");
        let filler = "x".repeat(1000);
        let mut s = String::new();
        while (s.len() as u64) < TAIL_BYTES + 5000 {
            s.push_str(&filler);
            s.push('\n');
        }
        s.push_str(PROMPT);
        s.push('\n');
        s.push_str(EDIT);
        s.push('\n');
        std::fs::write(&p, &s).unwrap();
        let tail = read_tail(&p).unwrap();
        assert!(tail.starts_with(&filler) || tail.starts_with('{'));
        assert!(lt(&tail).edited);
    }
}
