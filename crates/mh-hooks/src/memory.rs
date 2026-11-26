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
use mh_core::{identity::subagent_name, Config};
use mh_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// The heading the compact summary is asked to use.
pub const UNRECORDED_HEADING: &str = "## Not yet recorded";

/// Libellus note kinds, in the order the summary is asked to use them.
const KINDS: &[&str] = &["decision", "dead-end", "question", "finding", "context"];
const MAX_ITEMS: usize = 20;
const MAX_SUMMARY: usize = 200;
/// How much of the end of a transcript is read to find the current turn.
const TAIL_BYTES: u64 = 8 << 20;

/// Tools that edit files.
const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
/// Tools that save memory. Matched on the part after the last `__`, so both
/// `mcp__mem-harness__relatum_new` and `mcp__relatum__relatum_new` count.
const SAVE_TOOLS: &[&str] = &[
    "relatum_new",
    "relatum_edit",
    "relatum_supersede",
    "libellus_note",
    "libellus_resolve",
    "libellus_plan",
    "libellus_promote",
    "adsertio_claim",
    "adsertio_attach",
];

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

/// The `(kind, summary)` lines under [`UNRECORDED_HEADING`] in `summary`.
pub fn unrecorded_items(summary: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in summary.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            inside = t
                .trim_end_matches(':')
                .eq_ignore_ascii_case(UNRECORDED_HEADING);
            continue;
        }
        if !inside {
            continue;
        }
        let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) else {
            continue;
        };
        let item = item.trim();
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
                    (k, rest.trim())
                } else {
                    ("context".to_string(), item)
                }
            }
            None => ("context".to_string(), item),
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
    t.push('…');
    t
}

type Runner<'a> = dyn Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + Sync + 'a;

/// PostCompact: save the unrecorded lines of `compact_summary` as Libellus
/// notes for the main agent. Returns a short report for stderr (empty when
/// there was nothing to do). Never blocks anything.
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
    let items = unrecorded_items(summary);
    if items.is_empty() {
        return Ok(String::new());
    }
    if !crate::stores::exists(project, ".libellus", "LIBELLUS_DIR") {
        return Ok(format!(
            "{} unrecorded item(s) not saved: no Libellus store here (run mem-harness init)",
            items.len()
        ));
    }
    let notes = Value::Array(
        items
            .iter()
            .map(|(kind, text)| {
                Value::obj(vec![
                    ("kind", Value::str(kind.as_str())),
                    ("summary", Value::str(text.as_str())),
                    ("tags", Value::Array(vec![Value::str("from-compact")])),
                ])
            })
            .collect(),
    )
    .to_json();
    let timeout = Duration::from_secs(cfg.brief.command_timeout_s.max(1));
    let args = [
        "note",
        "--agent",
        cfg.main_agent.as_str(),
        "--notes",
        notes.as_str(),
    ];
    match run("libellus", &args, project, timeout) {
        Ok(o) if o.code == Some(0) => Ok(format!(
            "saved {} unrecorded item(s) as Libellus notes tagged from-compact",
            items.len()
        )),
        Ok(o) => Err(format!(
            "libellus note failed: {}",
            o.stderr
                .lines()
                .chain(o.stdout.lines())
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no output")
                .trim()
        )),
        Err(e) => Err(e),
    }
}

/// What the current turn of a transcript did.
#[derive(Debug, Default, PartialEq)]
pub struct Turn {
    pub edited: bool,
    pub saved: bool,
}

/// Read the turn that ends a transcript: everything after the last prompt
/// (a user entry with text, not only tool results).
pub fn last_turn(jsonl: &str) -> Turn {
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
                if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let prompt = match content {
                    Value::Str(s) => !s.trim().is_empty(),
                    Value::Array(items) => items
                        .iter()
                        .any(|i| i.get("type").and_then(Value::as_str) == Some("text")),
                    _ => false,
                };
                if prompt {
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
                    turn.edited |= EDIT_TOOLS.contains(&name);
                    turn.saved |= SAVE_TOOLS.contains(&base);
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

/// The request sent back to an agent that is about to stop.
pub fn nudge_text(agent: &str) -> String {
    format!(
        "Mem-Harness: this turn changed files but saved nothing to memory, so it will be lost at the next compact. \
If the turn settled a decision, rule, convention or dead end, or produced a result worth checking later, save it now: \
libellus_note (agent=\"{agent}\") for your own decisions and dead ends, relatum_new for a rule every agent must follow, \
adsertio_claim for a result. If nothing is worth keeping, finish without another tool call."
    )
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
    if crate::stores::missing(project).len() == crate::stores::STORES.len() {
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
    let turn = last_turn(&read_tail(Path::new(&path))?);
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
    Ok(Value::obj(vec![
        ("decision", Value::str("block")),
        ("reason", Value::str(nudge_text(&agent))),
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
        let dir = temp("post");
        let cfg = Config::builtin();
        let calls = Mutex::new(Vec::<Vec<String>>::new());
        let run = |p: &str, a: &[&str], _: &Path, _: Duration| {
            let mut v = vec![p.to_string()];
            v.extend(a.iter().map(|s| s.to_string()));
            calls.lock().unwrap().push(v);
            Ok(CmdOut {
                code: Some(0),
                stdout: "{}".into(),
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
        let call = calls.lock().unwrap()[0].clone();
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

    const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"do it"}}"#;
    const EDIT: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Edit","input":{}}]}}"#;
    const RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}}"#;
    const SAVE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"2","name":"mcp__mem-harness__libellus_note","input":{}}]}}"#;
    const META: &str = r#"{"type":"user","isMeta":true,"message":{"content":"<reminder>"}}"#;

    fn lines(ls: &[&str]) -> String {
        ls.join("\n") + "\n"
    }

    #[test]
    fn last_turn_starts_at_the_last_prompt() {
        assert_eq!(
            last_turn(&lines(&[PROMPT, EDIT, RESULT])),
            Turn {
                edited: true,
                saved: false
            }
        );
        assert_eq!(
            last_turn(&lines(&[PROMPT, EDIT, RESULT, SAVE])),
            Turn {
                edited: true,
                saved: true
            }
        );
        assert_eq!(
            last_turn(&lines(&[PROMPT, EDIT, RESULT, PROMPT])),
            Turn::default()
        );
        assert_eq!(
            last_turn(&lines(&[PROMPT, EDIT, META])),
            Turn {
                edited: true,
                saved: false
            }
        );
        assert!(last_turn(&lines(&[PROMPT, "not json", EDIT])).edited);
        let other = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__relatum__relatum_new"}]}}"#;
        assert!(last_turn(&lines(&[PROMPT, EDIT, other])).saved);
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
        assert!(last_turn(&tail).edited);
    }
}
