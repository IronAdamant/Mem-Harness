//! `mem-harness`: one MCP server and one set of hooks in front of the
//! nine suite tools. See DESIGN.md.

use mh_core::{config, identity, project, Config};
use mh_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
mem-harness: one MCP server and one set of hooks for the suite tools

Usage:
  mem-harness mcp                       serve MCP on stdio (what Claude Code starts)
  mem-harness hook <event>              run a Claude Code hook; event: session-start | subagent-start |
                                        pre-compact | post-compact | stop | subagent-stop
  mem-harness brief [--agent NAME]      print the merged compaction brief
  mem-harness init                      create the Relatum, Libellus and Adsertio stores here if missing
  mem-harness ownership                 print who owns each feature and what is hidden where
  mem-harness doctor                    check config, project and every backend's binary
  mem-harness install                   print the user-wide MCP entry and hook settings to paste
  mem-harness print-config              print the built-in harness.json (copy to ~/.mem-harness/harness.json to edit)
  mem-harness --version

Config: $MEM_HARNESS_CONFIG (an absolute path) or ~/.mem-harness/harness.json, else built in.
A project's .mem-harness/harness.json may set only: profile, brief, main_agent.
Environment: MEM_HARNESS_CONFIG, MEM_HARNESS_PROFILE (profile), MEM_HARNESS_PROJECT (project root).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let result = match cmd {
        "mcp" => no_args(rest).and_then(|_| cmd_mcp()),
        "hook" => return cmd_hook(rest),
        "brief" => cmd_brief(rest),
        "ownership" => no_args(rest).and_then(|_| cmd_ownership()),
        "init" => no_args(rest).and_then(|_| cmd_init()),
        "doctor" => return cmd_doctor(rest),
        "install" => no_args(rest).and_then(|_| cmd_install()),
        "print-config" => no_args(rest).map(|_| print!("{}", config::DEFAULT_JSON)),
        "--version" | "-V" | "version" => {
            println!("mem-harness {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mem-harness: {e}");
            ExitCode::FAILURE
        }
    }
}

fn no_args(rest: &[String]) -> Result<(), String> {
    match rest.first() {
        None => Ok(()),
        Some(a) => Err(format!("unexpected argument {a:?}")),
    }
}

fn load() -> Result<(PathBuf, Config), String> {
    let root = project::root()?;
    let cfg = Config::load(&root)?;
    Ok((root, cfg))
}

fn text_result(text: String) -> Value {
    Value::obj(vec![(
        "content",
        Value::Array(vec![Value::obj(vec![
            ("type", Value::str("text")),
            ("text", Value::str(text)),
        ])]),
    )])
}

fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    Value::obj(vec![
        ("type", Value::str("object")),
        ("properties", Value::obj(props)),
        (
            "required",
            Value::Array(required.iter().map(|r| Value::str(*r)).collect()),
        ),
        ("additionalProperties", Value::Bool(false)),
    ])
}

fn string_prop(desc: &str) -> Value {
    Value::obj(vec![
        ("type", Value::str("string")),
        ("description", Value::str(desc)),
    ])
}

fn natives(root: &Path, cfg: &Config) -> Vec<mh_proxy::NativeTool> {
    let brief_root = root.to_path_buf();
    let brief_cfg = cfg.clone();
    let brief = mh_proxy::NativeTool {
        def: Value::obj(vec![
            ("name", Value::str("harness_brief")),
            (
                "description",
                Value::str(
                    "The merged brief (Libellus notes and board, Relatum rules, unverified or refuted Adsertio claims, open Chisel issues), read fresh. Call after compaction or when unsure what was decided.",
                ),
            ),
            ("inputSchema", schema(vec![("agent", string_prop("Agent name; default main. Sub-agents pass their own."))], &[])),
        ]),
        run: Arc::new(move |args: Value| {
            let agent = args.get("agent").and_then(Value::as_str).unwrap_or(&brief_cfg.main_agent).to_string();
            if !identity::valid_agent(&agent) {
                return Err(format!("invalid agent {agent:?}: lowercase letters, digits and '-'"));
            }
            Ok(text_result(mh_hooks::compose_brief(&brief_cfg, &brief_root, &agent)))
        }),
    };
    let init_root = root.to_path_buf();
    let init_stores = cfg.stores.clone();
    let init = mh_proxy::NativeTool {
        def: Value::obj(vec![
            ("name", Value::str("harness_init")),
            (
                "description",
                Value::str(
                    "Create the Relatum, Libellus and Adsertio stores in this project if missing (one call; existing stores are never touched). Call when a note, board or claim tool says its store is missing.",
                ),
            ),
            ("inputSchema", schema(vec![], &[])),
        ]),
        run: Arc::new(move |_args: Value| Ok(text_result(init_report(&init_root, &init_stores)))),
    };
    vec![brief, init]
}

