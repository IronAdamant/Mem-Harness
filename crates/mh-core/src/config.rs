//! `harness.json`: backends, the ownership table and question routes.

use mh_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The configuration shipped in the binary.
pub const DEFAULT_JSON: &str = include_str!("../default-harness.json");

/// The owner name for features the host (Claude Code) handles itself.
pub const HOST_OWNER: &str = "host";

#[derive(Clone, Debug)]
pub struct Trim {
    /// A group or tool name, as the backend's own `*_DISABLE` accepts.
    pub what: String,
    pub why: String,
}

/// Backend kinds `mh_backend::build` can start. A new kind adds its name
/// here and a match arm there; its own settings are read from
/// [`BackendSpec::raw`], so the spec itself does not change.
pub const KNOWN_KINDS: &[&str] = &["stdio-mcp"];

/// Variables every child keeps when `env_pass` restricts its environment.
pub const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "PROGRAMDATA",
    "PROGRAMFILES",
];

#[derive(Clone, Debug)]
pub struct BackendSpec {
    /// Short name; also the prefix of every tool the proxy exposes for it.
    pub name: String,
    /// One of [`KNOWN_KINDS`].
    pub kind: String,
    /// The program for `stdio-mcp` (empty for kinds that do not start one).
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The variable the backend reads to hide groups or tools; empty when none.
    pub disable_env: String,
    /// A prefix the backend already puts on its tool names (`relatum_`),
    /// stripped before the proxy adds `<name>_`.
    pub strip_prefix: String,
    pub disable: Vec<Trim>,
    pub role: String,
    /// Seconds to wait for the `initialize` reply (and each `tools/list` page).
    pub handshake_s: u64,
    /// Overall cap on one `tools/call`, in seconds.
    pub call_s: u64,
    /// When set, the child gets only [`BASE_ENV`], these variables and
    /// `env`; when `None` it inherits the harness's whole environment.
    pub env_pass: Option<Vec<String>>,
    /// The proxy hides the backend's tools whose own name matches one of
    /// these patterns (`*` matches any run of characters), whether or not
    /// the backend honours `disable_env`.
    pub hide_tools: Vec<String>,
    /// When not empty, the proxy shows only tools matching one of these.
    pub only_tools: Vec<String>,
    /// Leave processes the server started running when its connection ends
    /// (normally the whole process group is stopped with it).
    pub keep_children: bool,
    /// The whole JSON object of this backend; kind-specific settings live
    /// under its `settings` key.
    pub raw: Value,
}

impl BackendSpec {
    /// A `stdio-mcp` backend with every optional setting at its default.
    pub fn stdio(name: &str, command: &str, args: &[&str]) -> BackendSpec {
        BackendSpec {
            name: name.to_string(),
            kind: "stdio-mcp".to_string(),
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: Vec::new(),
            disable_env: String::new(),
            strip_prefix: String::new(),
            disable: Vec::new(),
            role: String::new(),
            handshake_s: 20,
            call_s: 3600,
            env_pass: None,
            hide_tools: Vec::new(),
            only_tools: Vec::new(),
            keep_children: false,
            raw: Value::obj(vec![]),
        }
    }
}

/// A store a tool keeps in the project, created by `mem-harness init`.
#[derive(Clone, Debug)]
pub struct StoreSpec {
    pub tool: String,
    /// The folder at or above the project root.
    pub dir: String,
    /// The variable that moves it, or empty.
    pub env: String,
    /// The command and arguments that create it.
    pub init: Vec<String>,
    /// How the stop nudge names this tool's save tool, for example
    /// `relatum_new for a rule every agent must follow`; `{agent}` is the
    /// agent's name. Empty: the nudge does not mention it.
    pub save_hint: String,
}

/// One section of the brief. `parse` picks the reader for the command's
/// output: `text` (stdout lines under `heading`), or one of the built-in
/// readers `libellus`, `relatum-rules` (no command; read natively),
/// `adsertio-claims` and `chisel-issues`.
#[derive(Clone, Debug)]
pub struct SectionSpec {
    pub name: String,
    pub parse: String,
    pub command: String,
    /// `{agent}` is replaced by the agent's name.
    pub args: Vec<String>,
    /// Share of the budget, relative to the other sections.
    pub weight: usize,
    /// For `text`: the heading line put above the output.
    pub heading: String,
    /// Run only when this path exists in the project (for example `.chisel`).
    pub only_if_exists: String,
    /// The MCP tool the brief's "trimmed" note points to; default `name`.
    pub tool: String,
}

/// The `parse` values a section may use.
pub const SECTION_PARSERS: &[&str] = &[
    "text",
    "libellus",
    "relatum-rules",
    "adsertio-claims",
    "chisel-issues",
];

