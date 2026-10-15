//! Typed Claude launch preparation (ADR-0036 §1, #1437).
//!
//! No processes are spawned here. #1438 consumes the prepared profile and
//! keeps it alive until descendants have exited. After #1426 supplies lease
//! identity, #1439 exposes the CLI.
#![deny(missing_docs)]
// The launch-only slice is wired into the runtime by #1438, then the API by #1439.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::api::error_code::ErrorCode;
use crate::platform::structured as private_fs;

/// File-only structured-session policy. Default refuses launches until O19a.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StructuredConfig {
    /// Execution identity; operator mode is an explicit exception to #1177.
    pub run_as: RunAs,
    /// Absolute path to a setup-token file, owned by this user and mode 0600.
    /// The token itself is never stored in config or written into settings.
    pub token_file: Option<PathBuf>,
    /// Persistent profile name under flock state; defaults to `default`.
    pub profile: String,
}

impl Default for StructuredConfig {
    fn default() -> Self {
        Self {
            run_as: RunAs::Agent,
            token_file: None,
            profile: "default".into(),
        }
    }
}

/// Unix identity policy, amended by the owner on 2026-10-14.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum RunAs {
    /// Requires the privileged O19a launcher (#1208), not present in this slice.
    #[default]
    #[serde(rename = "flk-agent")]
    Agent,
    /// Opt in to the operator uid, with a warning on every preparation.
    #[serde(rename = "operator")]
    Operator,
}

impl RunAs {
    /// Enforce the launch boundary before reading credentials or creating files.
    pub fn admit(self) -> Result<(), LaunchError> {
        match self {
            Self::Agent => Err(LaunchError::NeedsAgentUser),
            Self::Operator => {
                tracing::warn!("structured operator mode: agent tools run with the operator's uid and access (#1177)");
                Ok(())
            }
        }
    }
}

/// Launch-preparation refusal, without credential data in any error.
#[derive(Debug)]
pub enum LaunchError {
    /// O19a's dedicated-user launcher is required, even if a user exists.
    NeedsAgentUser,
    /// Socket isolation requires Claude Code 2.1.290 or newer on an absolute path.
    UnsupportedHarness,
    /// Credential or private-directory I/O failed. No fallback is attempted.
    Io(io::Error),
}

impl LaunchError {
    /// API registry code to use when this preparation fails.
    pub(crate) fn code(&self) -> ErrorCode {
        match self {
            Self::NeedsAgentUser => ErrorCode::StructuredNeedsAgentUser,
            Self::UnsupportedHarness => ErrorCode::StructuredHarnessVersionUnsupported,
            Self::Io(_) => ErrorCode::AgentStartFailed,
        }
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsAgentUser => f.write_str(
                "structured_needs_agent_user: flk-agent launch requires O19a (#1208)",
            ),
            Self::Io(err) => write!(f, "structured launch preparation failed: {err}"),
            Self::UnsupportedHarness => f.write_str(
                "structured_harness_version_unsupported: require Claude Code >= 2.1.290 at an absolute binary path",
            ),
        }
    }
}

impl std::error::Error for LaunchError {}
impl From<io::Error> for LaunchError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Canonical absolute binary path whose `--version` passes the isolation floor.
#[derive(Debug)]
pub struct VerifiedBinary {
    path: PathBuf,
    version: String,
}

