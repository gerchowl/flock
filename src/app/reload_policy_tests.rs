use super::*;
use crate::config::reload::SectionStatus;
use std::{ffi::OsString, fs, path::PathBuf};

struct ConfigPath(Option<OsString>);
impl Drop for ConfigPath {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, value);
        } else {
            std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        }
    }
}
fn fixture() -> (
    App,
    crate::structured::launch::SessionConfigDir,
    PathBuf,
    ConfigPath,
) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let path = root.path().join("config.toml");
    let guard = ConfigPath(std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR));
    std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    (app, root, path, guard)
}
fn state(report: &crate::config::ConfigReloadReport, name: &str) -> SectionStatus {
    report
        .sections
        .iter()
        .find(|s| s.section == name)
        .unwrap()
        .status
}

#[test]
fn disk_reload_applies_structured_opt_in_profile_and_fleet_limits() {
    let _env = crate::config::test_config_env_guard();
    let (mut app, _root, path, _path) = fixture();
    fs::write(&path, "[structured]\nrun_as='operator'\nprofile='reloaded'\n[fleet]\nagent_spawn_enabled=true\nmax_concurrent_agents=3\nmax_spawn_depth=4\nmax_spawn_fanout=5\n").unwrap();
    let report = app.reload_config();
    assert_eq!(report.status, crate::config::ConfigReloadStatus::Applied);
    assert_eq!(state(&report, "structured"), SectionStatus::Applied);
    assert_eq!(state(&report, "fleet"), SectionStatus::Applied);
    assert_eq!(
        app.state.config.structured.run_as,
        crate::structured::launch::RunAs::Operator
    );
    assert_eq!(app.state.config.structured.profile, "reloaded");
    assert!(app.state.config.fleet.agent_spawn_enabled);
    assert_eq!(app.state.config.fleet.max_concurrent_agents, 3);
    assert_eq!(app.state.config.fleet.max_spawn_depth, 4);
    assert_eq!(app.state.config.fleet.max_spawn_fanout, 5);
    assert!(
        app.reload_config().sections.is_empty(),
        "unchanged policies are not reported as changed"
    );
}

#[test]
fn disk_reload_keeps_invalid_policies_and_security_account_boundary() {
    let _env = crate::config::test_config_env_guard();
    let (mut app, _root, path, _path) = fixture();
    app.state.config.structured.profile = "admitted-profile".into();
    app.state.config.structured.run_as = crate::structured::launch::RunAs::Operator;
    app.state.config.fleet.agent_spawn_enabled = true;
    app.state.config.fleet.max_concurrent_agents = 7;
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: 41,
        dedicated: Ok(Some(42)).into(),
    };
    fs::write(&path, "[structured]\nrun_as='invalid'\n[fleet]\nmax_concurrent_agents='invalid'\n[security]\nagent_account='none'\n[sessions]\nstall_ms=5000\n").unwrap();
    let report = app.reload_config();
    assert_eq!(report.status, crate::config::ConfigReloadStatus::Partial);
    assert_eq!(state(&report, "structured"), SectionStatus::Invalid);
    assert_eq!(state(&report, "fleet"), SectionStatus::Invalid);
    assert_eq!(state(&report, "security"), SectionStatus::RestartRequired);
    assert_eq!(state(&report, "sessions"), SectionStatus::RestartRequired);
    assert_eq!(app.state.config.structured.profile, "admitted-profile");
    assert_eq!(
        app.state.config.structured.run_as,
        crate::structured::launch::RunAs::Operator
    );
    assert!(app.state.config.fleet.agent_spawn_enabled);
    assert_eq!(app.state.config.fleet.max_concurrent_agents, 7);
    assert_eq!(
        app.state.config.security.agent_account,
        crate::config::model::AgentAccount::FlkAgent
    );
    assert_eq!(app.state.config.sessions.stall_ms, 120_000);
    assert_eq!(
        app.runtime_config.uid_policy.classify(Some(42)),
        crate::structured::identity::Principal::Agent
    );
    fs::write(&path, "[structured]\nrun_as='invalid'\n[fleet]\nagent_spawn_enabled=false\nmax_concurrent_agents=2\n").unwrap();
    let mixed = app.reload_config();
    assert_eq!(state(&mixed, "structured"), SectionStatus::Invalid);
    assert_eq!(state(&mixed, "fleet"), SectionStatus::Applied);
    assert_eq!(app.state.config.structured.profile, "admitted-profile");
    assert!(!app.state.config.fleet.agent_spawn_enabled);
    assert_eq!(app.state.config.fleet.max_concurrent_agents, 2);
    fs::write(&path, "[structured]\nrun_as='operator'\nprofile='new-valid'\n[fleet]\nmax_concurrent_agents='invalid'\n").unwrap();
    let mixed = app.reload_config();
    assert_eq!(state(&mixed, "structured"), SectionStatus::Applied);
    assert_eq!(state(&mixed, "fleet"), SectionStatus::Invalid);
    assert_eq!(app.state.config.structured.profile, "new-valid");
    assert!(!app.state.config.fleet.agent_spawn_enabled);
    assert_eq!(app.state.config.fleet.max_concurrent_agents, 2);
}
