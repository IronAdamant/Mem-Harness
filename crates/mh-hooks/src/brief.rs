//! The merged brief: Libellus, Relatum rules, Adsertio claims, Chisel issues.

use crate::run::{run_command, CmdOut};
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

/// Split `avail` among sections by weight; a section that needs less than
/// its share frees the rest for the others.
fn allocate(needs: &[usize], weights: &[usize], avail: usize) -> Vec<usize> {
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
    let alloc = allocate(&needs, &weights, budget.saturating_sub(fixed));
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

/// Section from `libellus brief` output. A missing store is silent.
pub fn libellus_section(res: Result<CmdOut, String>) -> Option<Section> {
    let w = 50;
    match res {
        Err(e) => Some(Section::new(
            w,
            "libellus_brief",
            vec![format!("- libellus: {}", one_line(&e, 160))],
        )),
        Ok(o) if o.code != Some(0) => {
            if o.stderr.contains("no .libellus folder") {
                return None;
            }
            Some(Section::new(
                w,
                "libellus_brief",
                vec![format!("- libellus: {}", first_err_line(&o))],
            ))
        }
        Ok(o) if o.stdout.trim().is_empty() => None,
        Ok(o) => Some(Section::new(w, "libellus_brief", text_lines(&o.stdout))),
    }
}

/// Section from the Relatum rules read natively. `None` is no store.
/// Damaged notes are named (they are not rules until fixed, and Adsertio
/// refuses them as proof), but never hide the good rules.
pub fn rules_section(res: Option<Result<mh_adapters::relatum::Rules, String>>) -> Option<Section> {
    let w = 20;
    let found = match res? {
        Err(e) => {
            return Some(Section::new(
                w,
                "relatum_ls",
                vec![format!("- relatum: {}", one_line(&e, 160))],
            ))
        }
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
    Some(Section::new(w, "relatum_ls", lines))
}

/// Section from `adsertio view --compact ...` JSON (`{"claims":[...]}`).
pub fn claims_section(res: Result<CmdOut, String>) -> Option<Section> {
    let w = 20;
    let err = |m: String| {
        Some(Section::new(
            w,
            "adsertio_view",
            vec![format!("- adsertio: {m}")],
        ))
    };
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
    let mut lines = vec!["## Claims not verified".to_string()];
    for c in claims {
        let f = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("?");
        lines.push(format!(
            "- [{}] {}: {}",
            f("verdict"),
            f("id"),
            one_line(f("summary"), 200)
        ));
    }
    Some(Section::new(w, "adsertio_view", lines))
}

/// Section from `chisel issues --brief` lines.
pub fn chisel_section(res: Result<CmdOut, String>) -> Option<Section> {
    let w = 10;
    let err = |m: String| Some(Section::new(w, "issues", vec![format!("- chisel: {m}")]));
    match res {
        Err(e) => err(one_line(&e, 160)),
        Ok(o) if o.code != Some(0) => err(first_err_line(&o)),
        Ok(o) if o.stdout.trim().is_empty() => None,
        Ok(o) => {
            let mut lines = vec!["## Open test issues".to_string()];
            lines.extend(text_lines(&o.stdout));
            Some(Section::new(w, "issues", lines))
        }
    }
}

type Runner<'a> = dyn Fn(&str, &[&str], &Path, Duration) -> Result<CmdOut, String> + Sync + 'a;

/// Build the brief with an injectable command runner (for tests).
pub fn compose_brief_with(cfg: &Config, project: &Path, agent: &str, run: &Runner) -> String {
    let timeout = Duration::from_secs(cfg.brief.command_timeout_s.max(1));
    let chisel = project.join(".chisel").exists();
    let (a, r, c, h) = std::thread::scope(|s| {
        let a = s.spawn(|| {
            libellus_section(run(
                "libellus",
                &["brief", "--agent", agent],
                project,
                timeout,
            ))
        });
        let r = s.spawn(|| {
            let store = mh_adapters::relatum::store_dir(project)?;
            rules_section(Some(mh_adapters::relatum::active_rules(&store)))
        });
        let c = s.spawn(|| {
            let args = [
                "view",
                "--compact",
                "--states",
                "unverified",
                "--states",
                "refuted",
                "--limit",
                "50",
                "--if-store",
            ];
            claims_section(run("adsertio", &args, project, timeout))
        });
        let h = s.spawn(|| {
            chisel
                .then(|| chisel_section(run("chisel", &["issues", "--brief"], project, timeout)))
                .flatten()
        });
        let j = |x: std::thread::ScopedJoinHandle<Option<Section>>, name: &str| {
            x.join().unwrap_or_else(|_| {
                Some(Section::new(
                    10,
                    name,
                    vec![format!("- {name}: section panicked")],
                ))
            })
        };
        (
            j(a, "libellus_brief"),
            j(r, "relatum_ls"),
            j(c, "adsertio_view"),
            j(h, "issues"),
        )
    });
    let mut sections: Vec<Section> = Vec::new();
    let missing = crate::stores::missing(project);
    if !missing.is_empty() {
        sections.push(Section::new(
            5,
            "harness_init",
            vec![
                "## Setup".to_string(),
                format!(
                    "- Not created yet here: {}. Call harness_init once before writing notes or claims.",
                    missing.join(", ")
                ),
            ],
        ));
    }
    sections.extend([a, r, c, h].into_iter().flatten());
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
        assert!(libellus_section(fail(
            "error: no .libellus folder here or above. Run `libellus init`"
        ))
        .is_none());
        assert!(libellus_section(ok("  \n")).is_none());
        let s = libellus_section(ok(
            "# Libellus brief for main\n\n## Current state\n- none\n",
        ))
        .unwrap();
        assert_eq!(s.lines[0], "# Libellus brief for main");
        assert_eq!(s.lines.len(), 4);
        let s = libellus_section(Err("libellus not found on PATH".into())).unwrap();
        assert_eq!(s.lines, ["- libellus: libellus not found on PATH"]);
        let s = libellus_section(fail("error: store is damaged\nmore")).unwrap();
        assert_eq!(s.lines, ["- libellus: store is damaged"]);
    }

    #[test]
    fn claims_cases() {
        assert!(claims_section(ok("")).is_none());
        assert!(claims_section(ok(r#"{"claims":[],"counts":{}}"#)).is_none());
        let s = claims_section(ok(r#"{"claims":[{"id":"c1","summary":"Tests\npass","verdict":"refuted","evidence":[]},{"id":"c2","summary":"s","verdict":"unverified"}],"truncated":false}"#)).unwrap();
        assert_eq!(
            s.lines,
            [
                "## Claims not verified",
                "- [refuted] c1: Tests pass",
                "- [unverified] c2: s"
            ]
        );
        assert!(claims_section(ok("garbage")).unwrap().lines[0]
            .starts_with("- adsertio: output is not JSON"));
        assert!(claims_section(fail("error: boom")).unwrap().lines[0].contains("boom"));
    }

    #[test]
    fn chisel_cases() {
        assert!(chisel_section(ok("")).is_none());
        let s = chisel_section(ok("chisel: 1 open test issue(s): x.\nSuspects: y\n")).unwrap();
        assert_eq!(s.lines.len(), 3);
        assert_eq!(s.lines[0], "## Open test issues");
    }

    #[test]
    fn rules_cases() {
        use mh_adapters::relatum::{Note, Rules};
        let rules =
            |rules: Vec<Note>, damaged: Vec<(String, String)>| Some(Ok(Rules { rules, damaged }));
        assert!(rules_section(None).is_none());
        assert!(rules_section(rules(vec![], vec![])).is_none());
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
        let s = rules_section(rules((0..17).map(n).collect(), vec![])).unwrap();
        assert_eq!(s.lines.len(), 1 + 15 + 1);
        assert_eq!(s.lines[1], "- net/r0 (decision): sum 0");
        assert_eq!(s.lines[16], "- (+2 more: relatum_ls)");
        assert!(rules_section(Some(Err("x: bad".into()))).unwrap().lines[0]
            .starts_with("- relatum: x: bad"));
        // Damaged notes are named after the good rules and never hide them.
        let bad = |id: &str| (id.to_string(), "why".to_string());
        let s = rules_section(rules(vec![n(0)], vec![bad("a/x"), bad("b/y")])).unwrap();
        assert_eq!(s.lines[1], "- net/r0 (decision): sum 0");
        assert_eq!(
            s.lines[2],
            "- DAMAGED, not rules until fixed (relatum_check says why): a/x, b/y"
        );
        let many: Vec<_> = (0..7).map(|i| bad(&format!("d/{i}"))).collect();
        let s = rules_section(rules(vec![], many)).unwrap();
        assert!(
            s.lines[1].ends_with("d/0, d/1, d/2, d/3, d/4 (+2 more)"),
            "{}",
            s.lines[1]
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
