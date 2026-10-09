mod support;

use std::path::PathBuf;

#[test]
fn isolated_environment_overrides_ambient_paths_and_preserves_cargo_cache() {
    let base = std::env::temp_dir().join(format!("flock-env-{}", std::process::id()));
    let config = base.join("config");
    let runtime = base.join("runtime");
    let mut cmd = portable_pty::CommandBuilder::new("/bin/sh");
    for name in [
        "HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        cmd.env(name, base.join("ambient"));
    }
    for (key, value) in support::environment::isolated_env(&config, &runtime) {
        cmd.env(key, value);
    }
    support::environment::assert_pty_isolated(&cmd);
    for (key, path) in [
        ("HOME", config.join("home")),
        ("XDG_STATE_HOME", config.join("state")),
        ("XDG_CONFIG_HOME", config),
        ("XDG_RUNTIME_DIR", runtime),
    ] {
        assert_eq!(cmd.get_env(key), Some(path.as_os_str()));
        assert!(path.is_dir());
    }
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cargo"));
    assert_eq!(cmd.get_env("CARGO_HOME"), Some(cargo.as_os_str()));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
#[should_panic(expected = "must not resolve under the real HOME")]
fn state_guard_rejects_real_home_fallback() {
    let real_home = std::env::var_os("HOME").unwrap();
    support::environment::assert_isolated_state(Some(&real_home), None);
}

#[test]
#[should_panic(expected = "must not resolve under the real HOME")]
fn state_guard_rejects_explicit_real_home_state() {
    let real_home = PathBuf::from(std::env::var_os("HOME").unwrap());
    support::environment::assert_isolated_state(
        None,
        Some(real_home.join(".local/state").as_os_str()),
    );
}

#[test]
fn state_guard_rejects_symlink_into_real_home() {
    let base = std::env::temp_dir().join(format!("flock-env-link-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let link = base.join("state");
    std::os::unix::fs::symlink(std::env::var_os("HOME").unwrap(), &link).unwrap();
    let result = std::panic::catch_unwind(|| {
        support::environment::assert_isolated_state(
            None,
            Some(link.join("missing-test-state").as_os_str()),
        );
    });
    std::fs::remove_dir_all(base).unwrap();
    assert!(result.is_err(), "a symlink must not bypass the state guard");
}
