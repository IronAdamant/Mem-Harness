//! Regression tests for defects found in review, against the public API.
//! The Libellus cases run the real `libellus` CLI when it is installed.

use mh_core::Config;
use mh_hooks::memory::{last_turn, post_compact, unrecorded_items};
use mh_hooks::{assemble, run_command, Section};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mh-regress-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The timeout holds when the child exits but a background process it
/// started keeps the stdout pipe open.
#[cfg(unix)]
#[test]
fn timeout_holds_when_a_grandchild_keeps_the_pipe() {
    let t = std::time::Instant::now();
    let r = run_command(
        "/bin/sh",
        &["-c", "sleep 4 & echo hi"],
        &std::env::temp_dir(),
        Duration::from_secs(1),
    );
    let took = t.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "run_command took {took:?} with a 1s timeout (result {r:?})"
    );
    assert_eq!(r.unwrap().stdout, "hi\n");
}

/// A background-task notification arriving mid-turn is not a prompt, so the
/// edits made before it still count for the turn.
#[test]
fn task_notification_mid_turn_keeps_the_edits() {
    let prompt = r#"{"type":"user","message":{"role":"user","content":"do it"}}"#;
    let edit = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"1","name":"Edit","input":{}}]}}"#;
    let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}}"#;
    let notif = r#"{"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>"}}"#;
    let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#;
    let jsonl = [prompt, edit, result, notif, text].join("\n") + "\n";
    assert!(
        last_turn(&jsonl, &Config::builtin().brief).edited,
        "edit before the notification was dropped"
    );
}

/// Nested and fenced summaries keep their unrecorded items.
#[test]
fn unrecorded_heading_variants() {
    let h3 = "## Summary\n### Not yet recorded\n- decision: a\n";
    assert_eq!(unrecorded_items(h3).len(), 1, "### heading missed");
    let fenced = "## Not yet recorded\n- decision: a\n```sh\n# run this\n```\n- finding: b\n";
    assert_eq!(
        unrecorded_items(fenced).len(),
        2,
        "fence comment ended section"
    );
}

fn has_libellus() -> bool {
    mh_core::project::find_command("libellus", &std::env::temp_dir()).is_some()
        && std::env::var_os("LIBELLUS_DIR").is_none()
}

fn libellus_notes(project: &Path) -> Vec<String> {
    let dir = project.join(".libellus/agents/main/notes");
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn libellus_init(p: &Path) {
    let r = run_command("libellus", &["init"], p, Duration::from_secs(10)).unwrap();
    assert_eq!(r.code, Some(0), "{r:?}");
}

/// A control character in one line no longer makes Libellus refuse the
/// whole batch.
#[test]
fn one_tab_does_not_lose_the_batch() {
    if !has_libellus() {
        return;
    }
    let p = tmp("tab");
    libellus_init(&p);
    let cfg = Config::builtin();
    let summary = "## Not yet recorded\n- decision: keep the cache in src/a.rs\n- finding: col\tseparated output\n";
    let stdin =
        mh_json::Value::obj(vec![("compact_summary", mh_json::Value::str(summary))]).to_json();
    let r = post_compact(&cfg, &p, &stdin, &run_command);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(
        libellus_notes(&p).len(),
        2,
        "notes: {:?}",
        libellus_notes(&p)
    );
}

/// The next compact's summary repeats the previous "Not yet recorded" list;
/// lines already saved from a compact are not saved again.
#[test]
fn repeated_compact_does_not_duplicate_notes() {
    if !has_libellus() {
        return;
    }
    let p = tmp("dup");
    libellus_init(&p);
    let cfg = Config::builtin();
    let stdin = r###"{"compact_summary":"## Not yet recorded\n- decision: keep the cache in src/a.rs\n- decision: keep the cache in src/a.rs\n"}"###;
    let first = post_compact(&cfg, &p, stdin, &run_command).unwrap();
    assert!(first.starts_with("saved 1 "), "{first}");
    let second = post_compact(&cfg, &p, stdin, &run_command).unwrap();
    assert!(second.starts_with("skipped 1"), "{second}");
    assert_eq!(
        libellus_notes(&p).len(),
        1,
        "notes: {:?}",
        libellus_notes(&p)
    );
}

/// Budget property: never over budget once the budget holds header+footer.
#[test]
fn budget_property() {
    let mut seed: u64 = 7;
    let mut rnd = |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n.max(1)
    };
    for _ in 0..3000 {
        let k = rnd(5) as usize;
        let secs: Vec<Section> = (0..k)
            .map(|i| {
                let n = rnd(30) as usize;
                let lines = (0..n).map(|_| "é".repeat(rnd(300) as usize)).collect();
                Section::new(rnd(60) as usize, &format!("t{i}"), lines)
            })
            .collect();
        let budget = 200 + rnd(9000) as usize;
        let out = assemble("main", &secs, budget);
        assert!(
            out.chars().count() <= budget,
            "{} > {budget}",
            out.chars().count()
        );
    }
}
