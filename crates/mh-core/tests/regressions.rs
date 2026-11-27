//! Second-round review reproductions. Each test asserts the behaviour the
//! docs promise; a failure is the defect.

use mh_core::config::{self, Config};
use std::path::PathBuf;
use std::sync::Mutex;

static ENV: Mutex<()> = Mutex::new(());

struct Sandbox {
    dir: PathBuf,
    project: PathBuf,
    old_home: Option<std::ffi::OsString>,
    old_profile: Option<std::ffi::OsString>,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("mh-r2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("repo");
        std::fs::create_dir_all(project.join(".mem-harness")).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let old_profile = std::env::var_os("USERPROFILE");
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::remove_var("MEM_HARNESS_CONFIG");
        std::env::remove_var("MEM_HARNESS_PROFILE");
        Sandbox {
            dir,
            project,
            old_home,
            old_profile,
        }
    }
    fn overlay(&self, text: &str) {
        std::fs::write(self.project.join(".mem-harness").join("harness.json"), text).unwrap();
    }
    fn load(&self) -> Result<Config, String> {
        Config::load(&self.project)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        match self.old_home.take() {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match self.old_profile.take() {
            Some(h) => std::env::set_var("USERPROFILE", h),
            None => std::env::remove_var("USERPROFILE"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

fn env_of(c: &Config, backend: &str) -> String {
    let b = c.backend(backend).unwrap();
    c.backend_env(backend)
        .into_iter()
        .find(|(k, _)| *k == b.disable_env)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// F1a: a comma in a trim smuggles "locks" past the owner check, so the
/// project file hides the hub's own locks group.
#[test]
fn project_cannot_hide_owner_feature_with_a_comma() {
    let _g = lock();
    let s = Sandbox::new("comma");
    s.overlay(r#"{"profiles":{"lean":{"hub":["x,locks"]}}}"#);
    let r = s.load();
    if let Ok(c) = &r {
        let v = env_of(c, "hub");
        assert!(
            !v.split(',').any(|p| p == "locks"),
            "hub's owned feature hidden from it: COORDINATIONHUB_DISABLE={v}"
        );
    }
}

/// F1b: same trick disables `core`, which validation says is impossible.
#[test]
fn project_cannot_disable_core_with_a_comma() {
    let _g = lock();
    let s = Sandbox::new("core");
    s.overlay(r#"{"profiles":{"lean":{"stele":["x,core"]}}}"#);
    let r = s.load();
    if let Ok(c) = &r {
        let v = env_of(c, "stele");
        assert!(
            !v.split(',').any(|p| p.eq_ignore_ascii_case("core")),
            "core disabled: STELE_DISABLE={v}"
        );
    }
}

/// F2: the owner check compares the trim with the feature *name*; feature
/// names (rules, task-board, private-notes) are not the owner's tool or
/// group names, so a project hides Relatum's rule tools freely.
#[test]
fn project_cannot_hide_the_rules_owners_tools() {
    let _g = lock();
    let s = Sandbox::new("names");
    s.overlay(
        r#"{"profiles":{"lean":{"relatum":["relatum_new","relatum_edit","relatum_supersede"]}}}"#,
    );
    let r = s.load();
    assert!(
        r.is_err(),
        "accepted: RELATUM_DISABLE={}",
        env_of(r.as_ref().unwrap(), "relatum")
    );
}

/// F3: the overlay replaces the whole ownership table, so it can drop the
/// "locks" entry and then hide the hub's locks.
#[test]
fn project_cannot_rewrite_ownership_to_hide_locks() {
    let _g = lock();
    let s = Sandbox::new("own");
    s.overlay(
        r#"{"ownership":[{"feature":"x","owner":"stele","disable":{"hub":["locks","messages"]}}]}"#,
    );
    let r = s.load();
    assert!(
        r.is_err(),
        "accepted: COORDINATIONHUB_DISABLE={}",
        env_of(r.as_ref().unwrap(), "hub")
    );
}

/// F5: `mem-harness print-config` says to copy its output to
/// `.mem-harness/harness.json`; that file is now refused.
#[test]
fn unknown_brief_key_is_an_error() {
    let _g = lock();
    let s = Sandbox::new("brief");
    s.overlay(r#"{"brief":{"nudge_on_stp":false}}"#);
    let r = s.load();
    assert!(
        r.is_err(),
        "typo accepted; nudge_on_stop still {}",
        r.unwrap().brief.nudge_on_stop
    );
}

/// F6b: a typo in a backend's `env_pass` (user file) silently gives the
/// server the whole environment.
#[test]
fn unknown_backend_key_is_an_error() {
    let mut v = mh_json::parse(config::DEFAULT_JSON).unwrap();
    let Some(mh_json::Value::Array(list)) = v.get_mut("backends") else {
        panic!()
    };
    list[0].set("env_passs", mh_json::parse(r#"["KEY"]"#).unwrap());
    let r = Config::from_json(&v.to_json(), "t");
    assert!(
        r.is_err(),
        "typo accepted; env_pass = {:?}",
        r.unwrap().backends[0].env_pass
    );
}

/// F8: a NUL in a project-supplied trim reaches the child's environment,
/// and the backend can no longer be started at all.
#[test]
fn trims_cannot_carry_nul() {
    let _g = lock();
    let s = Sandbox::new("nul");
    s.overlay(r#"{"profiles":{"lean":{"stele":["a\u0000b"]}}}"#);
    if let Ok(c) = s.load() {
        let env = c.backend_env("stele");
        let spawn = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .envs(env)
            .status();
        assert!(
            spawn.is_ok(),
            "backend cannot start: {}",
            spawn.unwrap_err()
        );
    }
}

/// F9: a relative MEM_HARNESS_CONFIG is read from the current folder, i.e.
/// from inside whatever repository the harness was started in, and that
/// file is trusted to name commands.
#[test]
fn relative_config_path_is_not_read_from_the_repo() {
    let _g = lock();
    let s = Sandbox::new("rel");
    let mut v = mh_json::parse(config::DEFAULT_JSON).unwrap();
    let Some(mh_json::Value::Array(list)) = v.get_mut("backends") else {
        panic!()
    };
    list[0].set("command", mh_json::Value::str("./evil"));
    std::fs::write(s.project.join("harness.json"), v.to_json()).unwrap();
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&s.project).unwrap();
    std::env::set_var("MEM_HARNESS_CONFIG", "harness.json");
    let r = s.load();
    std::env::remove_var("MEM_HARNESS_CONFIG");
    std::env::set_current_dir(old).unwrap();
    if let Ok(c) = r {
        assert_ne!(
            c.backends[0].command, "./evil",
            "repo file trusted: {}",
            c.source
        );
    }
}

#[test]
fn glob_edges() {
    use config::glob;
    assert!(glob("*", ""));
    assert!(glob("**", "abc"));
    assert!(glob("a*a", "aa"));
    assert!(!glob("a*a", "a"));
    assert!(glob("é*é", "éxé"));
    assert!(!glob("é*é", "é"));
    assert!(glob("*ü*", "aüb"));
    assert!(glob("a**b", "ab"));
    assert!(!glob("", "x"));
    assert!(glob("", ""));
}

#[cfg(unix)]
fn mode(p: &std::path::Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

/// F4: `find_command` takes the first PATH hit even when it is not
/// executable; posix_spawnp/execvp would skip it. Every backend and hook
/// command is now spawned through `find_command`, so this is a regression.
#[cfg(unix)]
#[test]
fn find_command_skips_non_executable_files() {
    let _g = lock();
    let dir = std::env::temp_dir().join(format!("mh-r2-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("mh-r2-tool"), "not a program").unwrap();
    mode(&a.join("mh-r2-tool"), 0o644);
    std::fs::write(b.join("mh-r2-tool"), "#!/bin/sh\necho ok\n").unwrap();
    mode(&b.join("mh-r2-tool"), 0o755);
    let old = std::env::var_os("PATH");
    std::env::set_var(
        "PATH",
        format!("{}:{}:/usr/bin:/bin", a.display(), b.display()),
    );
    let plain = std::process::Command::new("mh-r2-tool").output();
    let found = mh_core::project::find_command("mh-r2-tool", &dir);
    let via = found
        .as_ref()
        .map(|p| std::process::Command::new(p).output());
    match old {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(plain.is_ok(), "plain spawn: {:?}", plain.err());
    match via {
        Some(Ok(_)) => {}
        Some(Err(e)) => panic!("find_command picked {:?}: {e}", found.unwrap()),
        None => panic!("not found"),
    }
}
