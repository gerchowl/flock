//! The approved #1109 arity and same-session restart rules.
use super::*;
use crate::agent_resume::AgentSessionRef;

fn session() -> PersistedAgentSession {
    PersistedAgentSession {
        source: "flock:claude".into(),
        agent: "claude".into(),
        session_ref: AgentSessionRef::id("native-fixture").unwrap(),
    }
}

fn launch(args: &[&str]) -> Vec<String> {
    std::iter::once("claude")
        .chain(args.iter().copied())
        .map(String::from)
        .collect()
}

#[test]
fn optional_1109_known_values_and_bare_flag_preserve_next_option() {
    for value in ["true", "false", "1", "0", "yes", "no", "on", "off"] {
        for args in [
            vec![
                "--prompt-suggestions".to_string(),
                value.into(),
                "--model".into(),
                "fixture".into(),
            ],
            vec![
                format!("--prompt-suggestions={value}"),
                "--model".into(),
                "fixture".into(),
            ],
        ] {
            let mut original = vec!["claude".into()];
            original.extend(args.clone());
            let result = resume_launch(&session(), &original).unwrap();
            assert!(
                result.plan.argv.ends_with(&args),
                "{value}: {:?}",
                result.plan.argv
            );
        }
    }
    let result = resume_launch(
        &session(),
        &launch(&[
            "--prompt-suggestions",
            "opening prompt",
            "--model",
            "fixture",
        ]),
    )
    .unwrap();
    assert!(result
        .plan
        .argv
        .ends_with(&launch(&["--prompt-suggestions", "--model", "fixture"])[1..]));
    let result = resume_launch(
        &session(),
        &launch(&["--prompt-suggestions", "--model", "fixture"]),
    )
    .unwrap();
    assert!(result
        .plan
        .argv
        .ends_with(&launch(&["--prompt-suggestions", "--model", "fixture"])[1..]));
}

#[test]
fn optional_1109_unknown_and_open_values_refuse_even_equals_forms() {
    {
        let flag = "--remote-control";
        for option in [flag.to_string(), format!("{flag}=fixture")] {
            let error =
                resume_launch(&session(), &launch(&[&option, "--model", "fixture"])).unwrap_err();
            assert!(error.contains(flag), "{error}");
        }
    }
    for args in [vec!["--debug"], vec!["--debug", "fixture"]] {
        let error = resume_launch(&session(), &launch(&args)).unwrap_err();
        assert!(error.contains("--debug"), "{error}");
    }
    let error = resume_launch(&session(), &launch(&["--prompt-suggestions=maybe"])).unwrap_err();
    assert!(error.contains("--prompt-suggestions"), "{error}");
}

#[test]
fn optional_1109_tools_preserve_every_value() {
    for args in [
        vec!["--tools", "Read", "Edit", "--model", "fixture"],
        vec!["--tools=Read", "--model", "fixture"],
        vec!["--tools", "", "--model", "fixture"],
    ] {
        let original = launch(&args);
        let result = resume_launch(&session(), &original).unwrap();
        assert!(
            result.plan.argv.ends_with(&original[1..]),
            "{:?}",
            result.plan.argv
        );
    }
    assert!(resume_launch(&session(), &launch(&["--tools", "--model", "fixture"])).is_err());
}

#[test]
fn optional_1109_workspace_and_pr_selectors_are_replaced() {
    let args = launch(&[
        "--worktree",
        "fixture-tree",
        "-w=another-tree",
        "--from-pr=42",
        "--resume",
        "old-session",
        "--continue",
        "--session-id",
        "old-id",
        "--model",
        "fixture",
    ]);
    let result = resume_launch(&session(), &args).unwrap();
    assert_eq!(
        result.plan.argv,
        launch(&["--resume", "native-fixture", "--model", "fixture"])
    );
}

#[test]
fn optional_1109_remote_execution_selectors_are_refused() {
    for flag in ["--cloud", "--teleport"] {
        for args in [
            vec![flag.to_string()],
            vec![flag.to_string(), "fixture-session".into()],
            vec![format!("{flag}=fixture-session")],
        ] {
            let mut original = vec!["claude".into()];
            original.extend(args);
            let error = resume_launch(&session(), &original).unwrap_err();
            assert!(error.contains(flag), "{error}");
        }
    }
}

#[test]
fn optional_1109_claude_options_do_not_become_other_harness_options() {
    let mut session = session();
    session.agent = "codex".into();
    session.source = "flock:codex".into();
    let error = resume_launch(
        &session,
        &[
            "codex".into(),
            "--prompt-suggestions".into(),
            "false".into(),
        ],
    )
    .unwrap_err();
    assert!(error.contains("--prompt-suggestions"), "{error}");
}