impl VerifiedBinary {
    /// Check `--version` on this exact resolved path. The injected reader must
    /// run that path with `--version`, fail on a nonzero exit, and return stdout.
    /// S3a-1 tests inject text; process execution belongs to #1438.
    /// The reader must bound `--version` execution time. Resolving this path
    /// again at exec still races replacement; only `fexecve` fully closes that
    /// gap. Both are explicit runtime obligations in #1438.
    pub fn check(
        path: &Path,
        read_version: impl FnOnce(&Path, &[&str]) -> io::Result<String>,
    ) -> Result<Self, LaunchError> {
        if !path.is_absolute() || !path.is_file() {
            return Err(LaunchError::UnsupportedHarness);
        }
        let path = path.canonicalize()?;
        let output = read_version(&path, &["--version"])?;
        let version = output
            .trim()
            .strip_suffix(" (Claude Code)")
            .ok_or(LaunchError::UnsupportedHarness)?;
        let parts: Vec<u64> = version
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| LaunchError::UnsupportedHarness)?;
        if parts.len() != 3 || parts.as_slice() < [2, 1, 290].as_slice() {
            return Err(LaunchError::UnsupportedHarness);
        }
        Ok(Self {
            path,
            version: version.to_owned(),
        })
    }

    /// Exact executable path for the later runtime; PATH lookup is forbidden.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Observed harness version for parsing events that arrive before init.
    pub fn version(&self) -> &str {
        &self.version
    }
}

/// Inspect the pre-init session registry for a forbidden messaging socket.
/// Missing files mean no evidence yet, not readiness. Malformed/unreadable
/// records, symlinks, special files and records over 1 MiB fail closed.
/// #1438 checks this before writing the first user input.
pub fn registry_has_messaging_socket(config_dir: &Path, pid: u32) -> io::Result<bool> {
    let path = config_dir.join("sessions").join(format!("{pid}.json"));
    let file = match private_fs::open_regular_file(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if path.canonicalize()? != path {
        return Err(io::Error::other("redirected session registry"));
    }
    if file.metadata()?.len() > 1_048_576 {
        return Err(io::Error::other("session registry exceeds 1 MiB"));
    }
    let record: serde_json::Value = serde_json::from_reader(file.take(1_048_577))?;
    let object = record
        .as_object()
        .ok_or_else(|| io::Error::other("invalid session registry"))?;
    Ok(object.contains_key("messagingSocketPath"))
}

/// Setup-token held only in memory, wiped on drop and redacted in Debug.
pub struct Token(Zeroizing<String>);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Read at most 16 KiB from a private regular file, never from the env.
    /// A trailing newline from setup-token is accepted; embedded whitespace,
    /// NUL, invalid UTF-8, empty files and oversized files are refused.
    pub fn read(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("structured.token_file must be absolute"));
        }
        let mut raw = Zeroizing::new(String::with_capacity(16_385));
        private_fs::open_token(path)?
            .take(16_385)
            .read_to_string(&mut raw)?;
        if raw.len() > 16_384
            || raw.trim().is_empty()
            || raw
                .trim()
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b == 0)
        {
            return Err(io::Error::other("invalid structured setup-token file"));
        }
        Ok(Self(Zeroizing::new(raw.trim().to_owned())))
    }
}

/// Permission mode is explicit from S3 onward; S19 extends this type.
#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Use stdio permission requests rather than the token-path auto default.
    #[default]
    Default,
}

impl PermissionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
        }
    }
}

/// Typed settings file: no hooks, env overrides or token fields.
#[derive(Debug, Default, Serialize)]
pub struct LaunchSettings {
    /// Explicit permissions; S19 adds preset selection at this boundary.
    pub permissions: Permissions,
}

/// Harness permission policy, initially with empty allow and deny rules.
#[derive(Debug, Default, Serialize)]
pub struct Permissions {
    /// Startup mode, also passed on argv (#1406).
    #[serde(rename = "defaultMode")]
    pub default_mode: PermissionMode,
    /// Rules pre-allowing tools; empty in S3a-1.
    pub allow: Vec<String>,
    /// Rules denying tools; empty in S3a-1.
    pub deny: Vec<String>,
}

#[derive(Serialize, Default)]
struct McpConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
}

/// Persistent private harness profile, retained for resume, hot reload and forks
/// (ADR-0036 §1, §9). Sessions never delete its transcripts or registry.
/// `flk-agent` profiles belong under that user's `~/.flk/claude/<profile>` once
/// O19a is available; operator profiles use the separate flock-owned tree below.
#[derive(Debug)]
pub struct ProfileConfigDir(PathBuf);

