//! The merged brief, composed from `Config.brief_sections` (built in:
//! Libellus, Relatum rules, Adsertio claims, Chisel issues).

use crate::run::{run_command, CmdOut};
use crate::stores::State;
use mh_core::config::SectionSpec;
use mh_core::Config;
use mh_json::Value;
use std::path::Path;
use std::time::Duration;

const FOOTER: &str = "Verdicts and rules above were read just now; re-read them (adsertio_view, relatum_get) instead of repeating them from memory.";
const MAX_RULES: usize = 15;

/// One block of the brief. `lines` includes its own heading.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// Relative share of the budget when space is short.
    pub weight: usize,
    /// The tool named in the `(trimmed; ...)` marker.
    pub tool: String,
    pub lines: Vec<String>,
}

impl Section {
    pub fn new(weight: usize, tool: &str, lines: Vec<String>) -> Section {
        Section {
            weight,
            tool: tool.to_string(),
            lines,
        }
    }

    fn chars(&self) -> usize {
        // Lines joined with "\n", plus the "\n\n" before the section.
        self.lines
            .iter()
            .map(|l| l.chars().count() + 1)
            .sum::<usize>()
            + 1
    }
}

impl Section {
    /// The least room that still shows the section: its first line and the
    /// trim marker (what [`trim`] needs to keep both).
    fn floor(&self) -> usize {
        let first = self.lines.first().map_or(0, |l| l.chars().count() + 1);
        first + marker(&self.tool).chars().count() + 3
    }
}

fn marker(tool: &str) -> String {
    format!("- (trimmed; call {tool} for the rest)")
}

/// Cut a section at a line boundary to fit `alloc` chars, ending in a
/// marker. `None` when not even the marker fits.
fn trim(sec: &Section, alloc: usize) -> Option<Vec<String>> {
    if sec.chars() <= alloc {
        return Some(sec.lines.clone());
    }
    let m = marker(&sec.tool);
    // "\n\n" before, "\n" before the marker.
    let room = alloc.checked_sub(m.chars().count() + 3)?;
    let mut used = 0;
    let mut kept = Vec::new();
    for l in &sec.lines {
        let c = l.chars().count() + 1;
        if used + c > room + 1 {
            break;
        }
        used += c;
        kept.push(l.clone());
    }
    kept.push(m);
    Some(kept)
}

/// Split `avail` among sections: first each gets its `floors` entry (room
/// for its first line and trim marker, so no section vanishes unannounced)
/// when all floors fit; the rest goes by weight.
fn allocate(needs: &[usize], weights: &[usize], floors: &[usize], avail: usize) -> Vec<usize> {
    let floors: Vec<usize> = needs
        .iter()
        .zip(floors)
        .map(|(n, f)| (*n).min(*f))
        .collect();
    let total: usize = floors.iter().sum();
    if total == 0 || total > avail {
        return by_weight(needs, weights, avail);
    }
    let rest: Vec<usize> = needs.iter().zip(&floors).map(|(n, f)| n - f).collect();
    by_weight(&rest, weights, avail - total)
        .into_iter()
        .zip(floors)
        .map(|(a, f)| a + f)
        .collect()
}

/// Split `avail` among sections by weight; a section that needs less than
/// its share frees the rest for the others.
fn by_weight(needs: &[usize], weights: &[usize], avail: usize) -> Vec<usize> {
    let mut alloc = vec![0; needs.len()];
    let mut done: Vec<bool> = needs.iter().map(|n| *n == 0).collect();
    let mut remaining = avail;
    loop {
        let open: Vec<usize> = (0..needs.len()).filter(|i| !done[*i]).collect();
        if open.is_empty() {
            break;
        }
        let wsum: usize = open.iter().map(|i| weights[*i].max(1)).sum();
        let mut changed = false;
        for &i in &open {
            let share = remaining * weights[i].max(1) / wsum;
            if needs[i] <= share {
                alloc[i] = needs[i];
                remaining -= needs[i];
                done[i] = true;
                changed = true;
            }
        }
        if !changed {
            for &i in &open {
                alloc[i] = remaining * weights[i].max(1) / wsum;
            }
            break;
        }
    }
    alloc
}