#[test]
fn optional_1109_drop_report_serializes_and_old_results_still_decode() {
    let report = serde_json::json!({"type":"agent_restart_queued","pane_id":"fixture-pane","session":"fixture-session","stop_only":false,"dropped_flags":[{"flag":"--worktree","reason":"existing cwd is preserved"}]});
    let result: crate::api::schema::ResponseResult =
        serde_json::from_value(report.clone()).unwrap();
    assert_eq!(serde_json::to_value(result).unwrap(), report);
    let mut legacy = report;
    legacy.as_object_mut().unwrap().remove("dropped_flags");
    let result: crate::api::schema::ResponseResult =
        serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(result).unwrap(), legacy);
}

#[test]
fn optional_1109_files_preserve_every_value() {
    for args in [
        vec![
            "--file",
            "file_one:one.txt",
            "file_two:two.txt",
            "--model",
            "fixture",
        ],
        vec!["--file=file_one:one.txt", "--model", "fixture"],
    ] {
        let original = launch(&args);
        let result = resume_launch(&session(), &original).unwrap();
        assert!(
            result.plan.argv.ends_with(&original[1..]),
            "{:?}",
            result.plan.argv
        );
    }
    assert!(resume_launch(&session(), &launch(&["--file", "--model", "fixture"])).is_err());
}

#[test]
fn optional_1109_each_dropped_selector_has_a_reason_in_all_forms() {
    for flag in ["--worktree", "-w", "--from-pr"] {
        for args in [
            vec![flag.to_string()],
            vec![flag.to_string(), "fixture".into()],
            vec![format!("{flag}=fixture")],
        ] {
            let mut original = vec!["claude".into()];
            original.extend(args);
            original.extend(["--model".into(), "fixture-model".into()]);
            let result = resume_launch(&session(), &original).unwrap();
            assert_eq!(
                result.plan.argv,
                launch(&["--resume", "native-fixture", "--model", "fixture-model"])
            );
            assert_eq!(result.dropped_flags.len(), 1);
            assert_eq!(result.dropped_flags[0].flag, flag);
            assert!(!result.dropped_flags[0].reason.is_empty());
        }
    }
}

#[test]
fn inline_1109_variadic_options_do_not_replay_the_opening_prompt() {
    for flag in [
        "--add-dir",
        "--allowedTools",
        "--allowed-tools",
        "--disallowedTools",
        "--disallowed-tools",
        "--mcp-config",
        "--plugin-dir",
        "--betas",
        "--tools",
        "--file",
    ] {
        let inline = format!("{flag}=fixture");
        let original = launch(&[&inline, "implement the fixture", "--model", "fixture-model"]);
        let result = resume_launch(&session(), &original).unwrap();
        assert_eq!(
            result.plan.argv,
            launch(&[
                "--resume",
                "native-fixture",
                &inline,
                "--model",
                "fixture-model"
            ]),
            "{flag} replayed the opening prompt"
        );
    }
}

#[test]
fn inline_1109_debug_filters_are_preserved() {
    for arg in ["--debug=fixture", "-d=fixture"] {
        let result = resume_launch(&session(), &launch(&[arg, "opening prompt"])).unwrap();
        assert_eq!(
            result.plan.argv,
            launch(&["--resume", "native-fixture", arg])
        );
    }
}

#[test]
fn inline_1109_short_resume_is_replaced_by_the_native_selector() {
    for args in [vec!["-r", "old-session"], vec!["-r=old-session"]] {
        let result = resume_launch(&session(), &launch(&args)).unwrap();
        assert_eq!(result.plan.argv, launch(&["--resume", "native-fixture"]));
    }
    for args in [vec!["-r"], vec!["-r", "--model", "fixture"]] {
        let error = resume_launch(&session(), &launch(&args)).unwrap_err();
        assert!(error.contains("-r"), "{error}");
    }
    let mut other_session = session();
    other_session.agent = "codex".into();
    other_session.source = "flock:codex".into();
    assert!(resume_launch(
        &other_session,
        &["codex".into(), "-r".into(), "old-session".into()]
    )
    .is_err());
}

