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
        ("XDG_DATA_HOME", config.join("data")),
    ];
    for (_, path) in &env {
        assert_sandbox_path(path);
        fs::create_dir_all(path).expect("create isolated test directory");
    }
    if let Some(cargo) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
    {
        env.push(("CARGO_HOME", cargo));
    }
    if let Some(rustup) = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))
    {
        env.push(("RUSTUP_HOME", rustup));
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
    assert!(
        !resolved(state).starts_with(resolved(real_home)),
        "test server state directory must not resolve under the real HOME"
    );
}

pub fn assert_pty_isolated(cmd: &portable_pty::CommandBuilder) {
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        assert_sandbox_path(Path::new(
            cmd.get_env(key).expect("missing sandbox directory"),
        ));
    }
}

pub fn assert_command_isolated(cmd: &std::process::Command) {
    let value = |name: &str| match cmd.get_envs().find(|(key, _)| *key == name) {
        Some((_, value)) => value.map(std::ffi::OsStr::to_os_string),
        None => std::env::var_os(name),
    };
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        assert_sandbox_path(Path::new(&value(key).expect("missing sandbox directory")));
    }
}

/// All test subprocesses use this builder. Only flk (including a flk argument
/// to sandbox-exec) receives the sandbox environment at the execution boundary.
/// Keeping the inner command private prevents builder chains bypassing the guard.
pub struct Command(std::process::Command);

impl std::ops::Deref for Command {
    type Target = std::process::Command;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// Test-only subprocess funnel, outside the product's logging wrapper.
#[allow(clippy::disallowed_methods)]
impl Command {
    pub fn new(program: impl AsRef<std::ffi::OsStr>) -> Self {
        Self(std::process::Command::new(program))
    }
    pub fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.0.args(args);
        self
    }
    pub fn env(
        &mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> &mut Self {
        self.0.env(key, value);
        self
    }
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        self.0.envs(vars);
        self
    }
    pub fn env_remove(&mut self, key: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }
    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }
    pub fn stdin(&mut self, io: impl Into<std::process::Stdio>) -> &mut Self {
        self.0.stdin(io);
        self
    }
    pub fn stdout(&mut self, io: impl Into<std::process::Stdio>) -> &mut Self {
        self.0.stdout(io);
        self
    }
    pub fn stderr(&mut self, io: impl Into<std::process::Stdio>) -> &mut Self {
        self.0.stderr(io);
        self
    }
    pub fn arg0(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        use std::os::unix::process::CommandExt;
        self.0.arg0(arg);
        self
    }
    pub unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> std::io::Result<()> + Send + Sync + 'static,
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            self.0.pre_exec(f);
        }
        self
    }
    fn prepare(&mut self) {
        let bin = std::ffi::OsStr::new(env!("CARGO_BIN_EXE_flk"));
        let is_flk = self.0.get_program() == bin
            || self.0.get_args().any(|arg| {
                arg.to_string_lossy()
                    .contains(bin.to_string_lossy().as_ref())
            })
            || Path::new(self.0.get_program()).canonicalize().ok()
                == Path::new(bin).canonicalize().ok();
        if !is_flk {
            return;
        }
        let overrides: std::collections::HashMap<_, _> = self
            .0
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(std::ffi::OsStr::to_os_string)))
            .collect();
        let values = sandbox_env(|key| overrides.get(std::ffi::OsStr::new(key)).cloned().flatten());
        self.0.envs(values);
        // Do not inherit a live session/socket from the test runner. Explicit
        // fixture values and explicit removals retain their intended semantics.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("FLOCK_")
                && !overrides.contains_key(&key)
                && key != "FLOCK_HOST_NAME"
            {
                self.0.env_remove(key);
            }
        }
        assert_command_isolated(&self.0);
    }
    pub fn spawn(&mut self) -> std::io::Result<std::process::Child> {
        self.prepare();
        self.0.spawn()
    }
    pub fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.prepare();
        self.0.output()
    }
    pub fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.prepare();
        self.0.status()
    }
}

fn sandbox_env(value: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<(&'static str, PathBuf)> {
    let fallback = fallback_sandbox().clone();
    let config = value("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            value("HOME")
                .map(PathBuf::from)
                .unwrap_or(fallback)
                .join("config")
        });
    let runtime = value("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.join("runtime"));
    let mut env = isolated_env(&config, &runtime);
    for (key, path) in &mut env {
        if let Some(explicit) = value(key) {
            *path = explicit.into();
        }
    }
    // Check before creating any caller-supplied path.
    for (key, path) in &env {
        if matches!(*key, "CARGO_HOME" | "RUSTUP_HOME") {
            continue;
        }
        assert_sandbox_path(path);
        fs::create_dir_all(path).expect("create sandbox directory");
    }
    for key in ["FLOCK_SOCKET_PATH", "FLOCK_CLIENT_SOCKET_PATH"] {
        if let Some(socket) = value(key) {
            assert_sandbox_path(Path::new(&socket).parent().expect("socket directory"));
        }
    }
    env
}

fn assert_sandbox_path(path: &Path) {
    let real_home = std::env::var_os("HOME").expect("test runner HOME");
    assert_state_outside_home(path, Path::new(&real_home));
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        if let Some(real) = std::env::var_os(key) {
            assert_state_outside_home(path, Path::new(&real));
        }
    }
    let sandbox = std::env::temp_dir()
        .canonicalize()
        .expect("test sandbox root");
    // Existing socket fixtures deliberately use the short POSIX temp root on
    // macOS, where TMPDIR can leave too little room for a Unix socket name.
    let short = Path::new("/tmp")
        .canonicalize()
        .expect("short socket sandbox root"); // guardrails-ok(hermetic): Unix socket fixtures require a short root
    assert!(
        resolved(path).starts_with(sandbox) || resolved(path).starts_with(short),
        "test directory must be inside the temporary sandbox: {}",
        path.display()
    );
}

/// PTY equivalent of Command's execution boundary. portable-pty calls setsid
/// before exec, so each server is the leader of its own process group.
pub fn spawn_pty(
    slave: &dyn portable_pty::SlavePty,
    mut cmd: portable_pty::CommandBuilder,
) -> std::io::Result<Box<dyn portable_pty::Child + Send + Sync>> {
    for (key, value) in sandbox_env(|key| cmd.get_env(key).map(std::ffi::OsStr::to_os_string)) {
        cmd.env(key, value);
    }
    assert_pty_isolated(&cmd);
    slave.spawn_command(cmd).map_err(std::io::Error::other)
}

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

static FALLBACK_SANDBOX: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn fallback_sandbox() -> &'static PathBuf {
    FALLBACK_SANDBOX.get_or_init(|| {
        unsafe {
            libc::atexit(cleanup_fallback_sandbox);
        }
        std::env::temp_dir().join(format!("flk-env-{}", std::process::id()))
    })
}

extern "C" fn cleanup_fallback_sandbox() {
    if let Some(path) = FALLBACK_SANDBOX.get() {
        let _ = fs::remove_dir_all(path);
    }
}