/// Pure assembly: header, the non-empty sections trimmed to `budget`
/// characters in total, footer. Never longer than `budget` unless the
/// budget cannot hold the header and footer.
pub fn assemble(agent: &str, sections: &[Section], budget: usize) -> String {
    let header = format!("# Mem-Harness brief for {agent}");
    let sections: Vec<&Section> = sections
        .iter()
        .filter(|s| s.lines.iter().any(|l| !l.trim().is_empty()))
        .collect();
    let fixed = header.chars().count() + 2 + FOOTER.chars().count();
    let needs: Vec<usize> = sections.iter().map(|s| s.chars()).collect();
    let weights: Vec<usize> = sections.iter().map(|s| s.weight).collect();
    let floors: Vec<usize> = sections.iter().map(|s| s.floor()).collect();
    let alloc = allocate(&needs, &weights, &floors, budget.saturating_sub(fixed));
    let mut out = header;
    for (s, a) in sections.iter().zip(alloc) {
        if let Some(lines) = trim(s, a) {
            out.push_str("\n\n");
            out.push_str(&lines.join("\n"));
        }
    }
    out.push_str("\n\n");
    out.push_str(FOOTER);
    out
}

fn one_line(s: &str, n: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n {
        t
    } else {
        format!("{}...", t.chars().take(n).collect::<String>())
    }
}

fn first_err_line(out: &CmdOut) -> String {
    let line = out
        .stderr
        .lines()
        .chain(out.stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("failed with no message");
    let line = line.strip_prefix("error: ").unwrap_or(line);
    one_line(line, 160)
}

fn text_lines(s: &str) -> Vec<String> {
    s.trim_end().lines().map(str::to_string).collect()
}

/// Lines from `libellus brief` output. A missing store is silent.
pub fn libellus_lines(label: &str, res: Result<CmdOut, String>) -> Option<Vec<String>> {
    match res {
        Err(e) => Some(vec![format!("- {label}: {}", one_line(&e, 160))]),
        Ok(o) if o.code != Some(0) => {
            if o.stderr.contains("no .libellus folder") {
                return None;
            }
            Some(vec![format!("- {label}: {}", first_err_line(&o))])
        }
        Ok(o) if o.stdout.trim().is_empty() => None,
        Ok(o) => Some(text_lines(&o.stdout)),
    }
}

/// Lines from the Relatum rules read natively. `None` is no store.
/// Damaged notes are named (they are not rules until fixed, and Adsertio
/// refuses them as proof), but never hide the good rules.
pub fn rules_lines(
    res: Option<Result<mh_adapters::relatum::Rules, String>>,
) -> Option<Vec<String>> {
    let found = match res? {
        Err(e) => return Some(vec![format!("- relatum: {}", one_line(&e, 160))]),
        Ok(r) => r,
    };
    if found.rules.is_empty() && found.damaged.is_empty() {
        return None;
    }
    let mut lines = vec!["## Rules (Relatum)".to_string()];
    for n in found.rules.iter().take(MAX_RULES) {
        lines.push(format!(
            "- {} ({}): {}",
            n.id,
            n.kind,
            one_line(&n.summary, 200)
        ));
    }
    if found.rules.len() > MAX_RULES {
        lines.push(format!(
            "- (+{} more: relatum_ls)",
            found.rules.len() - MAX_RULES
        ));
    }
    if !found.damaged.is_empty() {
        let shown: Vec<&str> = found
            .damaged
            .iter()
            .take(5)
            .map(|(id, _)| id.as_str())
            .collect();
        let more = found.damaged.len().saturating_sub(shown.len());
        let tail = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        lines.push(format!(
            "- DAMAGED, not rules until fixed (relatum_check says why): {}{tail}",
            shown.join(", ")
        ));
    }
    Some(lines)
}

/// Lines from `adsertio view --compact ...` JSON (`{"claims":[...]}`).
pub fn claims_lines(label: &str, res: Result<CmdOut, String>) -> Option<Vec<String>> {
    let err = |m: String| Some(vec![format!("- {label}: {m}")]);
    let o = match res {
        Err(e) => return err(one_line(&e, 160)),
        Ok(o) => o,
    };
    if o.code != Some(0) {
        return err(first_err_line(&o));
    }
    if o.stdout.trim().is_empty() {
        return None;
    }
    let v = match mh_json::parse(o.stdout.trim()) {
        Ok(v) => v,
        Err(e) => return err(format!("output is not JSON ({})", e.msg)),
    };
    let claims = match v
        .get("claims")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
    {
        Some(c) => c,
        None => return err("output has no claims list".to_string()),
    };
    if claims.is_empty() {
        return None;
    }
    let mut lines = vec!["## Claims unverified or refuted".to_string()];
    for c in claims {
        let f = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("?");
        lines.push(format!(
            "- [{}] {}: {}",
            f("verdict"),
            f("id"),
            one_line(f("summary"), 200)
        ));
    }
    Some(lines)
}

/// One line of a command's stdout made safe to show inside the brief:
/// control characters are dropped (a tab becomes a space), and a line that
/// would start with `#` gets its hashes put in brackets (`## x` becomes
/// `[##] x`), so the output cannot open a heading of its own and pass as
/// the harness's structure (a forged rules section or a second brief). A
/// line made only of `=`, `-`, `*` or `_` (and spaces) is bracketed whole
/// (`---` becomes `[---]`): under a line of text it would make that line a
/// heading, and alone it is a rule that splits the brief.
pub fn neutralise(line: &str) -> String {
    let clean: String = line
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let body = clean.trim_start();
    let trimmed = body.trim_end();
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| matches!(c, '=' | '-' | '*' | '_' | ' '))
    {
        let indent = &clean[..clean.len() - body.len()];
        return format!("{indent}[{trimmed}]");
    }
    let hashes = body.chars().take_while(|c| *c == '#').count();
    if hashes == 0 {
        return clean;
    }
    let indent = &clean[..clean.len() - body.len()];
    format!("{indent}[{}]{}", &body[..hashes], &body[hashes..])
}

