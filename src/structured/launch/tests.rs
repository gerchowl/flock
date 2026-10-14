use super::*;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::{Arc, Mutex};

const FAKE_TOKEN: &str = "test-token-not-a-credential";
const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).expect("fixture entropy");
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("flk-launch-test-{suffix}"));
        private_fs::create_private_dir(&dir).expect("fixture directory");
        Self(dir.canonicalize().expect("canonical fixture"))
    }

    fn file(&self, name: &str, contents: &[u8], mode: u32) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).expect("fixture file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("fixture mode");
        path
    }

    fn config(&self) -> StructuredConfig {
        StructuredConfig {
            run_as: RunAs::Operator,
            token_file: Some(self.file("token", FAKE_TOKEN.as_bytes(), 0o600)),
            ..StructuredConfig::default()
        }
    }

    fn binary(&self) -> VerifiedBinary {
        let path = self.file("claude", b"not executed", 0o700);
        VerifiedBinary::check(&path, |_, _| Ok("2.1.296 (Claude Code)\n".into()))
            .expect("supported version")
    }

    fn profile(&self) -> LaunchProfile {
        LaunchProfile::prepare(&self.config(), &self.0, SESSION_ID, self.binary(), |_| None)
            .expect("launch preparation")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("fixture cleanup");
    }
}

#[test]
fn structured_config_requires_explicit_operator_opt_in() {
    let fixture = Fixture::new();
    let token_path = fixture.0.join("token");
    let default: crate::config::Config = toml::from_str("").expect("default config");
    assert_eq!(default.structured.run_as, RunAs::Agent);
    assert!(default.structured.token_file.is_none());
    assert_eq!(default.structured.profile, "default");
    let config: crate::config::Config = toml::from_str(&format!(
        "[structured]\nrun_as = 'operator'\ntoken_file = {token_path:?}"
    ))
    .expect("operator config");
    assert_eq!(config.structured.run_as, RunAs::Operator);
    assert_eq!(config.structured.token_file, Some(token_path));
    for value in ["root", "flk_agent", ""] {
        assert!(toml::from_str::<crate::config::Config>(&format!(
            "[structured]\nrun_as = '{value}'"
        ))
        .is_err());
    }
    assert!(toml::from_str::<crate::config::Config>(
        "[structured]\nrun_as = 'operator'\ntoken = 'no-inline-secrets'"
    )
    .is_err());
    let serialized = toml::to_string(&default).expect("config serialization");
    assert_eq!(
        toml::from_str::<crate::config::Config>(&serialized)
            .expect("round trip")
            .structured
            .run_as,
        RunAs::Agent
    );
}

#[test]
fn default_run_as_is_gated_before_any_file_access() {
    let fixture = Fixture::new();
    let err = LaunchProfile::prepare(
        &StructuredConfig::default(),
        &fixture.0.join("missing-parent"),
        SESSION_ID,
        fixture.binary(),
        |_| None,
    )
    .expect_err("O19a must gate preparation");
    assert!(matches!(err, LaunchError::NeedsAgentUser));
    assert_eq!(err.code(), ErrorCode::StructuredNeedsAgentUser);
    assert_eq!(err.code().retry().as_str(), "after_human");
    assert!(err.to_string().contains("#1208"));
    assert_eq!(
        fs::read_dir(&fixture.0).expect("fixture listing").count(),
        1
    );
}

#[test]
fn exact_launch_profile_and_minimal_typed_files() {
    let fixture = Fixture::new();
    let profile = fixture.profile();
    let dir = profile.directory.path();
    let expected: Vec<OsString> = [
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
        "default",
        "--settings",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([
        dir.join("settings.json").into_os_string(),
        "--mcp-config".into(),
        dir.join("mcp.json").into_os_string(),
        "--session-id".into(),
        SESSION_ID.into(),
    ])
    .collect();
    assert_eq!(profile.args(), expected);
    assert_eq!(profile.binary(), fixture.0.join("claude"));
    assert_eq!(profile.harness_version(), "2.1.296");
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("settings.json")).expect("settings"))
            .expect("settings JSON");
    assert_eq!(
        settings,
        serde_json::json!({"permissions": {
            "defaultMode": "default", "allow": [], "deny": []
        }})
    );
    let mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("mcp.json")).expect("MCP config"))
            .expect("MCP JSON");
    assert_eq!(mcp, serde_json::json!({"mcpServers": {}}));
    assert_eq!(
        fs::metadata(dir)
            .expect("config directory")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for name in ["settings.json", "mcp.json"] {
        let path = dir.join(name);
        assert_eq!(
            fs::metadata(&path)
                .expect("private file")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!fs::read_to_string(path)
            .expect("no secret file")
            .contains(FAKE_TOKEN));
    }
    assert_eq!(fs::read_dir(dir).expect("session files").count(), 2);
}

