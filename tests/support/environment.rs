use std::fs;
use std::path::{Path, PathBuf};

/// Directories shared by a test's server, clients and replacement servers.
/// Keep Cargo's real cache available when a fixture invokes Cargo under the fake HOME.
pub fn isolated_env(config: &Path, runtime: &Path) -> Vec<(&'static str, PathBuf)> {
    let home = config.join("home");
    let state = config.join("state");
    assert_isolated_state(Some(home.as_os_str()), Some(state.as_os_str()));
    let mut env = vec![
        ("HOME", home),
        ("XDG_CONFIG_HOME", config.to_path_buf()),
        ("XDG_RUNTIME_DIR", runtime.to_path_buf()),
        ("XDG_STATE_HOME", state),
    ];
    for (_, path) in &env {
        fs::create_dir_all(path).expect("create isolated test directory");
    }
    if let Some(cargo) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
    {
        env.push(("CARGO_HOME", cargo));
    }
    env
}

/// Fail before spawning if state could be written into the developer's HOME.
pub fn assert_isolated_state(home: Option<&std::ffi::OsStr>, state: Option<&std::ffi::OsStr>) {
    let state = state.map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(home.expect("test server must have HOME or XDG_STATE_HOME"))
            .join(".local/state")
    });
    let real_home = std::env::var_os("HOME").expect("test runner must have HOME");
    assert_state_outside_home(&state, Path::new(&real_home));
}

fn assert_state_outside_home(state: &Path, real_home: &Path) {
    // Canonicalize the existing ancestor too, so missing directories and
    // symlinks cannot disguise a path into the real home.
    fn resolved(path: &Path) -> PathBuf {
        assert!(path.is_absolute(), "test state paths must be absolute");
        if let Ok(path) = path.canonicalize() {
            return path;
        }
        let parent = path
            .parent()
            .expect("state path must have an existing ancestor");
        resolved(parent).join(path.file_name().expect("state path component"))
    }
    assert!(
        !resolved(state).starts_with(resolved(real_home)),
        "test server state directory must not resolve under the real HOME"
    );
}

pub fn assert_pty_isolated(cmd: &portable_pty::CommandBuilder) {
    assert_isolated_state(cmd.get_env("HOME"), cmd.get_env("XDG_STATE_HOME"));
}

pub fn assert_command_isolated(cmd: &std::process::Command) {
    let value = |name: &str| match cmd.get_envs().find(|(key, _)| *key == name) {
        Some((_, value)) => value.map(std::ffi::OsStr::to_os_string),
        None => std::env::var_os(name),
    };
    assert_isolated_state(value("HOME").as_deref(), value("XDG_STATE_HOME").as_deref());
}