#[derive(Clone, Debug)]
pub struct Ownership {
    pub feature: String,
    /// A backend name, or `host`.
    pub owner: String,
    pub why: String,
    /// Backend name -> groups or tools to hide there.
    pub disable: BTreeMap<String, Vec<String>>,
    /// Groups and tools of the owner that serve this feature. No trim may
    /// hide them (the feature's own name is always protected too).
    pub protect: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Route {
    pub question: String,
    /// The exposed (prefixed) tool name.
    pub tool: String,
}

#[derive(Clone, Debug)]
pub struct BriefConfig {
    pub budget_chars: usize,
    pub command_timeout_s: u64,
    /// Stop and SubagentStop: when a turn edited files but saved no note,
    /// rule or claim, ask once to save them before stopping.
    pub nudge_on_stop: bool,
    /// PostCompact: save the summary's "Not yet recorded" lines as Libellus notes.
    pub save_unrecorded_on_compact: bool,
    /// Tools that save memory, matched on the name after the last `__`.
    pub save_tools: Vec<String>,
    /// Tools that edit files.
    pub edit_tools: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub main_agent: String,
    pub brief: BriefConfig,
    pub backends: Vec<BackendSpec>,
    pub ownership: Vec<Ownership>,
    pub routes: Vec<Route>,
    /// The active profile's name (`full` means no profile trims).
    pub profile: String,
    /// Profile name -> backend -> extra trims. Profiles only hide more;
    /// they never re-enable what ownership hides.
    pub profiles: BTreeMap<String, BTreeMap<String, Vec<Trim>>>,
    pub stores: Vec<StoreSpec>,
    pub brief_sections: Vec<SectionSpec>,
    /// Where the config came from (`built-in`, a file path, plus any
    /// project overlay).
    pub source: String,
}

/// Top-level keys a project's `.mem-harness/harness.json` may set. It is
/// read from whatever repository Claude Code opens, so it can only pick a
/// profile the user defined and change brief settings. Everything that
/// runs commands, hides tools, or writes text the model reads as the
/// harness's guidance (`backends`, `stores`, `brief_sections`, `profiles`,
/// `ownership`, `routes`) comes only from the user's own config.
pub const PROJECT_KEYS: &[&str] = &["version", "profile", "brief", "main_agent"];

const ALL_KEYS: &[&str] = &[
    "version",
    "profile",
    "profiles",
    "brief",
    "main_agent",
    "routes",
    "ownership",
    "backends",
    "stores",
    "brief_sections",
    "$comment",
];

/// The user's own config: `~/.mem-harness/harness.json`.
pub fn user_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Path::new(&home).join(".mem-harness").join("harness.json"))
}

/// Does `pattern` match `name`? `*` matches any run of characters.
pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(at) => rest = &rest[at + mid.len()..],
            None => return false,
        }
    }
    true
}

impl Config {
    /// The config shipped in the binary (no environment, no files).
    pub fn builtin() -> Config {
        Config::from_json(DEFAULT_JSON, "built-in").expect("built-in config is valid")
    }

    /// The config for a project. The base is `$MEM_HARNESS_CONFIG`, else
    /// `~/.mem-harness/harness.json`, else the built-in one; these are the
    /// user's own and may name commands. `<project>/.mem-harness/harness.json`
    /// is then laid over it, limited to [`PROJECT_KEYS`]. Last,
    /// `$MEM_HARNESS_PROFILE` picks the profile. Every problem is an `Err`.
    pub fn load(project: &Path) -> Result<Config, String> {
        let explicit = match std::env::var("MEM_HARNESS_CONFIG") {
            Ok(p) if !p.trim().is_empty() => {
                let p = PathBuf::from(p.trim());
                // A relative path would be read from the folder Claude Code
                // was opened in, which is the repository: not the user's own.
                if !p.is_absolute() {
                    return Err(format!(
                        "MEM_HARNESS_CONFIG {} must be an absolute path",
                        p.display()
                    ));
                }
                Some(p)
            }
            _ => None,
        };
        let base_path = explicit
            .clone()
            .or_else(|| user_config_path().filter(|p| p.is_file()));
        let (mut v, mut source) = match &base_path {
            Some(p) => (read_json(p)?, p.display().to_string()),
            None => (
                mh_json::parse(DEFAULT_JSON).map_err(|e| format!("built-in: {e}"))?,
                "built-in".to_string(),
            ),
        };
        let overlay = project.join(".mem-harness").join("harness.json");
        let same = base_path.as_ref().is_some_and(|b| same_file(b, &overlay));
        if overlay.is_file() && !same {
            let o = read_json(&overlay)?;
            let shown = overlay.display().to_string();
            apply_overlay(&mut v, &o).map_err(|e| format!("{shown}: {e}"))?;
            source = format!("{source} + {shown}");
        }
        if let Some(p) = std::env::var("MEM_HARNESS_PROFILE")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
        {
            v.set("profile", Value::str(p));
            source = format!("{source} (MEM_HARNESS_PROFILE)");
        }
        let cfg = parse_config(&v, &source).map_err(|e| format!("{source}: {e}"))?;
        cfg.validate().map_err(|e| format!("{source}: {e}"))?;
        Ok(cfg)
    }

    pub fn from_json(text: &str, source: &str) -> Result<Config, String> {
        let v = mh_json::parse(text).map_err(|e| format!("{source}: {e}"))?;
        let cfg = parse_config(&v, source).map_err(|e| format!("{source}: {e}"))?;
        cfg.validate().map_err(|e| format!("{source}: {e}"))?;
        Ok(cfg)
    }

    pub fn backend(&self, name: &str) -> Option<&BackendSpec> {
        self.backends.iter().find(|b| b.name == name)
    }