#[test]
fn allowlist_excludes_server_credentials_and_live_session_markers() {
    let fixture = Fixture::new();
    let supplied: BTreeMap<&str, OsString> = [
        ("PATH", "fixture-path"),
        ("HOME", "fixture-home"),
        ("LANG", "C"),
        ("GH_TOKEN", "other-secret"),
        ("AWS_SECRET_ACCESS_KEY", "other-secret"),
        ("FLOCK_SOCKET_PATH", "live-server"),
        ("FLOCK_PANE_ID", "live-pane"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "ambient-token"),
        ("CLAUDE_CONFIG_DIR", "live-profile"),
        ("CLAUDE_CODE_MESSAGING_SOCKET", "live-socket"),
        ("CLAUDECODE", "nested-session"),
    ]
    .into_iter()
    .map(|(key, value)| (key, value.into()))
    .collect();
    let profile = LaunchProfile::prepare(
        &fixture.config(),
        &fixture.0,
        SESSION_ID,
        fixture.binary(),
        |key| supplied.get(key).cloned(),
    )
    .expect("profile");
    let mut emitted = BTreeMap::new();
    profile.configure_environment(|key, value| {
        emitted.insert(key, value.to_os_string());
    });
    assert_eq!(
        emitted.keys().copied().collect::<Vec<_>>(),
        [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "HOME",
            "LANG",
            "PATH"
        ]
    );
    assert_eq!(emitted["CLAUDE_CODE_OAUTH_TOKEN"], FAKE_TOKEN);
    assert_eq!(
        emitted["CLAUDE_CONFIG_DIR"],
        profile.profile_directory.path()
    );
    assert_eq!(profile.config_dir(), profile.profile_directory.path());
    assert_ne!(profile.profile_directory.path(), profile.directory.path());
    assert!(!format!("{profile:?}").contains(FAKE_TOKEN));
    assert!(profile
        .args()
        .iter()
        .all(|arg| !arg.to_string_lossy().contains(FAKE_TOKEN)));
    assert!(!format!(
        "{:?}",
        Token::read(fixture.config().token_file.as_deref().expect("token path")).expect("token")
    )
    .contains(FAKE_TOKEN));
}

#[test]
fn token_file_must_have_exactly_0600_permissions() {
    let fixture = Fixture::new();
    for mode in [
        0o644, 0o640, 0o604, 0o660, 0o606, 0o666, 0o700, 0o400, 0o1600,
    ] {
        let path = fixture.file(&format!("token-{mode:o}"), FAKE_TOKEN.as_bytes(), mode);
        let err = Token::read(&path).expect_err("non-0600 token must fail");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "mode {mode:o}");
        assert!(!format!("{err:?} {err}").contains(FAKE_TOKEN));
    }
    for bytes in [
        FAKE_TOKEN.as_bytes().to_vec(),
        format!("{FAKE_TOKEN}\n").into_bytes(),
    ] {
        assert!(Token::read(&fixture.file("token", &bytes, 0o600)).is_ok());
    }
}

#[test]
fn token_reader_refuses_links_nonfiles_and_malformed_values() {
    let fixture = Fixture::new();
    let target = fixture.file("token", FAKE_TOKEN.as_bytes(), 0o600);
    let link = fixture.0.join("link");
    symlink(&target, &link).expect("symlink fixture");
    assert!(Token::read(&link).is_err());
    assert!(Token::read(&fixture.0).is_err());
    assert!(Token::read(&fixture.0.join("missing")).is_err());
    assert!(Token::read(Path::new("relative-token")).is_err());
    for bytes in [
        vec![],
        b" \n".to_vec(),
        b"token\0secret".to_vec(),
        b"token secret".to_vec(),
        vec![0xff],
        vec![b'x'; 16_385],
    ] {
        let path = fixture.file("token", &bytes, 0o600);
        assert!(Token::read(&path).is_err(), "malformed token was accepted");
    }
}

#[test]
fn session_directories_are_distinct_and_removed_on_drop() {
    let fixture = Fixture::new();
    let first = fixture.profile();
    let second = fixture.profile();
    let first_path = first.directory.path().to_owned();
    let second_path = second.directory.path().to_owned();
    assert_ne!(first_path, second_path);
    fs::create_dir_all(first_path.join("projects/memory")).expect("harness state");
    fs::write(first_path.join("projects/memory/file"), "state").expect("nested state");
    drop(first);
    assert!(!first_path.exists());
    assert!(second_path.exists());
    drop(second);
    assert!(!second_path.exists());
}