fn init_report(root: &Path, stores: &[config::StoreSpec]) -> String {
    let out = mh_hooks::stores::init(stores, root, &mh_hooks::run_command);
    let rows: Vec<Value> = out
        .iter()
        .map(|(tool, outcome)| {
            Value::obj(vec![
                ("tool", Value::str(*tool)),
                ("store", Value::str(outcome.clone())),
            ])
        })
        .collect();
    Value::obj(vec![
        ("project", Value::str(root.display().to_string())),
        ("stores", Value::Array(rows)),
    ])
    .to_json()
}

fn cmd_init() -> Result<(), String> {
    let (root, cfg) = load()?;
    let out = mh_hooks::stores::init(&cfg.stores, &root, &mh_hooks::run_command);
    let mut failed = false;
    for (tool, outcome) in &out {
        failed |= outcome.starts_with("failed");
        println!("{tool:<9} {outcome}");
    }
    if failed {
        Err("some stores could not be created".into())
    } else {
        Ok(())
    }
}

fn cmd_mcp() -> Result<(), String> {
    let (root, cfg) = load()?;
    let natives = natives(&root, &cfg);
    let input = std::io::BufReader::new(std::io::stdin());
    mh_proxy::serve(cfg, root, natives, input, std::io::stdout())
}

/// A hook never stops the session: errors go to stderr and the exit code is 0.
fn cmd_hook(rest: &[String]) -> ExitCode {
    let Some(event) = rest.first() else {
        eprintln!(
            "mem-harness hook: give an event: {}",
            mh_hooks::hook::EVENTS
        );
        return ExitCode::SUCCESS;
    };
    let mut stdin = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut stdin);
    // A hook never stops the session, even if something panics.
    let ran = std::panic::catch_unwind(|| {
        load().and_then(|(root, cfg)| mh_hooks::run_hook(&cfg, &root, event, &stdin))
    })
    .unwrap_or_else(|_| Err("internal error (panic); please report it".to_string()));
    match ran {
        Ok(out) => {
            let mut so = std::io::stdout();
            let _ = so.write_all(out.as_bytes());
            if !out.ends_with('\n') {
                let _ = so.write_all(b"\n");
            }
        }
        Err(e) => eprintln!("mem-harness hook {event}: {e}"),
    }
    ExitCode::SUCCESS
}

fn cmd_brief(rest: &[String]) -> Result<(), String> {
    let (root, cfg) = load()?;
    let agent = match rest {
        [] => cfg.main_agent.clone(),
        [flag, name] if flag == "--agent" => name.clone(),
        [one] if one.starts_with("--agent=") => one["--agent=".len()..].to_string(),
        _ => return Err("brief takes only --agent NAME".into()),
    };
    if !identity::valid_agent(&agent) {
        return Err(format!(
            "invalid agent {agent:?}: lowercase letters, digits and '-'"
        ));
    }
    println!("{}", mh_hooks::compose_brief(&cfg, &root, &agent));
    Ok(())
}

fn cmd_ownership() -> Result<(), String> {
    let (_, cfg) = load()?;
    println!("config: {}\n\nFeature owners:", cfg.source);
    for o in &cfg.ownership {
        println!("  {:<22} {:<9} {}", o.feature, o.owner, o.why);
    }
    println!("\nHidden per backend (computed):");
    for b in &cfg.backends {
        let hidden = cfg.disabled_in(&b.name);
        let how = if b.disable_env.is_empty() {
            "-".to_string()
        } else {
            b.disable_env.clone()
        };
        println!(
            "  {:<9} {:<25} {}",
            b.name,
            how,
            if hidden.is_empty() {
                "(nothing)".into()
            } else {
                hidden.join(",")
            }
        );
        if !b.hide_tools.is_empty() {
            println!(
                "  {:<9} {:<25} {}",
                "",
                "proxy hides",
                b.hide_tools.join(",")
            );
        }
        if !b.only_tools.is_empty() {
            println!(
                "  {:<9} {:<25} {}",
                "",
                "proxy shows only",
                b.only_tools.join(",")
            );
        }
    }
    if !cfg.routes.is_empty() {
        println!("\nRoutes:");
        for r in &cfg.routes {
            println!("  {:<48} -> {}", r.question, r.tool);
        }
    }
    Ok(())
}

