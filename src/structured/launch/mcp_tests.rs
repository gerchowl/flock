use super::*;
use crate::structured::{lease::SessionLeases, runtime::tests::Fixture};
use std::os::unix::fs::PermissionsExt;

fn mcp_path(profile: &LaunchProfile) -> PathBuf {
    let index = profile
        .args()
        .iter()
        .position(|arg| arg == "--mcp-config")
        .unwrap();
    PathBuf::from(&profile.args()[index + 1])
}
fn registration(profile: &LaunchProfile) -> serde_json::Value {
    serde_json::from_slice(&fs::read(mcp_path(profile)).unwrap()).unwrap()
}
fn lease(fixture: &Fixture) -> super::super::lease::Lease {
    SessionLeases::default()
        .reserve(
            "fixture-agent",
            "00000000-0000-4000-8000-000000000001",
            None,
        )
        .unwrap_or_else(|error| panic!("{error:?}: {}", fixture.root.path().display()))
}

#[test]
fn session_1441_registration_is_exact_pinned_and_credential_free() {
    let fixture = Fixture::new();
    let profile = fixture.profile("mcp-config", "2.1.296");
    let lease = lease(&fixture);
    profile
        .register_flock_mcp(Some(&lease), &fixture.mcp())
        .unwrap();
    assert_eq!(
        registration(&profile),
        serde_json::json!({"mcpServers":{"flock":{
            "type":"stdio", "command":fixture.root.path().join("claude").canonicalize().unwrap(),
            "args":["mcp","serve"], "env":{"FLOCK_SOCKET_PATH":fixture.root.path().join("api.sock")}
        }}})
    );
    let raw = fs::read_to_string(mcp_path(&profile)).unwrap();
    assert!(!raw.contains("fixture-token-not-a-credential"));
    assert!(!raw.contains("fixture-agent"));
    assert!(SessionMcp::new(Path::new("flk"), &fixture.root.path().join("api.sock")).is_err());
    assert!(SessionMcp::new(&fixture.root.path().join("claude"), Path::new("api.sock")).is_err());
}

#[test]
fn session_1441_registration_never_writes_operator_profile_config() {
    let fixture = Fixture::new();
    let profile = fixture.profile("mcp-config", "2.1.296");
    let operator = fixture.file(
        ".claude.json",
        br#"{"mcpServers":{"custom":{"command":"operator-only"}}}"#,
        0o600,
    );
    let persistent = profile.config_dir().join(".claude.json");
    fs::write(&persistent, fs::read(&operator).unwrap()).unwrap();
    let before = (fs::read(&operator).unwrap(), fs::read(&persistent).unwrap());
    profile
        .register_flock_mcp(Some(&lease(&fixture)), &fixture.mcp())
        .unwrap();
    assert!(registration(&profile)["mcpServers"]["flock"].is_object());
    assert_eq!(
        (fs::read(operator).unwrap(), fs::read(persistent).unwrap()),
        before
    );
}

#[test]
fn session_1441_missing_mismatched_or_released_lease_refuses_without_registration() {
    let fixture = Fixture::new();
    let profile = fixture.profile("mcp-config", "2.1.296");
    let leases = SessionLeases::default();
    let other = leases
        .reserve("fixture-agent", "different-session", None)
        .unwrap();
    let mut released = lease(&fixture);
    released.release();
    for lease in [None, Some(&other), Some(&released)] {
        let error = profile
            .register_flock_mcp(lease, &fixture.mcp())
            .unwrap_err();
        assert!(matches!(error, LaunchError::McpLeaseRequired));
        assert_eq!(error.code(), ErrorCode::StructuredMcpLeaseRequired);
        assert_eq!(error.code().exit(), crate::cli::exit::USAGE);
        assert_eq!(registration(&profile), serde_json::json!({"mcpServers":{}}));
        assert!(!mcp_path(&profile).with_file_name("mcp.next.json").exists());
    }
}

#[test]
fn session_1441_config_is_private_rotated_and_owned_until_drop() {
    let fixture = Fixture::new();
    let profile = fixture.profile("mcp-config", "2.1.296");
    let lease = lease(&fixture);
    let path = mcp_path(&profile);
    let generated = path.parent().unwrap().to_owned();
    profile
        .register_flock_mcp(Some(&lease), &fixture.mcp())
        .unwrap();
    assert!(registration(&profile)["mcpServers"]["flock"].is_object());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&generated).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let second_socket = fixture.root.path().join("second.sock");
    let endpoint = SessionMcp::new(&fixture.root.path().join("claude"), &second_socket).unwrap();
    profile.register_flock_mcp(Some(&lease), &endpoint).unwrap();
    assert_eq!(
        registration(&profile)["mcpServers"]["flock"]["env"]["FLOCK_SOCKET_PATH"],
        second_socket.to_str().unwrap()
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!generated.join("mcp.next.json").exists());
    drop(profile);
    assert!(!generated.exists());
    let next = fixture.profile("mcp-config", "2.1.296");
    assert_ne!(mcp_path(&next), path);
}

#[test]
fn session_1441_registered_config_is_removed_on_spawn_failure() {
    let fixture = Fixture::new();
    let profile = fixture.profile("mcp-config", "2.1.296");
    let path = mcp_path(&profile);
    profile
        .register_flock_mcp(Some(&lease(&fixture)), &fixture.mcp())
        .unwrap();
    assert!(registration(&profile)["mcpServers"]["flock"].is_object());
    let error = profile
        .try_spawn(|| Err(io::Error::other("owned fake spawn failure")))
        .unwrap_err();
    assert_eq!(error.to_string(), "owned fake spawn failure");
    assert!(!path.parent().unwrap().exists());
}
