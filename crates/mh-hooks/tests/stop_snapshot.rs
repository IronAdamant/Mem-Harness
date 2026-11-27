//! The stop check with git snapshots: shell edits are seen, the user's
//! own edits are not blamed on a turn that ran no tools, and one set of
//! changes is asked about once. Each test makes its own temporary git
//! repository; they return early when git is not installed.

use mh_core::Config;
use mh_hooks::memory::stop;
use mh_hooks::{run_command, run_hook};
use mh_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn has_git() -> bool {
    mh_core::project::find_command("git", &std::env::temp_dir()).is_some()
        && std::env::var_os("LIBELLUS_DIR").is_none()
}

fn git(dir: &Path, args: &[&str]) {
    let mut all = vec!["-c", "user.email=t@example.com", "-c", "user.name=t"];
    all.extend_from_slice(args);
    let r = run_command("git", &all, dir, Duration::from_secs(20)).unwrap();
    assert_eq!(r.code, Some(0), "git {args:?}: {r:?}");
}

/// A git repository with one commit and a Libellus store (ignored by git).
fn repo(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mh-snap-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".libellus")).unwrap();
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    std::fs::write(d.join(".gitignore"), ".libellus/\n").unwrap();
    git(&d, &["init", "-q"]);
    git(&d, &["add", "."]);
    git(&d, &["commit", "-q", "-m", "init"]);
    d
}

const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"do it"}}"#;
const BASH: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Bash","input":{"command":"sed -i x a.txt"}}]}}"#;
const RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}}"#;
const TEXT: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#;
const FEEDBACK: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Stop hook feedback:\nMem-Harness: this turn changed files"}}"#;
const NOTE: &str = r#"{"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<status>completed</status>\n</task-notification>"}}"#;

struct Session {
    cfg: Config,
    dir: PathBuf,
    transcript: PathBuf,
    id: String,
}

impl Session {
    fn new(name: &str) -> Session {
        let dir = repo(name);
        let mut cfg = Config::builtin();
        cfg.brief.command_timeout_s = 20;
        Session {
            cfg,
            transcript: dir.join(".libellus/t.jsonl"),
            id: format!("mh-test-{}-{name}", std::process::id()),
            dir,
        }
    }

    fn input(&self, active: bool) -> String {
        Value::obj(vec![
            ("session_id", Value::str(self.id.as_str())),
            (
                "transcript_path",
                Value::str(self.transcript.display().to_string()),
            ),
            ("stop_hook_active", Value::Bool(active)),
        ])
        .to_json()
    }

    fn prompt_submit(&self) {
        let out = run_hook(&self.cfg, &self.dir, "prompt-submit", &self.input(false)).unwrap();
        assert_eq!(out, "{}");
    }