impl ProfileConfigDir {
    /// Create/reuse `<state_dir>/structured/claude/<profile>` for operator mode.
    /// The existing flock state root is supplied by the caller. Every component
    /// below it must be a real directory owned by this uid with mode 0700.
    /// Existing directories with unsafe ownership or mode are refused, not fixed.
    pub fn for_operator(state_dir: &Path, profile: &str) -> io::Result<Self> {
        if profile.is_empty()
            || !profile
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(io::Error::other(
                "structured.profile must contain only ASCII letters, digits, '-' or '_'",
            ));
        }
        let mut path = state_dir.canonicalize()?;
        for component in ["structured", "claude", profile] {
            path.push(component);
            private_fs::ensure_private_dir(&path)?;
        }
        Ok(Self(path))
    }

    /// Stable CLAUDE_CONFIG_DIR. Dropping this value does not remove the profile.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Owns a fresh 0700 directory for generated session settings and MCP files,
/// distinct from the persistent CLAUDE_CONFIG_DIR. Recursively removes it on drop,
/// including during unwinding, unless armed for a spawn. Armed guards retain files
/// until supervision confirms the descendant tree is gone.
/// SIGKILL and filesystem failures cannot be recovered by a Rust destructor.
#[derive(Debug)]
pub struct SessionConfigDir {
    path: PathBuf,
    retained: bool,
}

impl SessionConfigDir {
    /// Create under an existing parent, using OS entropy and atomic creation.
    /// Canonicalizing the parent makes cleanup independent of later cwd changes.
    pub fn create(parent: &Path) -> io::Result<Self> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = parent.canonicalize()?.join(format!("flk-claude-{suffix}"));
        private_fs::create_private_dir(&path)?;
        Ok(Self {
            path,
            retained: false,
        })
    }

    /// Temporary directory for this session's generated `--settings`/`--mcp-config`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Retain files until the supervisor confirms descendants and transients are gone.
    pub(super) fn retain(&mut self) {
        self.retained = true;
    }
    /// Remove generated files explicitly after verified reaping; report disk errors.
    pub(super) fn remove_after_reap(mut self) -> io::Result<()> {
        self.retained = true;
        fs::remove_dir_all(&self.path)
    }

    fn write(&self, name: &str, value: &impl Serialize) -> io::Result<PathBuf> {
        let path = self.path.join(name);
        let file = private_fs::create_private_file(&path)?;
        serde_json::to_writer(file, value)?;
        Ok(path)
    }
}

impl Drop for SessionConfigDir {
    fn drop(&mut self) {
        if self.retained {
            return;
        }
        if let Err(err) = fs::remove_dir_all(&self.path) {
            crate::logging::structured_config_cleanup_failed(&err);
        }
    }
}

/// Prepared argv, typed settings and owned secrets, with no spawn side effects.
/// Dropping an unarmed profile removes its session files; arm before spawning.
#[derive(Debug)]
pub struct LaunchProfile {
    args: Vec<OsString>,
    binary: VerifiedBinary,
    environment: EnvironmentAllowlist,
    profile_directory: ProfileConfigDir,
    directory: SessionConfigDir,
}

