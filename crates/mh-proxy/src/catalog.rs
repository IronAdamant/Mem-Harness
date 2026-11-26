//! The merged tool catalog: native tools first, then every backend's tools
//! under their exposed names.

use mh_core::Config;
use mh_json::Value;
use std::collections::HashMap;

/// Where an exposed tool name leads.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// The built-in `harness_status`.
    Status,
    /// `natives[i]`.
    Native(usize),
    /// Backend `i`, with the tool's own (unprefixed) name.
    Backend(usize, String),
}

#[derive(Default)]
pub struct Catalog {
    /// The tool objects as `tools/list` returns them.
    pub tools: Vec<Value>,
    pub targets: HashMap<String, Target>,
    /// Exposed tool count per backend.
    pub counts: Vec<usize>,
    /// `tools` as JSON, to tell whether a rebuild changed anything.
    pub json: String,
}

impl Catalog {
    /// Build from the native definitions (index 0 is `harness_status`, the
    /// rest are `natives[i - 1]`) and each backend's name and raw tools.
    /// Returns the catalog and one message per name collision.
    pub fn build(
        cfg: &Config,
        native_defs: &[Value],
        backends: &[(&str, &[Value])],
    ) -> (Catalog, Vec<String>) {
        let mut cat = Catalog {
            counts: vec![0; backends.len()],
            ..Catalog::default()
        };
        let mut notes = Vec::new();
        let mut owner_of: HashMap<String, String> = HashMap::new();
        for (i, def) in native_defs.iter().enumerate() {
            let Some(name) = def.get("name").and_then(Value::as_str) else {
                notes.push(format!("native tool #{i} has no name; skipped"));
                continue;
            };
            if let Some(first) = owner_of.get(name) {
                notes.push(format!(
                    "tool {name:?}: native tool #{i} collides with {first}; kept the first"
                ));
                continue;
            }
            let target = if i == 0 {
                Target::Status
            } else {
                Target::Native(i - 1)
            };
            owner_of.insert(name.to_string(), "a native tool".to_string());
            cat.targets.insert(name.to_string(), target);
            cat.tools.push(def.clone());
        }
        for (bi, (bname, tools)) in backends.iter().enumerate() {
            for tool in tools.iter() {
                let Some(orig) = tool.get("name").and_then(Value::as_str) else {
                    notes.push(format!(
                        "backend {bname:?} listed a tool without a name; skipped"
                    ));
                    continue;
                };
                let exposed = cfg.exposed_name(bname, orig);
                if let Some(first) = owner_of.get(&exposed) {
                    notes.push(format!(
                        "tool {exposed:?}: {bname}'s {orig:?} collides with {first}; kept the first"
                    ));
                    continue;
                }
                let mut def = tool.clone();
                def.set("name", Value::str(exposed.clone()));
                owner_of.insert(exposed.clone(), format!("{bname}'s {orig:?}"));
                cat.targets
                    .insert(exposed, Target::Backend(bi, orig.to_string()));
                cat.tools.push(def);
                cat.counts[bi] += 1;
            }
        }
        cat.json = Value::Array(cat.tools.clone()).to_json();
        (cat, notes)
    }

    /// Exposed names that look like `name` under another prefix: the same
    /// suffix after stripping a known backend (or `harness`) prefix, else
    /// after the first `_`.
    pub fn similar(&self, name: &str, prefixes: &[&str]) -> Vec<String> {
        let suffix = prefixes
            .iter()
            .filter_map(|p| name.strip_prefix(p).and_then(|r| r.strip_prefix('_')))
            .filter(|s| !s.is_empty())
            .min_by_key(|s| s.len())
            .or_else(|| {
                name.split_once('_')
                    .map(|(_, rest)| rest)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or(name);
        let mut out: Vec<String> = self
            .targets
            .keys()
            .filter(|k| {
                k.as_str() != name && (k.as_str() == suffix || k.ends_with(&format!("_{suffix}")))
            })
            .cloned()
            .collect();
        out.sort();
        out.truncate(3);
        out
    }
}