#[test]
fn config_directory_is_removed_on_unwind_and_writer_error() {
    let fixture = Fixture::new();
    let mut path = PathBuf::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let profile = fixture.profile();
        path = profile.directory.path().to_owned();
        panic!("injected unwind");
    }));
    assert!(result.is_err());
    assert!(!path.exists());
    let path = {
        let dir = SessionConfigDir::create(&fixture.0).expect("private dir");
        let path = dir.path().to_owned();
        dir.write("settings.json", &LaunchSettings::default())
            .expect("initial settings");
        assert!(dir
            .write("settings.json", &LaunchSettings::default())
            .is_err());
        path
    };
    assert!(!path.exists());
    let config = fixture.config();
    let binary = fixture.binary();
    let result = std::panic::catch_unwind(|| {
        LaunchProfile::prepare(&config, &fixture.0, SESSION_ID, binary, |_| {
            panic!("injected preparation failure after writing session files");
        })
    });
    assert!(result.is_err());
    assert_eq!(
        fs::read_dir(fixture.0.join("structured/sessions"))
            .expect("contained session files")
            .filter(|entry| entry
                .as_ref()
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with("flk-claude-"))
            .count(),
        0
    );
    assert!(fixture.0.join("structured/claude/default").is_dir());
}

#[test]
fn harness_profile_persists_across_session_drop_and_unwind() {
    let fixture = Fixture::new();
    let first = fixture.profile();
    let persistent = first.config_dir().to_owned();
    assert_eq!(persistent, fixture.0.join("structured/claude/default"));
    assert_eq!(
        fs::metadata(&persistent)
            .expect("profile")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let transcript = persistent.join("transcript.jsonl");
    fs::write(&transcript, "retained transcript").expect("harness transcript");
    let generated = first.directory.path().to_owned();
    drop(first);
    assert!(transcript.is_file());
    assert!(!generated.exists());
    let second = fixture.profile();
    assert_eq!(second.config_dir(), persistent);
    assert_eq!(
        fs::read_to_string(&transcript).expect("resume transcript"),
        "retained transcript"
    );
    let generated = second.directory.path().to_owned();
    let result = std::panic::catch_unwind(|| {
        let _session = second;
        panic!("session unwind");
    });
    assert!(result.is_err());
    assert!(!generated.exists());
    assert!(transcript.is_file());
    let mut config = fixture.config();
    config.profile = "work".into();
    let other = LaunchProfile::prepare(&config, &fixture.0, SESSION_ID, fixture.binary(), |_| None)
        .expect("other profile");
    assert_eq!(
        other.profile_directory.path(),
        fixture.0.join("structured/claude/work")
    );
    assert_ne!(other.profile_directory.path(), persistent);
}

#[test]
fn persistent_profiles_refuse_unsafe_modes_links_files_and_names() {
    let fixture = Fixture::new();
    let profile = ProfileConfigDir::for_operator(&fixture.0, "work").expect("private profile");
    for relative in ["structured", "structured/claude", "structured/claude/work"] {
        let path = fixture.0.join(relative);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("unsafe mode");
        assert!(ProfileConfigDir::for_operator(&fixture.0, "work").is_err());
        assert_eq!(
            fs::metadata(&path)
                .expect("unchanged directory")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("restore mode");
    }
    let link = fixture.0.join("structured/claude/link");
    symlink(profile.path(), &link).expect("profile symlink");
    assert!(ProfileConfigDir::for_operator(&fixture.0, "link").is_err());
    fs::write(fixture.0.join("structured/claude/file"), "not a directory").expect("file fixture");
    assert!(ProfileConfigDir::for_operator(&fixture.0, "file").is_err());
    for name in ["", ".", "..", "../escape", "work/nested", "λ"] {
        assert!(ProfileConfigDir::for_operator(&fixture.0, name).is_err());
    }
    assert!(!fixture.0.join("escape").exists());
}

#[test]
fn preparation_failures_leave_no_session_files() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.token_file = None;
    assert!(
        LaunchProfile::prepare(&config, &fixture.0, SESSION_ID, fixture.binary(), |_| None)
            .is_err()
    );
    config.token_file = Some(fixture.file("token", b"", 0o600));
    assert!(
        LaunchProfile::prepare(&config, &fixture.0, SESSION_ID, fixture.binary(), |_| None)
            .is_err()
    );
    config = fixture.config();
    assert!(LaunchProfile::prepare(
        &config,
        &fixture.0.join("missing"),
        SESSION_ID,
        fixture.binary(),
        |_| None
    )
    .is_err());
    assert_eq!(fs::read_dir(&fixture.0).expect("fixture files").count(), 2);
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn operator_warning_is_emitted_for_every_preparation_without_secrets() {
    let fixture = Fixture::new();
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        drop(fixture.profile());
        drop(fixture.profile());
    });
    let output =
        String::from_utf8(capture.0.lock().expect("capture lock").clone()).expect("UTF-8 log");
    assert_eq!(output.matches("structured operator mode").count(), 2);
    assert!(output.contains("#1177"));
    assert!(!output.contains(FAKE_TOKEN));
}

