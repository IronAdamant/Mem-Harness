//! Claude Code hook events.

use crate::brief::compose_brief;
use mh_core::{identity::subagent_name, Config};
use mh_json::Value;
use std::path::Path;

fn output(event: &str, text: &str) -> String {
    Value::obj(vec![(
        "hookSpecificOutput",
        Value::obj(vec![
            ("hookEventName", Value::str(event)),
            ("additionalContext", Value::str(text)),
        ]),
    )])
    .to_json()
}

/// The line that tells a sub-agent its name.
pub fn identity_line(name: &str) -> String {
    format!(
        "You are agent `{name}`. Pass agent=\"{name}\" to every libellus_* call and agent_id=\"{name}\" to every hub_* call, unless your prompt names another agent."
    )
}

fn subagent_from(stdin: &str) -> Result<String, String> {
    let v = mh_json::parse(stdin.trim())
        .map_err(|e| format!("subagent-start stdin is not JSON ({})", e.msg))?;
    if v.as_object().is_none() {
        return Err(
            "subagent-start stdin must be a JSON object with agent_type and/or agent_id"
                .to_string(),
        );
    }
    let field = |k: &str| match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => match x.as_str() {
            Some(s) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
            None => Err(format!("{k} must be a string")),
        },
    };
    let (kind, id) = (field("agent_type")?, field("agent_id")?);
    if kind.is_none() && id.is_none() {
        return Err("subagent-start stdin needs agent_type and/or agent_id".to_string());
    }
    Ok(subagent_name(
        kind.as_deref().unwrap_or(""),
        id.as_deref().unwrap_or(""),
    ))
}

/// Handle one hook event and return the one-line JSON for Claude Code's
/// stdout (no trailing newline).
pub fn run_hook(cfg: &Config, project: &Path, event: &str, stdin: &str) -> Result<String, String> {
    match event {
        "session-start" => Ok(output(
            "SessionStart",
            &compose_brief(cfg, project, &cfg.main_agent),
        )),
        "subagent-start" => {
            let name = subagent_from(stdin)?;
            let text = format!(
                "{}\n\n{}",
                identity_line(&name),
                compose_brief(cfg, project, &name)
            );
            Ok(output("SubagentStart", &text))
        }
        other => Err(format!(
            "unknown hook event \"{other}\"; use session-start or subagent-start"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_project() -> (Config, std::path::PathBuf) {
        let mut cfg = Config::builtin();
        cfg.brief.command_timeout_s = 1;
        let dir = std::env::temp_dir().join(format!("mh-hooks-{}-hook", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (cfg, dir)
    }

    fn parse(s: &str) -> Value {
        assert!(!s.contains('\n'));
        mh_json::parse(s).unwrap()
    }

    #[test]
    fn session_start_shape() {
        let (cfg, dir) = cfg_project();
        for stdin in ["", "{\"source\":\"compact\"}", "not json"] {
            let v = parse(&run_hook(&cfg, &dir, "session-start", stdin).unwrap());
            let h = v.get("hookSpecificOutput").unwrap();
            assert_eq!(
                h.get("hookEventName").and_then(Value::as_str),
                Some("SessionStart")
            );
            let t = h.get("additionalContext").and_then(Value::as_str).unwrap();
            assert!(t.starts_with(&format!("# Mem-Harness brief for {}", cfg.main_agent)));
        }
    }

    #[test]
    fn subagent_start_shape_and_identity() {
        let (cfg, dir) = cfg_project();
        let s = run_hook(
            &cfg,
            &dir,
            "subagent-start",
            r#"{"agent_type":"Explore","agent_id":"agent-a1b2c3d4e5f6"}"#,
        )
        .unwrap();
        let v = parse(&s);
        let h = v.get("hookSpecificOutput").unwrap();
        assert_eq!(
            h.get("hookEventName").and_then(Value::as_str),
            Some("SubagentStart")
        );
        let t = h.get("additionalContext").and_then(Value::as_str).unwrap();
        assert!(t.starts_with(&identity_line("explore-c3d4e5f6")), "{t}");
        assert!(t.contains("# Mem-Harness brief for explore-c3d4e5f6"));
        assert!(run_hook(&cfg, &dir, "subagent-start", r#"{"agent_id":"xyz"}"#).is_ok());
        assert!(run_hook(
            &cfg,
            &dir,
            "subagent-start",
            r#"{"agent_type":"general-purpose"}"#
        )
        .is_ok());
    }

    #[test]
    fn bad_input_is_an_error() {
        let (cfg, dir) = cfg_project();
        for bad in [
            "",
            "nope",
            "[]",
            "{}",
            r#"{"agent_type":3}"#,
            r#"{"agent_type":" "}"#,
        ] {
            assert!(
                run_hook(&cfg, &dir, "subagent-start", bad).is_err(),
                "{bad}"
            );
        }
        let e = run_hook(&cfg, &dir, "stop", "{}").unwrap_err();
        assert!(
            e.contains("session-start") && e.contains("subagent-start"),
            "{e}"
        );
    }

    #[test]
    fn identity_line_text() {
        let l = identity_line("x-1");
        assert!(
            l.contains("agent=\"x-1\"")
                && l.contains("agent_id=\"x-1\"")
                && l.contains("libellus_*")
                && l.contains("hub_*")
        );
    }
}