fn cmd_doctor(rest: &[String]) -> ExitCode {
    if let Err(e) = no_args(rest) {
        eprintln!("mem-harness doctor: {e}");
        return ExitCode::from(2);
    }
    let root = match project::root() {
        Ok(r) => r,
        Err(e) => {
            println!("FAIL project: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("project: {}", root.display());
    let cfg = match Config::load(&root) {
        Ok(c) => {
            println!("ok   config: {}", c.source);
            c
        }
        Err(e) => {
            println!("FAIL config: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut ok = true;
    if !root.join(".git").exists() {
        println!("warn project: no .git here; Chisel, Probatio and Trammel's ref checks need a git repository");
    }
    for b in &cfg.backends {
        match project::find_command(&b.command, &root) {
            Some(p) => println!("ok   {:<9} {}", b.name, p.display()),
            None => {
                ok = false;
                println!("FAIL {:<9} `{}` not on PATH or in ~/.cargo/bin; install it or change `command` in harness.json", b.name, b.command);
            }
        }
    }
    for (cli, needs) in cfg.cli_commands() {
        if project::find_command(&cli, &root).is_none() {
            ok = false;
            println!("FAIL cli      `{cli}` not found; needed by {needs}");
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The hook events `install` wires, as (settings key, matcher, mem-harness event).
const HOOKS: &[(&str, Option<&str>, &str)] = &[
    (
        "SessionStart",
        Some("startup|resume|clear|compact"),
        "session-start",
    ),
    ("SubagentStart", None, "subagent-start"),
    ("PreCompact", None, "pre-compact"),
    ("PostCompact", None, "post-compact"),
    ("Stop", None, "stop"),
    ("SubagentStop", None, "subagent-stop"),
];

fn cmd_install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own path: {e}"))?;
    let exe = exe.display().to_string();
    let hooks = Value::obj(vec![(
        "hooks",
        Value::obj(
            HOOKS
                .iter()
                .map(|(key, matcher, event)| {
                    let mut entry = Vec::new();
                    if let Some(m) = matcher {
                        entry.push(("matcher", Value::str(*m)));
                    }
                    entry.push((
                        "hooks",
                        Value::Array(vec![Value::obj(vec![
                            ("type", Value::str("command")),
                            ("command", Value::str(exe.clone())),
                            (
                                "args",
                                Value::Array(vec![Value::str("hook"), Value::str(*event)]),
                            ),
                        ])]),
                    ));
                    (*key, Value::Array(vec![Value::obj(entry)]))
                })
                .collect(),
        ),
    )]);
    if cfg!(windows) {
        let q = ps_quote(&exe);
        println!(
            "# 0. Back up both files first (PowerShell; Roll back in the README restores these):"
        );
        println!("$stamp = Get-Date -Format yyyyMMdd-HHmmss");
        println!(
            "Copy-Item \"$HOME\\.claude.json\" \"$HOME\\.claude.json.bak-mem-harness-$stamp\""
        );
        println!("Copy-Item \"$HOME\\.claude\\settings.json\" \"$HOME\\.claude\\settings.json.bak-mem-harness-$stamp\"\n");
        println!("# 1. Add the MCP server for every project (writes ~/.claude.json):");
        println!("claude mcp add --scope user mem-harness -- {q} mcp\n");
    } else {
        let q = sh_quote(&exe);
        println!("# 0. Back up both files first (Roll back in the README restores these):");
        println!("stamp=$(date +%Y%m%d-%H%M%S)");
        println!("cp ~/.claude.json ~/.claude.json.bak-mem-harness-$stamp");
        println!("cp ~/.claude/settings.json ~/.claude/settings.json.bak-mem-harness-$stamp\n");
        println!("# 1. Add the MCP server for every project (writes ~/.claude.json):");
        println!("claude mcp add --scope user mem-harness -- {q} mcp\n");
    }
    println!("#    and remove the per-tool servers it replaces, for example:");
    println!("#    claude mcp remove --scope user libellus   (likewise relatum, adsertio, chisel, ...)\n");
    println!("# 2. Merge this into ~/.claude/settings.json, replacing the Libellus and Adsertio hooks:\n{}\n", hooks.to_json());
    println!("# 3. Restart Claude Code; /mcp should show mem-harness connected.");
    println!("#    Old servers or hooks left in place show every tool and the brief twice.");
    Ok(())
}

/// Quote for a POSIX shell: `'...'`, with each `'` written `'\''`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote for PowerShell: `'...'`, with each `'` doubled.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_quotes_and_spaces() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("/it's/x"), "'/it'\\''s/x'");
        assert_eq!(ps_quote("C:\\it's"), "'C:\\it''s'");
    }

    #[test]
    fn sh_quote_round_trips_through_a_shell() {
        if cfg!(windows) {
            return;
        }
        for s in ["/it's here/x", "a\"b$c`d", "'; touch pwned; '"] {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), s);
        }
    }
}
