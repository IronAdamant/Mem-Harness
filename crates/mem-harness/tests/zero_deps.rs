//! Every crate depends only on sibling crates in this workspace, and
//! Cargo.lock names no package from outside it.

use std::path::Path;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

#[test]
fn manifests_have_only_path_dependencies() {
    let crates = workspace().join("crates");
    for entry in std::fs::read_dir(&crates).unwrap() {
        let manifest = entry.unwrap().path().join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        let mut section = String::new();
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line.to_string();
                continue;
            }
            if section.contains("dependencies") && !line.is_empty() && !line.starts_with('#') {
                assert!(
                    line.contains("path = \"../"),
                    "{}: {section} has a non-path dependency: {line}",
                    manifest.display()
                );
            }
        }
    }
}

#[test]
fn lockfile_holds_only_workspace_packages() {
    let lock = std::fs::read_to_string(workspace().join("Cargo.lock")).unwrap();
    for line in lock.lines() {
        assert!(
            !line.starts_with("source = "),
            "Cargo.lock names an outside package: {line}"
        );
    }
}

#[test]
fn every_dependency_is_used() {
    let crates = workspace().join("crates");
    for entry in std::fs::read_dir(&crates).unwrap() {
        let dir = entry.unwrap().path();
        let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let mut code = String::new();
        for sub in ["src", "tests", "examples"] {
            collect_rs(&dir.join(sub), &mut code);
        }
        let mut section = String::new();
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line.to_string();
                continue;
            }
            if section == "[dependencies]" && !line.is_empty() && !line.starts_with('#') {
                let name = line.split('=').next().unwrap().trim().replace('-', "_");
                assert!(
                    code.contains(&format!("{name}::")) || code.contains(&format!("use {name}")),
                    "{}: {name} is a dependency but no code uses it",
                    dir.display()
                );
            }
        }
    }
}

fn collect_rs(dir: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let p = entry.unwrap().path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push_str(&std::fs::read_to_string(&p).unwrap());
        }
    }
}