/// Lines from a command whose stdout is shown under `heading`
/// (`chisel issues --brief` is one), each passed through [`neutralise`]:
/// the output is a third party's text, and only the harness writes
/// headings. Empty output is no section; a failure is one
/// `- <label>: <reason>` line.
pub fn text_section_lines(
    label: &str,
    heading: &str,
    res: Result<CmdOut, String>,
) -> Option<Vec<String>> {
    match res {
        Err(e) => Some(vec![format!("- {label}: {}", one_line(&e, 160))]),
        Ok(o) if o.code != Some(0) => Some(vec![format!("- {label}: {}", first_err_line(&o))]),
        Ok(o) if o.stdout.trim().is_empty() => None,
        Ok(o) => {
            let mut lines = vec![heading.to_string()];
            lines.extend(text_lines(&o.stdout).iter().map(|l| neutralise(l)));
            Some(lines)
        }
    }
}

const CHISEL_HEADING: &str = "## Open test issues";

/// The name a failure line starts with: the command's file name, or the
/// section's name when it has no command.
fn label(spec: &SectionSpec) -> String {
    Path::new(&spec.command)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&spec.name)
        .to_string()
}

/// One configured section: gate, run, read. `None` is no section.
fn cfg_store<'a>(stores: &'a [mh_core::StoreSpec], tool: &str) -> Option<&'a mh_core::StoreSpec> {
    stores.iter().find(|st| st.tool == tool)
}

