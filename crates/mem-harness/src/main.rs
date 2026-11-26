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
  mem-harness hook <event>              run a Claude Code hook; event: session-start | subagent-start
  mem-harness brief [--agent NAME]      print the merged compaction brief
  mem-harness init                      create the Relatum, Libellus and Adsertio stores here if missing
  mem-harness ownership                 print who owns each feature and what is hidden where
  mem-harness doctor                    check config, project and every backend's binary
  mem-harness install                   print the .mcp.json entry and hook settings to paste
  mem-harness print-config              print the built-in harness.json (copy to .mem-harness/harness.json to edit)
  mem-harness --version

Environment: MEM_HARNESS_CONFIG (config file), MEM_HARNESS_PROJECT (project root).";

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
    let root = project::root();
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
                    "The merged brief (Libellus notes and board, Relatum rules, unverified Adsertio claims, open Chisel issues), read fresh. Call after compaction or when unsure what was decided.",
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
        run: Arc::new(move |_args: Value| Ok(text_result(init_report(&init_root)))),
    };
    vec![brief, init]
}

fn init_report(root: &Path) -> String {
    let out = mh_hooks::stores::init(root, &mh_hooks::run_command);
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
    let (root, _) = load()?;
    let out = mh_hooks::stores::init(&root, &mh_hooks::run_command);
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
        eprintln!("mem-harness hook: give an event (session-start | subagent-start)");
        return ExitCode::SUCCESS;
    };
    let mut stdin = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut stdin);
    match load().and_then(|(root, cfg)| mh_hooks::run_hook(&cfg, &root, event, &stdin)) {
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
    let root = project::root();
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
        match project::find_command(&b.command) {
            Some(p) => println!("ok   {:<9} {}", b.name, p.display()),
            None => {
                ok = false;
                println!("FAIL {:<9} `{}` not on PATH or in ~/.cargo/bin; install it or change `command` in harness.json", b.name, b.command);
            }
        }
    }
    for hook_cmd in ["libellus", "adsertio", "chisel"] {
        if project::find_command(hook_cmd).is_none() {
            ok = false;
            println!("FAIL brief    `{hook_cmd}` CLI not found; its brief section will say so");
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own path: {e}"))?;
    let exe = exe.display().to_string();
    let cmd = |args: &[&str]| -> Value {
        Value::obj(vec![
            ("type", Value::str("command")),
            ("command", Value::str(exe.clone())),
            (
                "args",
                Value::Array(args.iter().map(|a| Value::str(*a)).collect()),
            ),
        ])
    };
    let mcp = Value::obj(vec![(
        "mcpServers",
        Value::obj(vec![(
            "mem-harness",
            Value::obj(vec![
                ("type", Value::str("stdio")),
                ("command", Value::str(exe.clone())),
                ("args", Value::Array(vec![Value::str("mcp")])),
            ]),
        )]),
    )]);
    let hooks = Value::obj(vec![(
        "hooks",
        Value::obj(vec![
            (
                "SessionStart",
                Value::Array(vec![Value::obj(vec![
                    ("matcher", Value::str("startup|resume|clear|compact")),
                    ("hooks", Value::Array(vec![cmd(&["hook", "session-start"])])),
                ])]),
            ),
            (
                "SubagentStart",
                Value::Array(vec![Value::obj(vec![(
                    "hooks",
                    Value::Array(vec![cmd(&["hook", "subagent-start"])]),
                )])]),
            ),
        ]),
    )]);
    println!("# 1. Project .mcp.json (replaces the nine separate server entries for this project):\n{}\n", mcp.to_json());
    println!("# 2. .claude/settings.json hooks:\n{}\n", hooks.to_json());
    println!(
        "# 3. Remove the separate Libellus and Adsertio SessionStart/SubagentStart hooks from ~/.claude/settings.json,\n#    and the nine per-tool MCP servers for this project; otherwise the brief and tools appear twice."
    );
    Ok(())
}