impl LaunchProfile {
    /// Prepare ADR-0036 §1 with the owner's 2026-10-14 amendment.
    /// `session_id` is supplied by the session owner (#1438), never a path.
    pub fn prepare(
        config: &StructuredConfig,
        state_dir: &Path,
        session_id: &str,
        binary: VerifiedBinary,
        read_env: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, LaunchError> {
        config.run_as.admit()?;
        let token = Token::read(
            config
                .token_file
                .as_deref()
                .ok_or_else(|| io::Error::other("structured.token_file is required"))?,
        )?;
        let profile_directory = ProfileConfigDir::for_operator(state_dir, &config.profile)?;
        let directory = super::supervision::prepare_session_files(state_dir)?;
        let settings = LaunchSettings::default();
        let settings_path = directory.write("settings.json", &settings)?;
        let mcp_path = directory.write("mcp.json", &McpConfig::default())?;
        let mut args: Vec<OsString> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--include-hook-events",
            "--permission-prompt-tool",
            "stdio",
            "--setting-sources",
            "local",
            "--strict-mcp-config",
            "--restricted",
            "--permission-mode",
            settings.permissions.default_mode.as_str(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        args.extend([
            "--settings".into(),
            settings_path.into_os_string(),
            "--mcp-config".into(),
            mcp_path.into_os_string(),
            "--session-id".into(),
            session_id.into(),
        ]);
        let environment = EnvironmentAllowlist::capture(read_env, token);
        Ok(Self {
            args,
            binary,
            environment,
            profile_directory,
            directory,
        })
    }

    /// Arm generated files before attempting spawn, retaining uncertain lifetimes.
    pub fn arm_session(&mut self) -> io::Result<()> {
        super::supervision::arm_session_files(&mut self.directory)
    }

    /// Arm and attempt a spawn, transferring the profile back only with a child.
    /// An error removes generated files because no child exists; persistent
    /// profiles remain. The closure must directly return its spawn result, never
    /// launch a child and then return an unrelated error. Panics retain files.
    pub fn try_spawn(
        mut self,
        spawn: impl FnOnce() -> io::Result<std::process::Child>,
    ) -> io::Result<(std::process::Child, Self)> {
        match self.arm_session().and_then(|()| spawn()) {
            Ok(child) => Ok((child, self)),
            Err(error) => {
                if let Err(cleanup) = self.directory.remove_after_reap() {
                    return Err(io::Error::other(format!(
                        "{error}; spawn-failure cleanup: {cleanup}"
                    )));
                }
                Err(error)
            }
        }
    }

    /// Transfer generated-file ownership to supervision after spawn. Arm first.
    pub fn into_session_files(self) -> SessionConfigDir {
        self.directory
    }

    /// Arguments after the `claude` executable. No credential is on argv.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Version-checked absolute executable for #1438.
    pub fn binary(&self) -> &Path {
        self.binary.path()
    }

    /// Verified harness version for the stdout adapter, including pre-init lines.
    pub fn harness_version(&self) -> &str {
        self.binary.version()
    }

    /// Persistent harness profile for registry checks, resume and fork operations.
    pub fn config_dir(&self) -> &Path {
        self.profile_directory.path()
    }

    /// Emit only the allowlisted and session-specific entries for a child builder.
    /// The sink must not log or persist the token. This single delivery seam
    /// lets #1438 change transport if the token-inheritance review requires it.
    pub fn configure_environment(&self, mut set: impl FnMut(&'static str, &OsStr)) {
        for (key, value) in &self.environment.values {
            set(key, value);
        }
        set("CLAUDE_CONFIG_DIR", self.config_dir().as_os_str());
        set(
            "CLAUDE_CODE_OAUTH_TOKEN",
            OsStr::new(self.environment.token.0.as_str()),
        );
    }
}

/// Closed environment allowlist for an `env_clear()` child. Its only credential
/// is the dedicated setup-token; arbitrary server env keys cannot be inserted.
/// HOME remains needed by the harness; operator-mode tools can read that home.
#[derive(Debug)]
pub struct EnvironmentAllowlist {
    values: BTreeMap<&'static str, OsString>,
    token: Token,
}

impl EnvironmentAllowlist {
    fn capture(mut read: impl FnMut(&str) -> Option<OsString>, token: Token) -> Self {
        let values = [
            "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR",
        ]
        .into_iter()
        .filter_map(|key| read(key).map(|value| (key, value)))
        .collect();
        Self { values, token }
    }
}

#[cfg(test)]
mod tests;
