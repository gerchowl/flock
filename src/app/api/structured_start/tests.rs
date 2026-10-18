#[test]
fn configured_stall_policy_is_defaulted_and_range_checked() {
    let default: crate::config::Config = toml::from_str("").unwrap();
    assert_eq!(default.sessions.stall_ms, 120_000);
    for value in [5000, 3_600_000] {
        let config: crate::config::Config =
            toml::from_str(&format!("[sessions]\nstall_ms = {value}\n")).unwrap();
        assert_eq!(config.sessions.stall_ms, value);
    }
    for value in [0, 4999, 3_600_001] {
        let error =
            toml::from_str::<crate::config::Config>(&format!("[sessions]\nstall_ms = {value}\n"))
                .unwrap_err();
        assert_eq!(error.message(), format!("invalid value: integer `{value}`, expected sessions.stall_ms in 5000..=3600000 milliseconds"));
        assert!(error.to_string().contains("stall_ms"));
    }
}