    fn stop_with(&self, lines: &[&str]) -> bool {
        std::fs::write(&self.transcript, lines.join("\n") + "\n").unwrap();
        let out = stop(&self.cfg, &self.dir, &self.input(false), false).unwrap();
        out.contains("\"block\"")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(p) = mh_hooks::snapshot::path_for(&self.id, "") {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A file changed through Bash is an edit; Bash that changed nothing is not.
#[test]
fn shell_edits_are_seen_and_reads_are_not() {
    if !has_git() {
        return;
    }
    let s = Session::new("shell");
    s.prompt_submit();
    assert!(
        !s.stop_with(&[PROMPT, BASH, RESULT, TEXT]),
        "nothing changed"
    );
    s.prompt_submit();
    std::fs::write(s.dir.join("a.txt"), "two\n").unwrap();
    assert!(s.stop_with(&[PROMPT, BASH, RESULT, TEXT]), "a.txt changed");
}

/// The user edits a file while the agent only talks: no tool ran, so the
/// change is not the turn's.
#[test]
fn user_edits_without_a_tool_call_are_not_the_turns() {
    if !has_git() {
        return;
    }
    let s = Session::new("user");
    s.prompt_submit();
    std::fs::write(s.dir.join("new.txt"), "mine\n").unwrap();
    assert!(!s.stop_with(&[PROMPT, TEXT]));
}

/// After a nudge, later turns that change nothing more (a background task
/// finishing, then a Bash call that only reads) are not nudged again; a
/// further change is.
#[test]
fn one_set_of_changes_is_asked_about_once() {
    if !has_git() {
        return;
    }
    let s = Session::new("once");
    s.prompt_submit();
    std::fs::write(s.dir.join("a.txt"), "two\n").unwrap();
    let first = [PROMPT, BASH, RESULT, TEXT];
    assert!(s.stop_with(&first));
    let later = [
        PROMPT, BASH, RESULT, TEXT, FEEDBACK, TEXT, NOTE, BASH, RESULT,
    ];
    assert!(
        !s.stop_with(&later),
        "the same changes were asked about again"
    );
    std::fs::write(s.dir.join("a.txt"), "three\n").unwrap();
    assert!(s.stop_with(&later), "a new change after the nudge");
}

/// A sub-agent's baseline is taken at SubagentStart and keyed by its id.
#[test]
fn subagent_baseline_comes_from_subagent_start() {
    if !has_git() {
        return;
    }
    let s = Session::new("sub");
    let agent_tx = s.dir.join(".libellus/agent.jsonl");
    let input = Value::obj(vec![
        ("session_id", Value::str(s.id.as_str())),
        ("agent_id", Value::str("a1b2c3d4")),
        ("agent_type", Value::str("Explore")),
        (
            "agent_transcript_path",
            Value::str(agent_tx.display().to_string()),
        ),
        (
            "transcript_path",
            Value::str(s.transcript.display().to_string()),
        ),
    ])
    .to_json();
    // A change made before the sub-agent started is not its own.
    std::fs::write(s.dir.join("a.txt"), "before\n").unwrap();
    run_hook(&s.cfg, &s.dir, "subagent-start", &input).unwrap();
    std::fs::write(&agent_tx, [PROMPT, BASH, RESULT, TEXT].join("\n") + "\n").unwrap();
    assert_eq!(stop(&s.cfg, &s.dir, &input, true).unwrap(), "{}");
    std::fs::write(s.dir.join("b.txt"), "by the agent\n").unwrap();
    let out = stop(&s.cfg, &s.dir, &input, true).unwrap();
    assert!(out.contains("agent=\\\"explore-a1b2c3d4\\\""), "{out}");
    let _ = std::fs::remove_file(mh_hooks::snapshot::path_for(&s.id, "a1b2c3d4").unwrap());
}

/// Outside git, the stop check falls back to the edit tools in the
/// transcript; a Bash call alone is not an edit there.
#[test]
fn outside_git_tool_names_decide() {
    if std::env::var_os("LIBELLUS_DIR").is_some() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mh-snap-{}-nogit", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".libellus")).unwrap();
    let t = dir.join("t.jsonl");
    std::fs::write(&t, [PROMPT, BASH, RESULT, TEXT].join("\n") + "\n").unwrap();
    let input = Value::obj(vec![
        (
            "session_id",
            Value::str(format!("mh-nogit-{}", std::process::id())),
        ),
        ("transcript_path", Value::str(t.display().to_string())),
    ])
    .to_json();
    let cfg = Config::builtin();
    // Only meaningful when the temp folder is not inside a git checkout.
    if mh_hooks::snapshot::take(&dir, Duration::from_secs(10), &run_command).is_none() {
        assert_eq!(stop(&cfg, &dir, &input, false).unwrap(), "{}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// When the last prompt lies before the part of the transcript that is
/// read, the turn is unknown and the agent may stop.
#[test]
fn no_prompt_in_the_window_allows_the_stop() {
    if std::env::var_os("LIBELLUS_DIR").is_some() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mh-snap-{}-window", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".libellus")).unwrap();
    let t = dir.join("t.jsonl");
    let edit = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Edit","input":{}}]}}"#;
    let filler = format!("{{\"type\":\"progress\",\"x\":\"{}\"}}\n", "x".repeat(1000));
    let mut s = String::from(PROMPT);
    s.push('\n');
    while s.len() < (9 << 20) {
        s.push_str(&filler);
    }
    s.push_str(edit);
    s.push('\n');
    std::fs::write(&t, &s).unwrap();
    let input = Value::obj(vec![(
        "transcript_path",
        Value::str(t.display().to_string()),
    )])
    .to_json();
    assert_eq!(stop(&Config::builtin(), &dir, &input, false).unwrap(), "{}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `git` with a fixed, old committer date, as commits fetched from
/// elsewhere have.
fn git_old(dir: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

/// A `git pull` (here a fast-forward to a commit made earlier) moves HEAD
/// and the files with it, but the turn edited nothing: no nudge.
#[test]
fn fast_forward_to_older_commits_is_not_an_edit() {
    if !has_git() {
        return;
    }
    let s = Session::new("pull");
    git(&s.dir, &["checkout", "-q", "-b", "upstream"]);
    std::fs::write(s.dir.join("a.txt"), "from elsewhere\n").unwrap();
    git(&s.dir, &["add", "a.txt"]);
    git_old(&s.dir, &["commit", "-q", "-m", "upstream work"]);
    git(&s.dir, &["checkout", "-q", "-"]);
    s.prompt_submit();
    git(&s.dir, &["merge", "-q", "--ff-only", "upstream"]);
    assert!(
        !s.stop_with(&[PROMPT, BASH, RESULT, TEXT]),
        "a pull was taken for an edit"
    );
    // Switching back is not an edit either.
    s.prompt_submit();
    git(&s.dir, &["checkout", "-q", "upstream"]);
    assert!(!s.stop_with(&[PROMPT, BASH, RESULT, TEXT]));
}

/// Edits committed within the turn leave a clean tree, but the new commit
/// is the turn's: nudged.
#[test]
fn a_commit_made_in_the_turn_is_an_edit() {
    if !has_git() {
        return;
    }
    let s = Session::new("commit");
    s.prompt_submit();
    std::fs::write(s.dir.join("a.txt"), "edited and committed\n").unwrap();
    git(&s.dir, &["add", "a.txt"]);
    git(&s.dir, &["commit", "-q", "-m", "turn work"]);
    assert!(s.stop_with(&[PROMPT, BASH, RESULT, TEXT]));
}
