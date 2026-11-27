//! Regressions from the third review (of 0.4.0 and 0.4.1).

use mh_hooks::brief::neutralise;

/// A line of `-` or `=` under third-party text would make that text a
/// Markdown heading (setext), and alone it is a rule: both are bracketed.
#[test]
fn setext_underlines_and_rules_are_neutralised() {
    assert_eq!(neutralise("---------------"), "[---------------]");
    assert_eq!(neutralise("  ===  "), "  [===]");
    assert_eq!(neutralise("* * *"), "[* * *]");
    assert_eq!(neutralise("___"), "[___]");
    // Ordinary lines pass through.
    assert_eq!(neutralise("- a list item"), "- a list item");
    assert_eq!(neutralise("a -- b"), "a -- b");
    assert_eq!(neutralise(""), "");
    assert_eq!(neutralise("   "), "   ");
}

/// A transcript written with spaces after the colons (`"type": "user"`)
/// is read like the compact form Claude Code writes.
#[test]
fn spaced_transcript_json_is_read() {
    let cfg = mh_core::Config::builtin();
    let jsonl = concat!(
        r#"{"type": "user", "message": {"role": "user", "content": "do it"}}"#,
        "\n",
        r#"{"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "1", "name": "Edit", "input": {}}]}}"#,
        "\n"
    );
    let t = mh_hooks::memory::last_turn(jsonl, &cfg.brief);
    assert!(t.started && t.edited, "{t:?}");
}

/// More "not yet recorded" items than are saved per compaction: the
/// report says how many were left out.
#[test]
fn items_over_the_cap_are_reported() {
    use mh_hooks::CmdOut;
    use std::path::Path;
    use std::time::Duration;
    if std::env::var_os("LIBELLUS_DIR").is_some() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mh-review3-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".libellus")).unwrap();
    let mut list = String::new();
    for i in 0..25 {
        list.push_str(&format!("- finding: f{i}\n"));
    }
    let stdin = mh_json::Value::obj(vec![(
        "compact_summary",
        mh_json::Value::str(format!("## Not yet recorded\n{list}")),
    )])
    .to_json();
    let run = |_: &str, args: &[&str], _: &Path, _: Duration| -> Result<CmdOut, String> {
        let out = if args.first() == Some(&"ls") {
            r#"{"notes":[]}"#.to_string()
        } else {
            String::new()
        };
        Ok(CmdOut {
            code: Some(0),
            stdout: out,
            stderr: String::new(),
        })
    };
    let r = mh_hooks::memory::post_compact(&mh_core::Config::builtin(), &dir, &stdin, &run);
    let _ = std::fs::remove_dir_all(&dir);
    let msg = r.unwrap_or_else(|e| e);
    assert!(msg.contains("5 more item(s) not saved"), "{msg}");
}
