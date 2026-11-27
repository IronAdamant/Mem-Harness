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
  mem-harness hook <event>              run a Claude Code hook; event: session-start | prompt-submit |
                                        subagent-start | pre-compact | post-compact | stop |
                                        subagent-stop
  mem-harness brief [--agent NAME]      print the merged compaction brief
  mem-harness init                      create the Relatum, Libellus and Adsertio stores here if missing
  mem-harness ownership                 print who owns each feature and what is hidden where
  mem-harness doctor                    check config, project, every backend's binary and where stores live
  mem-harness env [--powershell]        print the store variables, to run the tools by hand with the same stores
  mem-harness projects                  list the projects with stores kept by the harness
  mem-harness projects move OLD NEW     carry a renamed or moved project's stores to its new path
  mem-harness projects forget PROJECT   delete a project's stores (PROJECT: its path or id)
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
        "env" => cmd_env(rest),
        "projects" => cmd_projects(rest),
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
    prepare(&root, &cfg);
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

/// Before running anything for the project: commands the harness runs get
/// the backends' store variables, the project's store folders are
/// recorded, and the store folder names are kept out of git. Problems
/// here are reported, never fatal.
fn prepare(root: &Path, cfg: &Config) {
    mh_hooks::set_child_env(cfg.storage_env(root));
    let mut kinds: Vec<mh_core::storage::Kind> = Vec::new();
    for st in cfg.backends.iter().filter_map(|b| b.storage.as_ref()) {
        if !kinds.contains(&st.kind) {
            kinds.push(st.kind);
        }
    }
    for k in kinds {
        if let Err(e) = mh_core::storage::record(k, root) {
            eprintln!("mem-harness: {e}");
        }
    }
    if let Err(e) = mh_core::storage::ensure_git_exclude(root, &cfg.git_exclude) {
        eprintln!("mem-harness: {e}");
    }
}

fn cmd_mcp() -> Result<(), String> {
    // Windows: the backends must not hold Claude Code's pipes to us.
    mh_sys::stop_std_inheritance();
    let (root, cfg) = load()?;
    prepare(&root, &cfg);
    let natives = natives(&root, &cfg);
    let input = std::io::BufReader::new(std::io::stdin());
    mh_proxy::serve(cfg, root, natives, input, std::io::stdout())
}

/// A hook never stops the session: errors go to stderr and the exit code is 0.
fn cmd_hook(rest: &[String]) -> ExitCode {
    // Windows: what a hook command leaves running must not hold Claude
    // Code's pipes to us (see mh_hooks::run).
    mh_sys::stop_std_inheritance();
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
        load().and_then(|(root, cfg)| {
            prepare(&root, &cfg);
            mh_hooks::run_hook(&cfg, &root, event, &stdin)
        })
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
    prepare(&root, &cfg);
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
    for b in &cfg.backends {
        let Some(st) = &b.storage else { continue };
        match cfg.storage_dir(&b.name, &root) {
            Some(d) => println!(
                "ok   store     {:<9} {} ({})",
                b.name,
                d.display(),
                st.kind.as_str()
            ),
            None => {
                ok = false;
                println!(
                    "FAIL store     {:<9} no home folder to keep its store in",
                    b.name
                );
            }
        }
    }
    // Store folders left in the project by tools before the harness kept
    // them elsewhere (the opt-in memory stores are meant to be here).
    for name in &cfg.git_exclude {
        let name = name.trim_end_matches('/');
        let wanted = cfg.stores.iter().any(|st| st.dir == name);
        if !wanted && root.join(name).exists() {
            println!(
                "warn old store {} is no longer used (its store is kept outside the project now); delete it when you no longer need it",
                root.join(name).display()
            );
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_env(rest: &[String]) -> Result<(), String> {
    let ps = match rest {
        [] => false,
        [f] if f == "--powershell" => true,
        _ => return Err("env takes only --powershell".into()),
    };
    let (root, cfg) = load()?;
    for (k, v) in cfg.storage_env(&root) {
        if ps {
            println!("$env:{k} = {}", ps_quote(&v));
        } else {
            println!("export {k}={}", sh_quote(&v));
        }
    }
    for b in &cfg.backends {
        if let (Some(st), Some(d)) = (&b.storage, cfg.storage_dir(&b.name, &root)) {
            if !st.arg.is_empty() {
                println!("# {}: pass {} {}", b.command, st.arg, d.display());
            }
        }
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / f64::from(1u32 << 30)),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / f64::from(1u32 << 20)),
        b => format!("{} KB", b.div_ceil(1024)),
    }
}

fn cmd_projects(rest: &[String]) -> Result<(), String> {
    match rest {
        [] => {
            let all = mh_core::storage::list();
            if all.is_empty() {
                println!("no stores kept by the harness yet");
            }
            for e in all {
                let state = if e.exists { "" } else { "  (project gone)" };
                println!(
                    "{:<5} {:>8}  {}  {}{state}",
                    e.kind.as_str(),
                    human(e.bytes),
                    e.id,
                    if e.path.is_empty() { "?" } else { &e.path }
                );
            }
            Ok(())
        }
        [cmd, old, new] if cmd == "move" => {
            for line in mh_core::storage::move_project(old, Path::new(new))? {
                println!("moved {line}");
            }
            Ok(())
        }
        [cmd, what] if cmd == "forget" => {
            for d in mh_core::storage::forget(what)? {
                println!("removed {}", d.display());
            }
            Ok(())
        }
        _ => Err("projects takes: nothing, move OLD NEW, or forget PROJECT".into()),
    }
}

/// The hook events `install` wires, as (settings key, matcher, mem-harness event).
const HOOKS: &[(&str, Option<&str>, &str)] = &[
    (
        "SessionStart",
        Some("startup|resume|clear|compact"),
        "session-start",
    ),
    ("UserPromptSubmit", None, "prompt-submit"),
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
