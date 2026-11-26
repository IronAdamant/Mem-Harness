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

#[derive(Clone, Debug)]
pub struct BackendSpec {
    /// Short name; also the prefix of every tool the proxy exposes for it.
    pub name: String,
    /// `stdio-mcp` today; other kinds (HTTP, external APIs) plug in later.
    pub kind: String,
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
}

#[derive(Clone, Debug)]
pub struct Ownership {
    pub feature: String,
    /// A backend name, or `host`.
    pub owner: String,
    pub why: String,
    /// Backend name -> groups or tools to hide there.
    pub disable: BTreeMap<String, Vec<String>>,
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
    /// Where the config came from (`built-in` or a file path).
    pub source: String,
}

impl Config {
    pub fn builtin() -> Config {
        Config::from_json(DEFAULT_JSON, "built-in").expect("built-in config is valid")
    }

    /// The config for a project: `$MEM_HARNESS_CONFIG`, else
    /// `<project>/.mem-harness/harness.json`, else the built-in one.
    pub fn load(project: &Path) -> Result<Config, String> {
        let path = match std::env::var("MEM_HARNESS_CONFIG") {
            Ok(p) if !p.trim().is_empty() => Some(PathBuf::from(p)),
            _ => {
                let p = project.join(".mem-harness").join("harness.json");
                p.is_file().then_some(p)
            }
        };
        match path {
            None => Ok(Config::builtin()),
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                Config::from_json(&text, &p.display().to_string())
            }
        }
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

    fn validate(&self) -> Result<(), String> {
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
            if b.kind != "stdio-mcp" {
                return Err(format!(
                    "backend {:?}: unknown kind {:?} (stdio-mcp)",
                    b.name, b.kind
                ));
            }
            if b.command.trim().is_empty() {
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
    let main_agent = v
        .get("main_agent")
        .and_then(Value::as_str)
        .unwrap_or("main")
        .to_string();
    let brief = match v.get("brief") {
        None => BriefConfig {
            budget_chars: 8000,
            command_timeout_s: 10,
        },
        Some(b) => BriefConfig {
            budget_chars: b
                .get("budget_chars")
                .and_then(Value::as_i64)
                .unwrap_or(8000)
                .clamp(500, 100_000) as usize,
            command_timeout_s: b
                .get("command_timeout_s")
                .and_then(Value::as_i64)
                .unwrap_or(10)
                .clamp(1, 120) as u64,
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
        backends.push(BackendSpec {
            name: str_field(b, "name", &ctx)?,
            kind: b
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("stdio-mcp")
                .to_string(),
            command: str_field(b, "command", &ctx)?,
            args: str_list(b.get("args"), &format!("{ctx}.args"))?,
            env,
            disable_env: opt_str(b, "disable_env"),
            strip_prefix: opt_str(b, "strip_prefix"),
            disable,
            role: opt_str(b, "role"),
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
    let profile = match std::env::var("MEM_HARNESS_PROFILE") {
        Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => v
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("full")
            .to_string(),
    };

    Ok(Config {
        main_agent,
        brief,
        backends,
        ownership,
        routes,
        profile,
        profiles,
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
        assert!(Config::from_json(&selfdis, "t")
            .unwrap_err()
            .contains("cannot be disabled"));
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
            _ => Ok(Trim {
                what: str_field(d, "what", ctx)?,
                why: opt_str(d, "why"),
            }),
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