#[test]
fn version_floor_requires_a_resolved_absolute_binary() {
    let fixture = Fixture::new();
    let path = fixture.file("claude", b"not executed", 0o700);
    for version in ["2.1.290", "2.1.296", "2.2.0", "3.0.0"] {
        let binary = VerifiedBinary::check(&path, |invoked, args| {
            assert_eq!(args, ["--version"]);
            assert_eq!(invoked, path.canonicalize().expect("resolved binary"));
            Ok(format!("{version} (Claude Code)\n"))
        })
        .expect("supported version");
        assert_eq!(binary.path(), path);
        assert_eq!(binary.version(), version);
    }
    for output in [
        "2.1.287 (Claude Code)",
        "2.1.289 (Claude Code)",
        "2.0.999 (Claude Code)",
        "1.9.999 (Claude Code)",
        "2.1.290-beta (Claude Code)",
        "nonsense",
        "2.1.296",
        "2.1.296.0 (Claude Code)",
        "2.1 (Claude Code)",
    ] {
        let err =
            VerifiedBinary::check(&path, |_, _| Ok(output.into())).expect_err("unsafe version");
        assert_eq!(err.code(), ErrorCode::StructuredHarnessVersionUnsupported);
        assert_eq!(err.code().retry().as_str(), "after_human");
    }
    for path in [Path::new("claude").to_owned(), fixture.0.join("missing")] {
        assert!(
            VerifiedBinary::check(&path, |_, _| panic!("invalid path reached reader")).is_err()
        );
    }
    assert!(VerifiedBinary::check(&path, |_, _| Err(io::Error::other(
        "version command failed"
    )))
    .is_err());
    let link = fixture.0.join("binary-link");
    symlink(&path, &link).expect("binary symlink");
    let binary = VerifiedBinary::check(&link, |invoked, args| {
        assert_eq!(args, ["--version"]);
        assert_eq!(invoked, path);
        Ok("2.1.296 (Claude Code)".into())
    })
    .expect("resolved symlink");
    assert_eq!(binary.path(), path);
}

#[test]
fn registry_socket_check_is_conservative_and_distinguishes_no_evidence() {
    let fixture = Fixture::new();
    let pid = 123;
    assert!(!registry_has_messaging_socket(&fixture.0, pid).expect("not registered yet"));
    fs::create_dir(fixture.0.join("sessions")).expect("registry directory");
    let path = fixture.0.join("sessions/123.json");
    for record in [
        r#"{"messagingSocketPath":"socket"}"#,
        r#"{"messagingSocketPath":null}"#,
        r#"{"messagingSocketPath":""}"#,
    ] {
        fs::write(&path, record).expect("registry");
        assert!(registry_has_messaging_socket(&fixture.0, pid).expect("forbidden field"));
    }
    fs::write(&path, r#"{"sessionId":"fixture"}"#).expect("safe registry");
    assert!(!registry_has_messaging_socket(&fixture.0, pid).expect("no socket"));
    for record in ["not-json", "null", "[]"] {
        fs::write(&path, record).expect("invalid registry");
        assert!(registry_has_messaging_socket(&fixture.0, pid).is_err());
    }
    let oversized = format!("{{}}{}", " ".repeat(1_048_576));
    fs::write(&path, oversized).expect("oversized registry");
    assert!(registry_has_messaging_socket(&fixture.0, pid).is_err());
    fs::remove_file(&path).expect("remove oversized record");
    let target = fixture.file("registry-target", b"{}", 0o600);
    symlink(&target, &path).expect("registry symlink");
    assert!(registry_has_messaging_socket(&fixture.0, pid).is_err());
    fs::remove_file(&path).expect("remove record");
    fs::create_dir(&path).expect("unreadable as file");
    assert!(registry_has_messaging_socket(&fixture.0, pid).is_err());
}
