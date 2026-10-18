use super::*;
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}
#[test]
fn retained_cli_requires_exclusive_selection_and_all_confirmation() {
    assert!(matches!(
        parse(&args(&["list"])),
        Some(Method::SessionRetainedList(_))
    ));
    assert!(matches!(
        parse(&args(&["purge", "--dead"])),
        Some(Method::SessionRetainedPurge(_))
    ));
    assert!(matches!(
        parse(&args(&["purge", "--all", "--yes", "--older-than", "24h"])),
        Some(Method::SessionRetainedPurge(PurgeParams {
            older_than_ms: Some(86_400_000),
            ..
        }))
    ));
    for raw in [
        &["purge"][..],
        &["purge", "--all"],
        &["purge", "--dead", "--all", "--yes"],
        &["purge", "--dead", "--dead"],
        &["purge", "--dead", "--older-than"],
        &["purge", "--dead", "--older-than", "bad"],
        &["list", "--all"],
        &["purge", "--all", "--yes", "--yes"],
    ] {
        assert!(parse(&args(raw)).is_none(), "{raw:?}");
    }
}
#[test]
fn retained_doctor_text_reports_both_kinds_and_the_operator_purge_hint() {
    let result = ResponseResult::Retained {
        records: vec![],
        forward: 2,
        dead: 129,
        removed: 0,
        purge_hint: Some("flk session retained purge --dead".into()),
    };
    let message = text(&result, true).unwrap();
    assert!(message.contains("FORWARD 2, DEAD 129"));
    assert!(message.contains("flk session retained purge --dead"));
    assert!(!message.contains("slot="));
}