    /// Everything to hide in one backend: its own trims, then every
    /// ownership entry that names it, in table order, without repeats.
    pub fn disabled_in(&self, backend: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: &str| {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(s)) {
                out.push(s.to_string());
            }
        };
        if let Some(b) = self.backend(backend) {
            for t in &b.disable {
                push(&t.what);
            }
        }
        if let Some(trims) = self
            .profiles
            .get(&self.profile)
            .and_then(|p| p.get(backend))
        {
            for t in trims {
                push(&t.what);
            }
        }
        for o in &self.ownership {
            if let Some(list) = o.disable.get(backend) {
                for s in list {
                    push(s);
                }
            }
        }
        out
    }

    /// The environment for a backend's process: its `env`, plus its
    /// `disable_env` set to the computed list (when there is one).
    pub fn backend_env(&self, backend: &str) -> Vec<(String, String)> {
        let Some(b) = self.backend(backend) else {
            return Vec::new();
        };
        let mut env = b.env.clone();
        let disabled = self.disabled_in(backend);
        if !b.disable_env.is_empty() && !disabled.is_empty() {
            env.retain(|(k, _)| k != &b.disable_env);
            env.push((b.disable_env.clone(), disabled.join(",")));
        }
        env
    }

    /// The name the proxy exposes for a backend's tool:
    /// `<backend>_<tool without strip_prefix>`.
    pub fn exposed_name(&self, backend: &str, tool: &str) -> String {
        let strip = self
            .backend(backend)
            .map(|b| b.strip_prefix.as_str())
            .unwrap_or("");
        let bare = if !strip.is_empty() {
            tool.strip_prefix(strip).unwrap_or(tool)
        } else {
            tool
        };
        format!("{backend}_{bare}")
    }

    /// Does the proxy show `backend`'s tool `own` (the name the backend
    /// lists)? A pattern may match that name or the name after
    /// `strip_prefix` (`attach` for Adsertio's `adsertio_attach`).
    pub fn tool_visible(&self, backend: &str, own: &str) -> bool {
        let Some(b) = self.backend(backend) else {
            return true;
        };
        let stripped = if b.strip_prefix.is_empty() {
            own
        } else {
            own.strip_prefix(b.strip_prefix.as_str()).unwrap_or(own)
        };
        let hit = |p: &String| glob(p, own) || glob(p, stripped);
        if b.hide_tools.iter().any(hit) {
            return false;
        }
        b.only_tools.is_empty() || b.only_tools.iter().any(hit)
    }

    /// Every program the hooks and `init` run, with what needs it.
    pub fn cli_commands(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut add = |cmd: &str, why: String| {
            if cmd.is_empty() {
                return;
            }
            match out.iter_mut().find(|(c, _)| c == cmd) {
                Some(slot) => slot.1 = format!("{}, {why}", slot.1),
                None => out.push((cmd.to_string(), why)),
            }
        };
        for s in &self.brief_sections {
            add(&s.command, format!("brief section {}", s.name));
        }
        for st in &self.stores {
            add(
                st.init.first().map(String::as_str).unwrap_or(""),
                format!("init of {}", st.tool),
            );
        }
        out
    }

    fn validate_backends_extra(&self) -> Result<(), String> {
        for b in &self.backends {
            for (k, _) in &b.env {
                if !valid_env_key(k) {
                    return Err(format!(
                        "backend {:?}: env key {k:?} is not a variable name",
                        b.name
                    ));
                }
            }
            if !b.disable_env.is_empty() {
                if !valid_env_key(&b.disable_env) {
                    return Err(format!(
                        "backend {:?}: disable_env {:?} is not a variable name",
                        b.name, b.disable_env
                    ));
                }
                if BASE_ENV
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(&b.disable_env))
                {
                    return Err(format!(
                        "backend {:?}: disable_env {:?} is a system variable",
                        b.name, b.disable_env
                    ));
                }
            }
            for k in b.env_pass.iter().flatten() {
                if !valid_env_key(k) {
                    return Err(format!(
                        "backend {:?}: env_pass {k:?} is not a variable name",
                        b.name
                    ));
                }
            }
            for p in b.hide_tools.iter().chain(&b.only_tools) {
                if p.trim().is_empty() {
                    return Err(format!("backend {:?}: empty tool pattern", b.name));
                }
            }
        }
        Ok(())
    }

    /// Every trim must reach the backend (it needs a `disable_env`), and no
    /// trim may hide a feature from the backend that owns it.
    fn validate_trims(&self) -> Result<(), String> {
        let mut aimed: Vec<(String, String, String)> = Vec::new();
        for b in &self.backends {
            for t in &b.disable {
                aimed.push((
                    b.name.clone(),
                    t.what.clone(),
                    format!("backend {:?}", b.name),
                ));
            }
        }
        for (name, by_backend) in &self.profiles {
            for (b, trims) in by_backend {
                for t in trims {
                    aimed.push((b.clone(), t.what.clone(), format!("profile {name:?}")));
                }
            }
        }
        for o in &self.ownership {
            for (b, list) in &o.disable {
                for what in list {
                    aimed.push((b.clone(), what.clone(), format!("feature {:?}", o.feature)));
                }
            }
        }
        for (b, what, ctx) in &aimed {
            let Some(spec) = self.backend(b) else {
                continue;
            };
            if spec.disable_env.is_empty() {
                return Err(format!(
                    "{ctx}: hides {what:?} in {b:?}, which has no disable_env, so nothing would be hidden; use hide_tools for it"
                ));
            }
            if !valid_trim(what) {
                return Err(format!(
                    "{ctx}: trim {what:?} in {b:?}: use one group or tool name (letters, digits, '_' and '-')"
                ));
            }
            if let Some(o) = self.ownership.iter().find(|o| {
                &o.owner == b
                    && (o.feature.eq_ignore_ascii_case(what)
                        || o.protect.iter().any(|p| p.eq_ignore_ascii_case(what)))
            }) {
                return Err(format!(
                    "{ctx}: hides {what:?} in {b:?}, which owns the feature {:?}",
                    o.feature
                ));
            }
        }
        Ok(())
    }

    fn validate_brief_parts(&self) -> Result<(), String> {
        let mut names = Vec::new();
        for s in &self.brief_sections {
            if s.name.trim().is_empty() {
                return Err("brief_sections: a section has no name".into());
            }
            if names.contains(&s.name) {
                return Err(format!("brief_sections: {:?} is listed twice", s.name));
            }
            names.push(s.name.clone());
            if !SECTION_PARSERS.contains(&s.parse.as_str()) {
                return Err(format!(
                    "brief_sections {:?}: parse {:?} is not one of {}",
                    s.name,
                    s.parse,
                    SECTION_PARSERS.join(", ")
                ));
            }
            if s.parse != "relatum-rules" && s.command.trim().is_empty() {
                return Err(format!("brief_sections {:?}: command is empty", s.name));
            }
            if s.parse == "text" && s.heading.trim().is_empty() {
                return Err(format!(
                    "brief_sections {:?}: a text section needs a heading",
                    s.name
                ));
            }
            if s.weight == 0 {
                return Err(format!(
                    "brief_sections {:?}: weight must be at least 1",
                    s.name
                ));
            }
        }
        for st in &self.stores {
            if st.tool.is_empty() || st.dir.is_empty() || st.init.is_empty() {
                return Err(format!("stores: {:?} needs tool, dir and init", st.tool));
            }
            if !st.env.is_empty() && !valid_env_key(&st.env) {
                return Err(format!(
                    "stores {:?}: env {:?} is not a variable name",
                    st.tool, st.env
                ));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if !crate::identity::valid_agent(&self.main_agent) {
            return Err(format!(
                "main_agent {:?}: use lowercase letters, digits and '-'",
                self.main_agent
            ));
        }
        self.validate_backends_extra()?;
        self.validate_trims()?;
        self.validate_brief_parts()?;
        if self.profile != "full" && !self.profiles.contains_key(&self.profile) {
            let mut names: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
            names.push("full");
            return Err(format!(
                "profile {:?} is not defined (profiles: {})",
                self.profile,
                names.join(", ")
            ));
        }
        for (name, by_backend) in &self.profiles {
            if name == "full" {
                return Err("profile \"full\" is built in (no trims); do not define it".into());
            }
            for (b, trims) in by_backend {
                if self.backend(b).is_none() {
                    return Err(format!("profile {name:?}: unknown backend {b:?}"));
                }
                if trims.iter().any(|t| t.what.eq_ignore_ascii_case("core")) {
                    return Err(format!(
                        "profile {name:?}: core cannot be disabled in {b:?}"
                    ));
                }
            }
        }
        let mut seen = Vec::new();
        for b in &self.backends {
            if !valid_name(&b.name) {
                return Err(format!("backend name {:?}: use a-z, 0-9 and '-'", b.name));
            }
            if b.name == HOST_OWNER {
                return Err(format!("backend name {:?} is reserved", b.name));
            }
            if seen.contains(&b.name) {
                return Err(format!("backend {:?} is listed twice", b.name));
            }
            seen.push(b.name.clone());
            if !KNOWN_KINDS.contains(&b.kind.as_str()) {
                return Err(format!(
                    "backend {:?}: unknown kind {:?} ({})",
                    b.name,
                    b.kind,
                    KNOWN_KINDS.join(", ")
                ));
            }
            if b.kind == "stdio-mcp" && b.command.trim().is_empty() {
                return Err(format!("backend {:?}: command is empty", b.name));
            }
            for t in &b.disable {
                if t.what.eq_ignore_ascii_case("core") {
                    return Err(format!("backend {:?}: core cannot be disabled", b.name));
                }
            }
        }
        let mut features = Vec::new();
        for o in &self.ownership {
            if features.contains(&o.feature) {
                return Err(format!("feature {:?} has two owners", o.feature));
            }
            features.push(o.feature.clone());
            if o.owner != HOST_OWNER && self.backend(&o.owner).is_none() {
                return Err(format!(
                    "feature {:?}: owner {:?} is not a backend",
                    o.feature, o.owner
                ));
            }
            for (b, list) in &o.disable {
                if self.backend(b).is_none() {
                    return Err(format!(
                        "feature {:?}: disables in unknown backend {:?}",
                        o.feature, b
                    ));
                }
                if b == &o.owner {
                    return Err(format!(
                        "feature {:?}: its owner {:?} cannot be disabled for it",
                        o.feature, b
                    ));
                }
                if list.iter().any(|s| s.eq_ignore_ascii_case("core")) {
                    return Err(format!(
                        "feature {:?}: core cannot be disabled in {:?}",
                        o.feature, b
                    ));
                }
            }
        }
        for r in &self.routes {
            if !self
                .backends
                .iter()
                .any(|b| r.tool.starts_with(&format!("{}_", b.name)))
            {
                return Err(format!(
                    "route {:?}: tool {:?} names no backend",
                    r.question, r.tool
                ));
            }
        }
        Ok(())
    }
}

fn read_json(p: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    mh_json::parse(&text).map_err(|e| format!("{}: {e}", p.display()))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Lay a project file over the base: each allowed key replaces the base's,
/// except `profiles` (merged by profile name) and `brief` (merged by key).
fn apply_overlay(base: &mut Value, overlay: &Value) -> Result<(), String> {
    let pairs = overlay.as_object().ok_or("must be a JSON object")?;
    for (k, val) in pairs {
        if k == "$comment" {
            continue;
        }
        if !PROJECT_KEYS.contains(&k.as_str()) {
            let user = user_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.mem-harness/harness.json".into());
            return Err(if ALL_KEYS.contains(&k.as_str()) {
                format!(
                    "\"{k}\" can run commands, hide tools or instruct the model, so a project file cannot set it; put it in {user} or $MEM_HARNESS_CONFIG"
                )
            } else {
                format!(
                    "unknown key \"{k}\" (a project file may set: {})",
                    PROJECT_KEYS.join(", ")
                )
            });
        }
        match (k.as_str(), base.get_mut(k), val) {
            ("profiles" | "brief", Some(Value::Object(into)), Value::Object(from)) => {
                for (pk, pv) in from {
                    match into.iter_mut().find(|(ek, _)| ek == pk) {
                        Some(slot) => slot.1 = pv.clone(),
                        None => into.push((pk.clone(), pv.clone())),
                    }
                }
            }
            _ => base.set(k, val.clone()),
        }
    }
    Ok(())
}

/// One group or tool name, as the backends' `*_DISABLE` lists take them.
/// Commas, spaces and control characters would split or break the list.
fn valid_trim(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Reject keys of `v` not in `known` (`ctx` names the object).
fn known_keys(v: &Value, known: &[&str], ctx: &str) -> Result<(), String> {
    if let Some(pairs) = v.as_object() {
        for (k, _) in pairs {
            if !known.contains(&k.as_str()) && k != "$comment" {
                return Err(format!(
                    "{ctx}: unknown key \"{k}\" (known: {})",
                    known.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// An environment variable name: letters, digits and `_`, not starting with a digit.
fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn str_field(v: &Value, key: &str, ctx: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{ctx}: \"{key}\" must be a string"))
}

/// Default memory-saving tools for the stop check (names after the last `__`).
pub const DEFAULT_SAVE_TOOLS: &[&str] = &[
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

/// Default file-editing tools for the stop check.
pub const DEFAULT_EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// An integer field: absent gives `default`; present must be a whole
/// number in `min..=max`, never silently replaced.
fn int_in(
    v: &Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
    ctx: &str,
) -> Result<u64, String> {
    let Some(x) = v.get(key) else {
        return Ok(default);
    };
    let n = match x {
        Value::Num(t) if t.bytes().all(|c| c.is_ascii_digit()) => t.parse::<u64>().ok(),
        _ => None,
    };
    match n {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(format!(
            "{ctx}.{key}: a whole number from {min} to {max} expected"
        )),
    }
}

fn bool_or(v: &Value, key: &str, default: bool, ctx: &str) -> Result<bool, String> {
    match v.get(key) {
        None => Ok(default),
        Some(x) => x
            .as_bool()
            .ok_or_else(|| format!("{ctx}.{key}: true or false expected")),
    }
}

fn opt_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn str_list(v: Option<&Value>, ctx: &str) -> Result<Vec<String>, String> {
    match v {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{ctx}: list of strings expected"))
            })
            .collect(),
        Some(_) => Err(format!("{ctx}: list of strings expected")),
    }
}

fn parse_config(v: &Value, _source: &str) -> Result<Config, String> {
    match v.get("version").and_then(Value::as_i64) {
        Some(1) => {}
        _ => return Err("\"version\" must be 1".into()),
    }
    if let Some(pairs) = v.as_object() {
        for (k, _) in pairs {
            if !ALL_KEYS.contains(&k.as_str()) {
                return Err(format!("unknown key \"{k}\""));
            }
        }
    }
    let main_agent = match v.get("main_agent") {
        None => "main".to_string(),
        Some(m) => m
            .as_str()
            .ok_or("\"main_agent\" must be a string")?
            .to_string(),
    };
    let b = v.get("brief").cloned().unwrap_or(Value::obj(vec![]));
    if b.as_object().is_none() {
        return Err("\"brief\" must be an object".into());
    }
    known_keys(
        &b,
        &[
            "budget_chars",
            "command_timeout_s",
            "nudge_on_stop",
            "save_unrecorded_on_compact",
            "save_tools",
            "edit_tools",
        ],
        "brief",
    )?;
    let brief = BriefConfig {
        budget_chars: int_in(&b, "budget_chars", 8000, 500, 100_000, "brief")? as usize,
        command_timeout_s: int_in(&b, "command_timeout_s", 10, 1, 120, "brief")?,
        nudge_on_stop: bool_or(&b, "nudge_on_stop", true, "brief")?,
        save_unrecorded_on_compact: bool_or(&b, "save_unrecorded_on_compact", true, "brief")?,
        save_tools: match b.get("save_tools") {
            None => DEFAULT_SAVE_TOOLS.iter().map(|s| s.to_string()).collect(),
            some => str_list(some, "brief.save_tools")?,
        },
        edit_tools: match b.get("edit_tools") {
            None => DEFAULT_EDIT_TOOLS.iter().map(|s| s.to_string()).collect(),
            some => str_list(some, "brief.edit_tools")?,
        },
    };

    let mut backends = Vec::new();
    for (i, b) in v
        .get("backends")
        .and_then(Value::as_array)
        .ok_or("\"backends\" must be a list")?
        .iter()
        .enumerate()
    {
        let ctx = format!("backends[{i}]");
        known_keys(
            b,
            &[
                "name",
                "kind",
                "command",
                "args",
                "env",
                "disable_env",
                "strip_prefix",
                "disable",
                "role",
                "handshake_s",
                "call_s",
                "env_pass",
                "hide_tools",
                "only_tools",
                "keep_children",
                "settings",
            ],
            &ctx,
        )?;
        let env = match b.get("env") {
            None => Vec::new(),
            Some(Value::Object(pairs)) => pairs
                .iter()
                .map(|(k, val)| {
                    val.as_str()
                        .map(|s| (k.clone(), s.to_string()))
                        .ok_or_else(|| format!("{ctx}.env.{k}: string expected"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(format!("{ctx}.env: object expected")),
        };
        let mut disable = Vec::new();
        if let Some(list) = b.get("disable") {
            for d in list
                .as_array()
                .ok_or_else(|| format!("{ctx}.disable: list expected"))?
            {
                disable.push(match d {
                    Value::Str(s) => Trim {
                        what: s.clone(),
                        why: String::new(),
                    },
                    _ => Trim {
                        what: str_field(d, "what", &ctx)?,
                        why: opt_str(d, "why"),
                    },
                });
            }
        }
        let kind = b
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("stdio-mcp")
            .to_string();
        backends.push(BackendSpec {
            name: str_field(b, "name", &ctx)?,
            command: match b.get("command") {
                None if kind != "stdio-mcp" => String::new(),
                _ => str_field(b, "command", &ctx)?,
            },
            kind,
            args: str_list(b.get("args"), &format!("{ctx}.args"))?,
            env,
            disable_env: opt_str(b, "disable_env"),
            strip_prefix: opt_str(b, "strip_prefix"),
            disable,
            role: opt_str(b, "role"),
            handshake_s: int_in(b, "handshake_s", 20, 1, 600, &ctx)?,
            call_s: int_in(b, "call_s", 3600, 1, 86_400, &ctx)?,
            env_pass: match b.get("env_pass") {
                None => None,
                some => Some(str_list(some, &format!("{ctx}.env_pass"))?),
            },
            hide_tools: str_list(b.get("hide_tools"), &format!("{ctx}.hide_tools"))?,
            only_tools: str_list(b.get("only_tools"), &format!("{ctx}.only_tools"))?,
            keep_children: bool_or(b, "keep_children", false, &ctx)?,
            raw: b.clone(),
        });
    }

    let mut ownership = Vec::new();
    if let Some(list) = v.get("ownership") {
        for (i, o) in list
            .as_array()
            .ok_or("\"ownership\" must be a list")?
            .iter()
            .enumerate()
        {
            let ctx = format!("ownership[{i}]");
            known_keys(o, &["feature", "owner", "why", "disable", "protect"], &ctx)?;
            let mut disable = BTreeMap::new();
            if let Some(d) = o.get("disable") {
                for (k, val) in d
                    .as_object()
                    .ok_or_else(|| format!("{ctx}.disable: object expected"))?
                {
                    disable.insert(
                        k.clone(),
                        str_list(Some(val), &format!("{ctx}.disable.{k}"))?,
                    );
                }
            }
            ownership.push(Ownership {
                feature: str_field(o, "feature", &ctx)?,
                owner: str_field(o, "owner", &ctx)?,
                why: opt_str(o, "why"),
                disable,
                protect: str_list(o.get("protect"), &format!("{ctx}.protect"))?,
            });
        }
    }

    let mut routes = Vec::new();
    if let Some(list) = v.get("routes") {
        for (i, r) in list
            .as_array()
            .ok_or("\"routes\" must be a list")?
            .iter()
            .enumerate()
        {
            let ctx = format!("routes[{i}]");
            known_keys(r, &["question", "tool"], &ctx)?;
            routes.push(Route {
                question: str_field(r, "question", &ctx)?,
                tool: str_field(r, "tool", &ctx)?,
            });
        }
    }

    let mut profiles = BTreeMap::new();
    if let Some(obj) = v.get("profiles") {
        for (name, by_backend) in obj.as_object().ok_or("\"profiles\" must be an object")? {
            let mut map = BTreeMap::new();
            for (b, list) in by_backend
                .as_object()
                .ok_or_else(|| format!("profiles.{name}: object expected"))?
            {
                map.insert(
                    b.clone(),
                    parse_trims(list, &format!("profiles.{name}.{b}"))?,
                );
            }
            profiles.insert(name.clone(), map);
        }
    }
    let profile = match v.get("profile") {
        None => "full".to_string(),
        Some(p) => p
            .as_str()
            .ok_or("\"profile\" must be a string")?
            .to_string(),
    };

    let mut stores = Vec::new();
    if let Some(list) = v.get("stores") {
        for (i, st) in list
            .as_array()
            .ok_or("\"stores\" must be a list")?
            .iter()
            .enumerate()
        {
            let ctx = format!("stores[{i}]");
            known_keys(st, &["tool", "dir", "env", "init", "save_hint"], &ctx)?;
            stores.push(StoreSpec {
                tool: str_field(st, "tool", &ctx)?,
                dir: str_field(st, "dir", &ctx)?,
                env: opt_str(st, "env"),
                init: str_list(st.get("init"), &format!("{ctx}.init"))?,
                save_hint: opt_str(st, "save_hint"),
            });
        }
    }
    let mut brief_sections = Vec::new();
    if let Some(list) = v.get("brief_sections") {
        for (i, sec) in list
            .as_array()
            .ok_or("\"brief_sections\" must be a list")?
            .iter()
            .enumerate()
        {
            let ctx = format!("brief_sections[{i}]");
            known_keys(
                sec,
                &[
                    "name",
                    "parse",
                    "command",
                    "args",
                    "weight",
                    "heading",
                    "only_if_exists",
                    "tool",
                ],
                &ctx,
            )?;
            brief_sections.push(SectionSpec {
                name: str_field(sec, "name", &ctx)?,
                parse: str_field(sec, "parse", &ctx)?,
                command: opt_str(sec, "command"),
                args: str_list(sec.get("args"), &format!("{ctx}.args"))?,
                weight: int_in(sec, "weight", 10, 0, 1000, &ctx)? as usize,
                heading: opt_str(sec, "heading"),
                only_if_exists: opt_str(sec, "only_if_exists"),
                tool: match opt_str(sec, "tool") {
                    t if t.is_empty() => str_field(sec, "name", &ctx)?,
                    t => t,
                },
            });
        }
    }

    Ok(Config {
        main_agent,
        brief,
        backends,
        ownership,
        routes,
        profile,
        profiles,
        stores,
        brief_sections,
        source: _source.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_is_valid_and_has_nine_backends() {
        let c = Config::builtin();
        assert_eq!(c.backends.len(), 9);
    }

    #[test]
    fn disable_env_is_computed_from_the_table() {
        let c = Config::builtin();
        let env = c.backend_env("stele");
        let v = env
            .iter()
            .find(|(k, _)| k == "STELE_DISABLE")
            .map(|(_, v)| v.as_str());
        // Backend trims, then the active profile's, then ownership's.
        assert_eq!(v, Some("embeddings,locks,messages,notes,kv"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "STELE_MCP_MODE" && v == "lite"));
        let hub = c.disabled_in("hub");
        assert_eq!(hub.first().map(String::as_str), Some("diagnostics"));
        assert_eq!(&hub[hub.len() - 2..], ["tasks", "spawn"]);
        // Adsertio has no disable variable and nothing to hide.
        assert!(c.backend_env("adsertio").is_empty());
        // An owner is never disabled for its own feature.
        assert!(!c
            .disabled_in("relatum")
            .iter()
            .any(|s| s.contains("constraint")));
    }

    #[test]
    fn exposed_names_strip_the_backend_prefix() {
        let c = Config::builtin();
        assert_eq!(c.exposed_name("relatum", "relatum_get"), "relatum_get");
        assert_eq!(c.exposed_name("hub", "acquire_lock"), "hub_acquire_lock");
        assert_eq!(c.exposed_name("chisel", "job_status"), "chisel_job_status");
    }

    #[test]
    fn rejects_two_owners_and_self_disable() {
        let two = DEFAULT_JSON.replacen("\"feature\": \"messages\"", "\"feature\": \"locks\"", 1);
        assert!(Config::from_json(&two, "t")
            .unwrap_err()
            .contains("two owners"));
        let selfdis = DEFAULT_JSON.replacen(
            "\"disable\": { \"stele\": [\"messages\"] }",
            "\"disable\": { \"hub\": [\"messages\"] }",
            1,
        );
        let e = Config::from_json(&selfdis, "t").unwrap_err();
        assert!(e.contains("owns the feature"), "{e}");
    }
}

fn parse_trims(list: &Value, ctx: &str) -> Result<Vec<Trim>, String> {
    let items = list
        .as_array()
        .ok_or_else(|| format!("{ctx}: list expected"))?;
    items
        .iter()
        .map(|d| match d {
            Value::Str(s) => Ok(Trim {
                what: s.clone(),
                why: String::new(),
            }),
            _ => {
                known_keys(d, &["what", "why"], ctx)?;
                Ok(Trim {
                    what: str_field(d, "what", ctx)?,
                    why: opt_str(d, "why"),
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn lean_is_default_and_full_turns_trims_off() {
        let lean = Config::builtin();
        assert_eq!(lean.profile, "lean");
        assert!(lean
            .disabled_in("stele")
            .contains(&"embeddings".to_string()));
        assert!(lean
            .disabled_in("chisel")
            .contains(&"start_job".to_string()));
        assert!(!lean
            .disabled_in("chisel")
            .contains(&"job_status".to_string()));
        let full_json = DEFAULT_JSON.replacen("\"profile\": \"lean\"", "\"profile\": \"full\"", 1);
        let full = Config::from_json(&full_json, "t").unwrap();
        assert!(!full
            .disabled_in("stele")
            .contains(&"embeddings".to_string()));
        // Ownership trims stay on in every profile.
        assert!(full.disabled_in("stele").contains(&"locks".to_string()));
    }

    #[test]
    fn unknown_profile_is_an_error() {
        let bad = DEFAULT_JSON.replacen("\"profile\": \"lean\"", "\"profile\": \"tiny\"", 1);
        assert!(Config::from_json(&bad, "t")
            .unwrap_err()
            .contains("not defined"));
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    fn with(edit: impl Fn(&mut Value)) -> Result<Config, String> {
        let mut v = mh_json::parse(DEFAULT_JSON).unwrap();
        edit(&mut v);
        Config::from_json(&v.to_json(), "t")
    }

    fn backend_mut<'a>(v: &'a mut Value, name: &str) -> &'a mut Value {
        let Some(Value::Array(list)) = v.get_mut("backends") else {
            panic!()
        };
        list.iter_mut()
            .find(|b| b.get("name").and_then(Value::as_str) == Some(name))
            .unwrap()
    }

    #[test]
    fn glob_and_tool_visibility() {
        assert!(glob("lock_*", "lock_take"));
        assert!(glob("*_lock", "take_lock"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(!glob("a*b*c", "aXcYb"));
        assert!(!glob("ab*ba", "aba"));
        assert!(glob("exact", "exact") && !glob("exact", "exactly"));
        let c = with(|v| {
            let b = backend_mut(v, "adsertio");
            b.set("hide_tools", mh_json::parse(r#"["attach"]"#).unwrap());
            let p = backend_mut(v, "probatio");
            p.set("only_tools", mh_json::parse(r#"["mutate_*"]"#).unwrap());
        })
        .unwrap();
        assert!(!c.tool_visible("adsertio", "adsertio_attach"));
        assert!(!c.tool_visible("adsertio", "attach"));
        assert!(c.tool_visible("adsertio", "adsertio_claim"));
        assert!(c.tool_visible("probatio", "mutate_run"));
        assert!(!c.tool_visible("probatio", "schema"));
    }

    #[test]
    fn trims_must_reach_a_backend_and_never_hide_an_owner() {
        let e = with(|v| {
            let Some(Value::Array(own)) = v.get_mut("ownership") else {
                panic!()
            };
            own[0].set(
                "disable",
                mh_json::parse(r#"{"adsertio":["claim"]}"#).unwrap(),
            );
        })
        .unwrap_err();
        assert!(e.contains("no disable_env"), "{e}");
        let e = with(|v| {
            let p = v.get_mut("profiles").unwrap().get_mut("lean").unwrap();
            p.set("hub", mh_json::parse(r#"["locks"]"#).unwrap());
        })
        .unwrap_err();
        assert!(e.contains("owns the feature"), "{e}");
    }

    #[test]
    fn values_are_checked_not_replaced() {
        for (key, val) in [
            ("budget_chars", "99999999999999999999"),
            ("budget_chars", "8000.0"),
            ("budget_chars", "1e4"),
            ("command_timeout_s", "0"),
            ("nudge_on_stop", "\"yes\""),
        ] {
            let e = with(|v| {
                v.get_mut("brief")
                    .unwrap()
                    .set(key, mh_json::parse(val).unwrap());
            })
            .unwrap_err();
            assert!(e.contains(key), "{key}={val}: {e}");
        }
        assert!(with(|v| v.set("main_agent", Value::str("Main Agent"))).is_err());
        assert!(with(|v| v.set("surprise", Value::Bool(true)))
            .unwrap_err()
            .contains("unknown key"));
        for (k, bad) in [("env", r#"{"A=B":"x"}"#), ("env", r#"{"":"x"}"#)] {
            assert!(
                with(|v| backend_mut(v, "stele").set(k, mh_json::parse(bad).unwrap())).is_err()
            );
        }
        assert!(with(|v| backend_mut(v, "stele").set("disable_env", Value::str("PATH"))).is_err());
        assert!(with(|v| backend_mut(v, "stele").set("handshake_s", Value::int(0))).is_err());
        let c = with(|v| backend_mut(v, "stele").set("handshake_s", Value::int(90))).unwrap();
        assert_eq!(c.backend("stele").unwrap().handshake_s, 90);
    }

    #[test]
    fn brief_sections_and_stores_are_in_config() {
        let c = Config::builtin();
        let names: Vec<&str> = c.brief_sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["libellus_brief", "relatum_ls", "adsertio_view", "issues"]
        );
        assert_eq!(c.stores.len(), 3);
        let clis: Vec<String> = c.cli_commands().into_iter().map(|(c, _)| c).collect();
        for want in ["libellus", "adsertio", "chisel", "relatum"] {
            assert!(clis.iter().any(|c| c == want), "{want}: {clis:?}");
        }
        let e = with(|v| {
            let Some(Value::Array(list)) = v.get_mut("brief_sections") else {
                panic!()
            };
            list.push(mh_json::parse(r#"{"name":"x","parse":"text","command":"x"}"#).unwrap());
        })
        .unwrap_err();
        assert!(e.contains("heading"), "{e}");
    }

    /// Env and files are process-wide, so every `load` case runs here, in order.
    #[test]
    fn load_trusts_only_the_users_config() {
        let dir = std::env::temp_dir().join(format!("mh-core-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("repo");
        std::fs::create_dir_all(project.join(".mem-harness")).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        std::env::remove_var("MEM_HARNESS_CONFIG");
        std::env::remove_var("MEM_HARNESS_PROFILE");
        let overlay = project.join(".mem-harness").join("harness.json");

        // A repository may not name commands.
        std::fs::write(&overlay, r#"{"version":1,"backends":[]}"#).unwrap();
        let e = Config::load(&project).unwrap_err();
        assert!(e.contains("cannot set it"), "{e}");
        std::fs::write(&overlay, r#"{"brief_sections":[]}"#).unwrap();
        assert!(Config::load(&project)
            .unwrap_err()
            .contains("cannot set it"));

        // It may pick a profile and change brief settings, merged by key.
        std::fs::write(
            &overlay,
            r#"{"profile":"full","brief":{"nudge_on_stop":false}}"#,
        )
        .unwrap();
        let c = Config::load(&project).unwrap();
        assert_eq!(c.profile, "full");
        assert!(!c.brief.nudge_on_stop);
        assert_eq!(c.brief.budget_chars, 8000);
        assert_eq!(c.backends.len(), 9);
        assert!(c.source.contains(" + "), "{}", c.source);

        // The user's own file may name commands.
        std::fs::remove_file(&overlay).unwrap();
        std::fs::create_dir_all(home.join(".mem-harness")).unwrap();
        let user = home.join(".mem-harness").join("harness.json");
        let mut v = mh_json::parse(DEFAULT_JSON).unwrap();
        backend_mut(&mut v, "stele").set("command", Value::str("stele-custom"));
        std::fs::write(&user, v.to_json()).unwrap();
        let c = Config::load(&project).unwrap();
        assert_eq!(c.backend("stele").unwrap().command, "stele-custom");

        // A bad profile name is an error, never a panic.
        std::env::set_var("MEM_HARNESS_PROFILE", "Lean");
        let e = Config::load(&project).unwrap_err();
        assert!(e.contains("not defined"), "{e}");
        std::env::remove_var("MEM_HARNESS_PROFILE");

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