fn run_section(
    spec: &SectionSpec,
    stores: &[mh_core::StoreSpec],
    project: &Path,
    agent: &str,
    timeout: Duration,
    run: &Runner,
) -> Option<Vec<String>> {
    if !spec.only_if_exists.is_empty() && !project.join(&spec.only_if_exists).exists() {
        return None;
    }
    if spec.parse == "relatum-rules" {
        // The Relatum store from config when listed there, else its
        // default; found with the same lookup as the Setup section.
        let store = match cfg_store(stores, "relatum") {
            Some(st) => crate::stores::locate(project, st).ok().flatten()?,
            None => mh_adapters::relatum::store_dir(project)?,
        };
        return rules_lines(Some(mh_adapters::relatum::active_rules(&store)));
    }
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.replace("{agent}", agent))
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = run(&spec.command, &args, project, timeout);
    match spec.parse.as_str() {
        "libellus" => libellus_lines(&label(spec), res),
        "adsertio-claims" => claims_lines(&label(spec), res),
        "chisel-issues" => text_section_lines(&label(spec), CHISEL_HEADING, res),
        "text" => text_section_lines(&spec.name, &spec.heading, res),
        other => Some(vec![format!("- {}: unknown parse {other:?}", spec.name)]),
    }
}

/// The Setup section: stores `harness_init` would create, and stores the
/// tools refuse (which it cannot fix).
fn setup_section(cfg: &Config, project: &Path) -> Option<Section> {
    let mut absent = Vec::new();
    let mut lines = vec!["## Setup".to_string()];
    let mut refused = Vec::new();
    for (tool, state) in crate::stores::missing(&cfg.stores, project) {
        match state {
            State::Refused(why) => refused.push(format!("- {tool}: {}", one_line(&why, 240))),
            _ => absent.push(tool),
        }
    }
    if absent.is_empty() && refused.is_empty() {
        return None;
    }
    if !absent.is_empty() {
        lines.push(format!(
            "- Not created yet here: {}. Call harness_init once before writing notes or claims.",
            absent.join(", ")
        ));
    }
    lines.extend(refused);
    Some(Section::new(5, "harness_init", lines))
}

type Runner<'a> = dyn Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + Sync + 'a;

