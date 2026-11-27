//! Stores kept outside the project: folders, records, moving and forgetting,
//! and the `storage` setting. Every test here shares one temporary data and
//! cache folder, set before any test runs.

use mh_core::storage::{self, Kind};
use mh_core::Config;
use std::path::{Path, PathBuf};
use std::sync::Once;

static BASES: Once = Once::new();

fn bases() -> PathBuf {
    let root = std::env::temp_dir().join(format!("mh-storage-it-{}", std::process::id()));
    BASES.call_once(|| {
        let _ = std::fs::remove_dir_all(&root);
        std::env::set_var("MEM_HARNESS_DATA_DIR", root.join("data"));
        std::env::set_var("MEM_HARNESS_CACHE_DIR", root.join("cache"));
    });
    root
}

fn project(name: &str) -> PathBuf {
    let d = bases().join("projects").join(name);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

#[test]
fn bases_follow_the_environment() {
    let b = bases();
    assert_eq!(storage::base(Kind::Data).unwrap(), b.join("data"));
    assert_eq!(storage::base(Kind::Cache).unwrap(), b.join("cache"));
}

#[test]
fn record_list_move_forget() {
    let old = project("old-name");
    storage::record(Kind::Data, &old).unwrap();
    storage::record(Kind::Cache, &old).unwrap();
    std::fs::create_dir_all(storage::store_dir(Kind::Data, &old, "chisel").unwrap()).unwrap();
    std::fs::write(
        storage::store_dir(Kind::Data, &old, "chisel")
            .unwrap()
            .join("x"),
        "12345",
    )
    .unwrap();
    let mine: Vec<_> = storage::find(old.to_str().unwrap());
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert!(mine
        .iter()
        .all(|e| e.exists && e.path == old.to_string_lossy()));
    assert!(mine.iter().any(|e| e.kind == Kind::Data && e.bytes >= 5));

    // Renamed: the old path is gone, its stores are listed as such.
    let new = old.with_file_name("new-name");
    std::fs::rename(&old, &new).unwrap();
    assert!(storage::find(old.to_str().unwrap())
        .iter()
        .all(|e| !e.exists));
    let moved = storage::move_project(old.to_str().unwrap(), &new).unwrap();
    assert_eq!(moved.len(), 2, "{moved:?}");
    let chisel = storage::store_dir(Kind::Data, &new, "chisel").unwrap();
    assert_eq!(std::fs::read_to_string(chisel.join("x")).unwrap(), "12345");
    let now = storage::find(new.to_str().unwrap());
    assert!(now
        .iter()
        .all(|e| e.exists && e.path == new.to_string_lossy()));

    // Moving onto a project that has stores is refused.
    let other = project("other");
    storage::record(Kind::Data, &other).unwrap();
    assert!(storage::move_project(new.to_str().unwrap(), &other).is_err());
    // ... and moves nothing.
    assert_eq!(storage::find(new.to_str().unwrap()).len(), 2);

    let gone = storage::forget(new.to_str().unwrap()).unwrap();
    assert_eq!(gone.len(), 2);
    assert!(storage::find(new.to_str().unwrap()).is_empty());
    assert!(storage::forget(new.to_str().unwrap()).is_err());
    // By id as well.
    let id = storage::project_id(&other);
    assert_eq!(storage::forget(&id).unwrap().len(), 1);
}

#[test]
fn launch_adds_the_store_by_variable_or_flag() {
    let root = project("launch");
    let cfg = Config::builtin();
    let spec = |n: &str| cfg.backends.iter().find(|b| b.name == n).unwrap().clone();
    let (s, env) = cfg.launch(&spec("stele"), &root);
    let want = storage::store_dir(Kind::Cache, &root, "stele").unwrap();
    assert!(env
        .iter()
        .any(|(k, v)| k == "STELE_CONTEXT_STORAGE_DIR" && Path::new(v) == want));
    assert_eq!(s.args, spec("stele").args);
    let (s, env) = cfg.launch(&spec("hub"), &root);
    let want = storage::store_dir(Kind::Data, &root, "hub").unwrap();
    assert_eq!(
        s.args[s.args.len() - 2..],
        [
            "--storage-dir".to_string(),
            want.to_string_lossy().into_owned()
        ]
    );
    assert!(!env.iter().any(|(_, v)| Path::new(v) == want));
    // A backend without the setting is started as configured.
    let (s, env) = cfg.launch(&spec("relatum"), &root);
    assert_eq!(s.args, spec("relatum").args);
    assert_eq!(env, cfg.backend_env("relatum"));
    // Commands the harness runs get every store variable.
    let vars: Vec<String> = cfg.storage_env(&root).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        vars,
        [
            "STELE_CONTEXT_STORAGE_DIR",
            "CHISEL_STORAGE_DIR",
            "TRAMMEL_DB_PATH"
        ]
    );
}

fn with_backend_storage(storage: &str) -> Result<Config, String> {
    let mut v = mh_json::parse(mh_core::config::DEFAULT_JSON).unwrap();
    let mut list = v.get("backends").unwrap().as_array().unwrap().clone();
    list[0].set("storage", mh_json::parse(storage).unwrap());
    v.set("backends", mh_json::Value::Array(list));
    Config::from_json(&v.to_json(), "test")
}

#[test]
fn storage_settings_are_checked() {
    with_backend_storage(r#"{"kind":"data","env":"X_DIR"}"#).unwrap();
    assert!(with_backend_storage(r#"{"kind":"cache","arg":"--store"}"#).is_ok());
    for bad in [
        r#"{"kind":"forever","env":"X_DIR"}"#,
        r#"{"kind":"data"}"#,
        r#"{"kind":"data","env":"X","arg":"--x"}"#,
        r#"{"kind":"data","env":"not a var"}"#,
        r#"{"kind":"data","arg":"x"}"#,
        r#"{"kind":"data","env":"X","where":"y"}"#,
        r#""data""#,
    ] {
        assert!(with_backend_storage(bad).is_err(), "accepted {bad}");
    }
}

#[test]
fn git_exclude_names_are_plain_folder_names() {
    let with = |list: &str| {
        let mut v = mh_json::parse(mh_core::config::DEFAULT_JSON).unwrap();
        v.set("git_exclude", mh_json::parse(list).unwrap());
        Config::from_json(&v.to_json(), "test")
    };
    assert!(with(r#"[".chisel", ".trammel/"]"#).is_ok());
    for bad in [
        r#"["a/b"]"#,
        r#"["*"]"#,
        r#"["!x"]"#,
        r#"[".."]"#,
        r#"[""]"#,
    ] {
        assert!(with(bad).is_err(), "accepted {bad}");
    }
}

/// A repository's own overlay cannot change where stores go or what git
/// is told to skip: both are user config only.
#[test]
fn the_project_overlay_cannot_set_git_exclude() {
    assert!(!mh_core::config::PROJECT_KEYS.contains(&"git_exclude"));
}