#[test]
fn optional_1659_plugin_directories_are_single_valued_and_repeatable() {
    for (args, kept) in [
        (
            vec![
                "--plugin-dir",
                "plugin with spaces",
                "opening prompt",
                "--plugin-dir",
                "second.zip",
                "another prompt",
                "--model",
                "fixture",
            ],
            vec![
                "--plugin-dir",
                "plugin with spaces",
                "--plugin-dir",
                "second.zip",
                "--model",
                "fixture",
            ],
        ),
        (
            vec![
                "--plugin-dir=first",
                "opening prompt",
                "--plugin-dir=second.zip",
            ],
            vec!["--plugin-dir=first", "--plugin-dir=second.zip"],
        ),
    ] {
        let result = resume_launch(&session(), &launch(&args)).unwrap();
        assert_eq!(
            result.plan.argv,
            launch(&[vec!["--resume", "native-fixture"], kept].concat())
        );
    }
    for args in [
        vec!["--plugin-dir"],
        vec!["--plugin-dir", "--model", "fixture"],
    ] {
        assert!(resume_launch(&session(), &launch(&args))
            .unwrap_err()
            .contains("missing launch value"));
    }
}

#[test]
fn optional_1659_print_aliases_follow_verified_harness_arities() {
    for flag in ["-p", "--print"] {
        let result = resume_launch(
            &session(),
            &launch(&[flag, "opening prompt", "--model", "fixture"]),
        )
        .unwrap();
        assert_eq!(
            result.plan.argv,
            launch(&["--resume", "native-fixture", flag, "--model", "fixture"])
        );
        assert!(resume_launch(&session(), &launch(&[flag])).is_ok());
    }
    let mut arity_failures = Vec::new();
    let mut pi = session();
    pi.agent = "pi".into();
    pi.source = "flock:pi".into();
    for flag in ["-p", "--print"] {
        for args in [vec!["pi", flag], vec!["pi", flag, "opening prompt"]] {
            let result = resume_launch(&pi, &args.iter().map(|s| (*s).into()).collect::<Vec<_>>());
            match result {
                Ok(result)
                    if result.plan.argv
                        == ["pi", "--session", "native-fixture", flag].map(String::from) => {}
                result => arity_failures.push(format!("{args:?}: {result:?}")),
            }
        }
    }
    for agent in ["opencode", "copilot", "hermes"] {
        let mut unknown = session();
        unknown.agent = agent.into();
        unknown.source = format!("flock:{agent}");
        for args in [
            vec![agent, "-p"],
            vec![agent, "-p", "opening prompt"],
            vec![agent, "-p=fixture"],
        ] {
            let result = resume_launch(
                &unknown,
                &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            );
            match result {
                Err(error) if error.contains("-p") && error.contains("unknown option arity") => {}
                result => arity_failures.push(format!("{args:?}: {result:?}")),
            }
        }
    }
    assert!(arity_failures.is_empty(), "{arity_failures:#?}");
    let mut codex = session();
    codex.agent = "codex".into();
    codex.source = "flock:codex".into();
    let result = resume_launch(
        &codex,
        &vec!["codex", "-p", "fixture-profile", "--model", "fixture"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(result
        .plan
        .argv
        .ends_with(&["-p", "fixture-profile", "--model", "fixture"].map(String::from)));
}

#[test]
fn optional_1659_bare_debug_alias_is_refused() {
    for args in [vec!["-d"], vec!["-d", "fixture"]] {
        let error = resume_launch(&session(), &launch(&args)).unwrap_err();
        assert!(
            error.contains("-d") && error.contains("optional value set is open or unknown"),
            "{error}"
        );
    }
    let result = resume_launch(&session(), &launch(&["-d=fixture", "opening prompt"])).unwrap();
    assert_eq!(
        result.plan.argv,
        launch(&["--resume", "native-fixture", "-d=fixture"])
    );
}

#[test]
fn restart_refusals_name_options_without_their_values() {
    let value = "fixture-sensitive-profile";
    let mut failures = Vec::new();
    for agent in ["hermes", "opencode", "copilot"] {
        let session = PersistedAgentSession {
            source: format!("flock:{agent}"),
            agent: agent.into(),
            session_ref: AgentSessionRef::id("native-fixture").unwrap(),
        };
        for (flag, args) in [
            ("-p", vec![agent.into(), format!("-p={value}")]),
            ("-p", vec![agent.into(), "-p".into(), value.into()]),
            (
                "--unverified",
                vec![agent.into(), "--unverified".into(), value.into()],
            ),
        ] {
            let error = resume_launch(&session, &args).unwrap_err();
            if !error.contains(flag)
                || !error.contains("unknown option arity")
                || error.contains(value)
            {
                failures.push(format!("{agent}: {error}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "option values leaked or refusal changed: {failures:?}"
    );
}