/// Build the brief with an injectable command runner (for tests). The
/// sections are `cfg.brief_sections`, in order, each run on its own thread.
pub fn compose_brief_with(cfg: &Config, project: &Path, agent: &str, run: &Runner) -> String {
    let timeout = Duration::from_secs(cfg.brief.command_timeout_s.max(1));
    let stores = cfg.stores.as_slice();
    let built: Vec<Option<Section>> = std::thread::scope(|s| {
        let handles: Vec<_> = cfg
            .brief_sections
            .iter()
            .map(|spec| {
                (
                    spec,
                    s.spawn(move || run_section(spec, stores, project, agent, timeout, run)),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(spec, h)| {
                let lines = h
                    .join()
                    .unwrap_or_else(|_| Some(vec![format!("- {}: section panicked", spec.name)]))?;
                Some(Section::new(spec.weight, &spec.tool, lines))
            })
            .collect()
    });
    let mut sections: Vec<Section> = Vec::new();
    sections.extend(setup_section(cfg, project));
    sections.extend(built.into_iter().flatten());
    assemble(agent, &sections, cfg.brief.budget_chars)
}

/// Build the brief by running the real tools in `project`.
pub fn compose_brief(cfg: &Config, project: &Path, agent: &str) -> String {
    compose_brief_with(cfg, project, agent, &run_command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(w: usize, tool: &str, n: usize) -> Section {
        let mut lines = vec![format!("## {tool}")];
        lines.extend((0..n).map(|i| format!("- {tool} line {i} with some padding text")));
        Section::new(w, tool, lines)
    }

    fn all(n: [usize; 4]) -> Vec<Section> {
        vec![
            sec(50, "libellus_brief", n[0]),
            sec(20, "relatum_ls", n[1]),
            sec(20, "adsertio_view", n[2]),
            sec(10, "issues", n[3]),
        ]
    }

    #[test]
    fn fits_untouched() {
        let s = all([3, 3, 3, 3]);
        let out = assemble("main", &s, 10_000);
        assert!(out.starts_with("# Mem-Harness brief for main\n\n"));
        assert!(out.ends_with(FOOTER));
        assert!(!out.contains("trimmed"));
        assert!(out.contains("- issues line 2"));
    }

    #[test]
    fn budget_respected_and_priorities() {
        let s = all([100, 100, 100, 100]);
        for budget in [400, 800, 1500, 3000] {
            let out = assemble("main", &s, budget);
            assert!(
                out.chars().count() <= budget,
                "{} > {budget}",
                out.chars().count()
            );
        }
        let out = assemble("main", &s, 3000);
        let count = |t: &str| out.matches(&format!("- {t} line")).count();
        assert!(
            count("libellus_brief") > count("relatum_ls"),
            "libellus gets most"
        );
        assert!(count("relatum_ls") >= count("issues"));
        assert!(count("issues") > 0);
        for t in ["libellus_brief", "relatum_ls", "adsertio_view", "issues"] {
            assert!(
                out.contains(&format!("- (trimmed; call {t} for the rest)")),
                "{t}"
            );
        }
        // Libellus is about half of the space.
        let share = count("libellus_brief") as f64 * 40.0 / 3000.0;
        assert!(share > 0.35 && share < 0.55, "{share}");
    }

    #[test]
    fn unused_share_flows_to_others() {
        let mut s = all([100, 0, 0, 0]);
        s.truncate(1);
        let out = assemble("main", &s, 3000);
        assert!(out.chars().count() <= 3000);
        assert!(
            out.chars().count() > 2500,
            "libellus should take the whole budget"
        );
        assert!(!out.contains("## relatum_ls"));
    }

    #[test]
    fn small_sections_stay_whole_when_big_ones_trim() {
        let s = all([200, 2, 2, 2]);
        let out = assemble("main", &s, 2000);
        assert!(out.contains("- issues line 1"));
        assert!(out.contains("- relatum_ls line 1"));
        assert!(!out.contains("call issues"));
        assert!(out.contains("call libellus_brief"));
    }

    #[test]
    fn empty_sections_are_skipped() {
        let s = vec![
            Section::new(50, "libellus_brief", vec![]),
            Section::new(20, "x", vec!["  ".into()]),
            sec(10, "issues", 1),
        ];
        let out = assemble("a", &s, 1000);
        assert_eq!(out.matches("\n\n").count(), 2, "{out}");
        assert!(out.contains("## issues"));
    }

    #[test]
    fn tiny_budget_drops_sections() {
        let out = assemble("main", &all([5, 5, 5, 5]), 10);
        assert!(!out.contains("##"));
        assert!(out.ends_with(FOOTER));
    }

    fn ok(stdout: &str) -> Result<CmdOut, String> {
        Ok(CmdOut {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }
    fn fail(stderr: &str) -> Result<CmdOut, String> {
        Ok(CmdOut {
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }

    #[test]
    fn libellus_cases() {
        let l = |r| libellus_lines("libellus", r);
        assert!(l(fail(
            "error: no .libellus folder here or above. Run `libellus init`"
        ))
        .is_none());
        assert!(l(ok("  \n")).is_none());
        let s = l(ok(
            "# Libellus brief for main\n\n## Current state\n- none\n",
        ))
        .unwrap();
        assert_eq!(s[0], "# Libellus brief for main");
        assert_eq!(s.len(), 4);
        let s = l(Err("libellus not found on PATH".into())).unwrap();
        assert_eq!(s, ["- libellus: libellus not found on PATH"]);
        let s = l(fail("error: store is damaged\nmore")).unwrap();
        assert_eq!(s, ["- libellus: store is damaged"]);
    }

    #[test]
    fn claims_cases() {
        let claims_section = |r| claims_lines("adsertio", r);
        assert!(claims_section(ok("")).is_none());
        assert!(claims_section(ok(r#"{"claims":[],"counts":{}}"#)).is_none());
        let s = claims_section(ok(r#"{"claims":[{"id":"c1","summary":"Tests\npass","verdict":"refuted","evidence":[]},{"id":"c2","summary":"s","verdict":"unverified"}],"truncated":false}"#)).unwrap();
        assert_eq!(
            s,
            [
                "## Claims unverified or refuted",
                "- [refuted] c1: Tests pass",
                "- [unverified] c2: s"
            ]
        );
        assert!(
            claims_section(ok("garbage")).unwrap()[0].starts_with("- adsertio: output is not JSON")
        );
        assert!(claims_section(fail("error: boom")).unwrap()[0].contains("boom"));
    }

    #[test]
    fn chisel_cases() {
        let chisel_section = |r| text_section_lines("chisel", CHISEL_HEADING, r);
        assert!(chisel_section(ok("")).is_none());
        let s = chisel_section(ok("chisel: 1 open test issue(s): x.\nSuspects: y\n")).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "## Open test issues");
        assert_eq!(
            chisel_section(fail("error: boom")).unwrap(),
            ["- chisel: boom"]
        );
    }

    #[test]
    fn rules_cases() {
        use mh_adapters::relatum::{Note, Rules};
        let rules =
            |rules: Vec<Note>, damaged: Vec<(String, String)>| Some(Ok(Rules { rules, damaged }));
        assert!(rules_lines(None).is_none());
        assert!(rules_lines(rules(vec![], vec![])).is_none());
        let n = |i: usize| Note {
            id: format!("net/r{i}"),
            kind: "decision".into(),
            summary: format!("sum {i}"),
            status: "active".into(),
            superseded_by: None,
            tags: vec![],
            author: None,
            updated: None,
            body: String::new(),
        };
        let s = rules_lines(rules((0..17).map(n).collect(), vec![])).unwrap();
        assert_eq!(s.len(), 1 + 15 + 1);
        assert_eq!(s[1], "- net/r0 (decision): sum 0");
        assert_eq!(s[16], "- (+2 more: relatum_ls)");
        assert!(
            rules_lines(Some(Err("x: bad".into()))).unwrap()[0].starts_with("- relatum: x: bad")
        );
        // Damaged notes are named after the good rules and never hide them.
        let bad = |id: &str| (id.to_string(), "why".to_string());
        let s = rules_lines(rules(vec![n(0)], vec![bad("a/x"), bad("b/y")])).unwrap();
        assert_eq!(s[1], "- net/r0 (decision): sum 0");
        assert_eq!(
            s[2],
            "- DAMAGED, not rules until fixed (relatum_check says why): a/x, b/y"
        );
        let many: Vec<_> = (0..7).map(|i| bad(&format!("d/{i}"))).collect();
        let s = rules_lines(rules(vec![], many)).unwrap();
        assert!(
            s[1].ends_with("d/0, d/1, d/2, d/3, d/4 (+2 more)"),
            "{}",
            s[1]
        );
    }

    #[test]
    fn compose_with_fake_runner() {
        let cfg = Config::builtin();
        let dir = std::env::temp_dir().join(format!("mh-hooks-{}-brief", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".chisel")).unwrap();
        let runner =
            |prog: &str, _a: &[&str], cwd: &Path, _t: Duration| -> Result<CmdOut, String> {
                assert!(cwd.ends_with(dir.file_name().unwrap()));
                match prog {
                    "libellus" => ok("# Libellus brief for main\n- state"),
                    "adsertio" => {
                        ok(r#"{"claims":[{"id":"c1","summary":"s","verdict":"unverified"}]}"#)
                    }
                    _ => ok("chisel: 1 open"),
                }
            };
        let out = compose_brief_with(&cfg, &dir, "main", &runner);
        println!("{out}");
        assert!(out.contains("# Libellus brief for main"));
        assert!(out.contains("- [unverified] c1: s"));
        assert!(out.contains("## Open test issues"));
        assert!(out.chars().count() <= cfg.brief.budget_chars);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fake tools' output for the built-in sections, in a project with
    /// a Chisel store and no Relatum store.
    fn golden_runner(prog: &str, a: &[&str], _: &Path, _: Duration) -> Result<CmdOut, String> {
        match prog {
            "libellus" => {
                assert_eq!(a, ["brief", "--agent", "explore-1"]);
                ok("# Libellus brief for explore-1\n\n## Current state\n- state\n")
            }
            "adsertio" => ok(r#"{"claims":[{"id":"c1","summary":"s","verdict":"unverified"}]}"#),
            "chisel" => fail("error: chisel db locked"),
            other => panic!("unexpected {other}"),
        }
    }

    /// The built-in config gives exactly the brief the hard-coded sections
    /// gave before they moved into config.
    #[test]
    fn builtin_sections_are_byte_identical() {
        let cfg = Config::builtin();
        if cfg
            .stores
            .iter()
            .any(|s| std::env::var_os(&s.env).is_some())
        {
            return; // the environment points the tools elsewhere
        }
        let dir = std::env::temp_dir().join(format!("mh-hooks-{}-golden", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".chisel")).unwrap();
        let out = compose_brief_with(&cfg, &dir, "explore-1", &golden_runner);
        let want = "# Mem-Harness brief for explore-1\n\n\
## Setup\n\
- Not created yet here: relatum, libellus, adsertio. Call harness_init once before writing notes or claims.\n\n\
# Libellus brief for explore-1\n\n## Current state\n- state\n\n\
## Claims unverified or refuted\n- [unverified] c1: s\n\n\
- chisel: chisel db locked\n\n";
        assert_eq!(out, format!("{want}{FOOTER}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_sections_come_from_config() {
        let mut cfg = Config::builtin();
        let spec = |name: &str, cmd: &str, gate: &str| SectionSpec {
            name: name.into(),
            parse: "text".into(),
            command: cmd.into(),
            args: vec!["for".into(), "{agent}".into()],
            weight: 7,
            heading: format!("## {name} heading"),
            only_if_exists: gate.into(),
            tool: name.into(),
        };
        cfg.brief_sections = vec![
            spec("todo", "todo-cli", ""),
            spec("quiet", "quiet-cli", ""),
            spec("broken", "broken-cli", ""),
            spec("gated", "gated-cli", ".nope"),
        ];
        cfg.stores.clear();
        let dir = std::env::temp_dir().join(format!("mh-hooks-{}-text", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = |prog: &str, a: &[&str], _: &Path, _: Duration| -> Result<CmdOut, String> {
            assert_eq!(a, ["for", "main"]);
            match prog {
                "todo-cli" => ok("- one\n- two\n"),
                "quiet-cli" => ok("  \n"),
                "broken-cli" => Err("broken-cli not found on PATH".into()),
                other => panic!("{other} should not run"),
            }
        };
        let out = compose_brief_with(&cfg, &dir, "main", &runner);
        assert_eq!(
            out,
            format!(
                "# Mem-Harness brief for main\n\n## todo heading\n- one\n- two\n\n\
- broken: broken-cli not found on PATH\n\n{FOOTER}"
            )
        );
        let r = text_section_lines("x", "## X", fail("error: bad flag"));
        assert_eq!(r.unwrap(), ["- x: bad flag"]);
    }

    /// A store the tool refuses is named with its reason, not offered to
    /// harness_init.
    #[cfg(unix)]
    #[test]
    fn setup_names_a_refused_store() {
        let mut cfg = Config::builtin();
        cfg.brief_sections.clear();
        for s in &mut cfg.stores {
            s.env = format!("MH_TEST_UNSET_{}", s.tool.to_uppercase());
        }
        let dir = std::env::temp_dir().join(format!("mh-hooks-{}-setup", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::create_dir_all(dir.join(".swarmnotes")).unwrap();
        std::fs::create_dir_all(dir.join(".adsertio")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join(".libellus")).unwrap();
        let out = compose_brief_with(&cfg, &dir, "main", &golden_runner);
        assert!(!out.contains("Not created yet"), "{out}");
        assert!(
            out.contains("- libellus: ") && out.contains("is a symlink; libellus refuses it"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "runs the real libellus, adsertio and chisel"]
    fn real_commands() {
        let cfg = Config::builtin();
        let project = std::env::current_dir().unwrap();
        let out = compose_brief(&cfg, &project, "main");
        println!("{out}");
        assert!(out.starts_with("# Mem-Harness brief for main"));
    }
}
