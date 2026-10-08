//! Pure restart policy and launch-plan reconstruction (#582).
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent_resume::{AgentResumePlan, PersistedAgentSession};

/// A failed restart's requested identity and launch plan, held separately
/// while the replacement is alive and promoted to hibernation on exit.
#[derive(Debug, Clone)]
pub(crate) struct RestartRetry {
    pub session: PersistedAgentSession,
    pub plan: AgentResumePlan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RestartPolicy {
    pub enabled: bool,
    /// Operator-approved startup dialogs. All configured visible lines must match.
    pub dialogs: Vec<RestartDialogProfile>,
    pub restart_grace_secs: u64,
    pub kill_grace_secs: u64,
    pub verify_timeout_secs: u64,
    pub settle_ms: u64,
    pub fresh_ms: u64,
    pub operator_quiet_ms: u64,
    pub flush_wait_ms: u64,
    /// Zero disables an automatic limit.
    pub hard_rss_cap: u64,
    pub host_floor: u64,
    pub hard_spin_for_secs: u64,
    pub spin_cpu_percent: f32,
    pub hard_unresponsive_for_secs: u64,
    pub max_restarts: usize,
    pub window_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartDialogProfile {
    pub agent: String,
    pub lines: Vec<String>,
    pub answer: RestartDialogAnswer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartDialogAnswer {
    Enter,
    Escape,
}

pub(crate) fn dialog_answer(
    policy: &RestartPolicy,
    agent: &str,
    argv: &[String],
    screen: &str,
) -> Option<&'static [u8]> {
    let lower = screen.to_lowercase();
    if lower.contains("/login")
        || lower.contains("login expired")
        || lower.contains("sign in")
        || lower.contains("authenticate")
    {
        return None;
    }
    let matches = |lines: &[String]| {
        lines.len() >= 2
            && lines.iter().all(|expected| {
                !expected.trim().is_empty() && screen.lines().any(|line| line.trim() == expected)
            })
    };
    // The recorded ADR-0019 probe proves these two visible controls. The
    // original launch flag is the operator's approval for this exact dialog.
    if agent == "claude"
        && argv
            .iter()
            .any(|a| a == "--dangerously-load-development-channels")
        && matches(&[
            "WARNING: Loading development channels".into(),
            "❯ 1. I am using this for local development".into(),
        ])
    {
        return Some(b"\r");
    }
    policy
        .dialogs
        .iter()
        .find(|profile| profile.agent == agent && matches(&profile.lines))
        .map(|profile| match profile.answer {
            RestartDialogAnswer::Enter => b"\r".as_slice(),
            RestartDialogAnswer::Escape => b"\x1b".as_slice(),
        })
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            dialogs: Vec::new(),
            restart_grace_secs: 300,
            kill_grace_secs: 10,
            verify_timeout_secs: 120,
            settle_ms: 2_000,
            fresh_ms: 2_500,
            operator_quiet_ms: 15_000,
            flush_wait_ms: 1_000,
            // Automatic interventions are opt-in. Grace expiry still bounds
            // every explicit request, including a request from a wedged agent.
            hard_rss_cap: 0,
            host_floor: 0,
            hard_spin_for_secs: 0,
            spin_cpu_percent: 95.0,
            hard_unresponsive_for_secs: 0,
            max_restarts: 3,
            window_secs: 1_800,
        }
    }
}

impl RestartPolicy {
    pub fn automatic(&self) -> bool {
        self.enabled
            && (self.hard_rss_cap > 0
                || self.host_floor > 0
                || self.hard_spin_for_secs > 0
                || self.hard_unresponsive_for_secs > 0)
    }

    pub fn hard_reason(
        &self,
        rss: u64,
        available: u64,
        heaviest: bool,
        spin: Duration,
        silent: Duration,
    ) -> Option<String> {
        if self.hard_rss_cap > 0 && rss >= self.hard_rss_cap {
            Some(format!(
                "hard_rss_cap: {rss} >= {} bytes",
                self.hard_rss_cap
            ))
        } else if heaviest && self.host_floor > 0 && available < self.host_floor {
            Some(format!(
                "host_floor: {available} < {} bytes available",
                self.host_floor
            ))
        } else if self.hard_spin_for_secs > 0
            && spin >= Duration::from_secs(self.hard_spin_for_secs)
        {
            Some(format!("hard_spin_for: {} seconds", spin.as_secs()))
        } else if self.hard_unresponsive_for_secs > 0
            && silent >= Duration::from_secs(self.hard_unresponsive_for_secs)
        {
            Some(format!(
                "hard_unresponsive_for: {} seconds without status or output",
                silent.as_secs()
            ))
        } else {
            None
        }
    }
}

pub(crate) fn rate_limited(
    history: &mut Vec<Instant>,
    now: Instant,
    policy: &RestartPolicy,
) -> bool {
    history
        .retain(|at| now.saturating_duration_since(*at) < Duration::from_secs(policy.window_secs));
    history.len() >= policy.max_restarts
}

/// Keep the executable and launch options, replacing previous session selectors
/// and dropping the original positional opening prompt. Unknown option arities
/// are refused rather than silently discarding an option's value.
pub(crate) fn resume_launch(
    session: &PersistedAgentSession,
    launch: &[String],
) -> Result<AgentResumePlan, String> {
    let mut plan = crate::agent_resume::plan(&session.source, &session.agent, &session.session_ref)
        .ok_or_else(|| "agent has no supported resume integration".to_string())?;
    let executable = launch
        .first()
        .ok_or("original launch argv is unavailable")?;
    if std::path::Path::new(executable)
        .file_name()
        .and_then(|s| s.to_str())
        != Some(session.agent.as_str())
    {
        return Err("original executable does not match the session's harness".into());
    }
    plan.argv[0] = executable.clone();
    let mut index = 1;
    if session.agent == "codex" && launch.get(index).is_some_and(|s| s == "resume") {
        index += 1;
        if launch.get(index).is_some_and(|s| !s.starts_with('-')) {
            index += 1;
        }
    }
    while let Some(arg) = launch.get(index) {
        if arg == "--" {
            break;
        }
        if matches!(arg.as_str(), "--resume" | "--session" | "--session-id") {
            if launch
                .get(index + 1)
                .is_none_or(|value| value.starts_with('-'))
            {
                return Err(format!(
                    "cannot safely preserve {arg}: session value is missing or is another flag"
                ));
            }
            index += 2;
            continue;
        }
        if matches!(arg.as_str(), "--continue" | "-c") && session.agent == "claude"
            || arg == "--fork-session"
            || arg == "--last" && session.agent == "codex"
            || arg.starts_with("--resume=")
            || arg.starts_with("--session=")
            || arg.starts_with("--session-id=")
        {
            index += 1;
            continue;
        }
        if !arg.starts_with('-') {
            // Positional prompts are one-shot input, never a launch flag.
            index += 1;
            continue;
        }
        if session.agent == "claude"
            && matches!(
                arg.as_str(),
                "--add-dir"
                    | "--allowedTools"
                    | "--allowed-tools"
                    | "--disallowedTools"
                    | "--disallowed-tools"
                    | "--mcp-config"
                    | "--plugin-dir"
            )
        {
            let start = index;
            index += 1;
            while launch
                .get(index)
                .is_some_and(|value| !value.starts_with('-'))
            {
                index += 1;
            }
            if index == start + 1 {
                return Err(format!("missing launch value for {arg}"));
            }
            plan.argv.extend_from_slice(&launch[start..index]);
            continue;
        }
        if session.agent == "claude" && arg == "--debug" {
            return Err(
                "use --debug=<categories> to preserve optional debug arguments unambiguously"
                    .into(),
            );
        }
        if session.agent == "opencode" && arg == "--prompt" {
            index += 2;
            continue;
        }
        let takes_value = matches!(
            arg.as_str(),
            "--model"
                | "-m"
                | "--permission-mode"
                | "--mcp-config"
                | "--plugin-dir"
                | "--settings"
                | "--settings-file"
                | "--setting-sources"
                | "--system-prompt"
                | "--append-system-prompt"
                | "--system-prompt-file"
                | "--append-system-prompt-file"
                | "--allowedTools"
                | "--disallowedTools"
                | "--tools"
                | "--agents"
                | "--agent"
                | "--add-dir"
                | "--config"
                | "--sandbox"
                | "-s"
                | "--ask-for-approval"
                | "-a"
                | "--profile"
                | "-p"
                | "--enable"
                | "--disable"
                | "--provider"
                | "--thinking"
                | "--log-level"
                | "--port"
                | "--hostname"
                | "--title"
                | "--channel"
                | "--dangerously-load-development-channels"
                | "--channels"
                | "--effort"
        ) || arg == "-c" && session.agent == "codex";
        let boolean = matches!(
            arg.as_str(),
            "--dangerously-skip-permissions"
                | "--allow-dangerously-skip-permissions"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--full-auto"
                | "--no-alt-screen"
                | "--verbose"
                | "--debug"
                | "--strict-mcp-config"
                | "--disable-slash-commands"
                | "--allow-all-tools"
                | "--allow-all-paths"
                | "--allow-all-urls"
                | "--yolo"
                | "--no-session"
                | "--no-extensions"
                | "--no-skills"
                | "--no-prompt-templates"
        );
        if arg.contains('=') || boolean {
            plan.argv.push(arg.clone());
            index += 1;
        } else if takes_value {
            let value = launch
                .get(index + 1)
                .ok_or_else(|| format!("missing launch value for {arg}"))?;
            plan.argv.extend([arg.clone(), value.clone()]);
            index += 2;
        } else {
            return Err(format!(
                "cannot safely preserve launch option {arg}; unknown option arity"
            ));
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRef;

    #[test]
    fn restart_launch_preserves_flags_and_replaces_session_and_opening_prompt() {
        for agent in ["claude", "codex", "opencode", "copilot", "pi", "hermes"] {
            let session = PersistedAgentSession {
                source: format!("flock:{agent}"),
                agent: agent.into(),
                session_ref: AgentSessionRef::id("same-session").unwrap(),
            };
            let launch = [
                agent,
                "--model",
                "fixture-model",
                "--resume",
                "old-session",
                "original prompt",
            ]
            .map(String::from);
            let result = resume_launch(&session, &launch).unwrap();
            assert!(
                result.argv.contains(&"same-session".into())
                    || result.argv.contains(&"--resume=same-session".into())
            );
            assert!(result
                .argv
                .ends_with(&["--model".into(), "fixture-model".into()]));
            assert!(!result.argv.contains(&"old-session".into()));
            assert!(!result.argv.contains(&"original prompt".into()));
        }
    }

    #[test]
    fn restart_launch_keeps_codex_config_and_claude_channel_values() {
        for (agent, args) in [
            (
                "codex",
                vec![
                    "codex",
                    "resume",
                    "old",
                    "-c",
                    "model_reasoning_effort=high",
                    "--full-auto",
                ],
            ),
            (
                "claude",
                vec![
                    "claude",
                    "-c",
                    "--dangerously-load-development-channels",
                    "plugin:fixture",
                ],
            ),
        ] {
            let session = PersistedAgentSession {
                source: format!("flock:{agent}"),
                agent: agent.into(),
                session_ref: AgentSessionRef::id("same").unwrap(),
            };
            let result = resume_launch(
                &session,
                &args.into_iter().map(String::from).collect::<Vec<_>>(),
            )
            .unwrap();
            assert!(result
                .argv
                .iter()
                .any(|v| v == "model_reasoning_effort=high" || v == "plugin:fixture"));
        }
    }

    #[test]
    fn restart_launch_refuses_ambiguous_options_and_wrong_executables() {
        let session = PersistedAgentSession {
            source: "flock:claude".into(),
            agent: "claude".into(),
            session_ref: AgentSessionRef::id("same").unwrap(),
        };
        assert!(resume_launch(
            &session,
            &["claude".into(), "--unknown".into(), "value".into()]
        )
        .is_err());
        assert!(resume_launch(&session, &["wrapper".into()]).is_err());
        assert!(resume_launch(
            &session,
            &[
                "claude".into(),
                "--resume".into(),
                "--model".into(),
                "x".into()
            ]
        )
        .is_err());
    }

    #[test]
    fn restart_launch_preserves_multiple_claude_option_values() {
        let session = PersistedAgentSession {
            source: "flock:claude".into(),
            agent: "claude".into(),
            session_ref: AgentSessionRef::id("same").unwrap(),
        };
        let args = [
            "claude",
            "--allowedTools",
            "Read",
            "Edit",
            "--add-dir",
            "first",
            "second",
            "--model",
            "fixture",
        ]
        .map(String::from);
        let result = resume_launch(&session, &args).unwrap();
        assert!(result.argv.ends_with(&args[1..]));
    }

    #[test]
    fn restart_limits_apply_at_boundary_and_host_floor_only_to_heaviest() {
        let policy = RestartPolicy {
            hard_rss_cap: 100,
            host_floor: 20,
            hard_spin_for_secs: 10,
            hard_unresponsive_for_secs: 30,
            ..Default::default()
        };
        assert!(policy
            .hard_reason(100, 50, false, Duration::ZERO, Duration::ZERO)
            .unwrap()
            .starts_with("hard_rss_cap"));
        assert!(policy
            .hard_reason(50, 19, false, Duration::ZERO, Duration::ZERO)
            .is_none());
        assert!(policy
            .hard_reason(50, 19, true, Duration::ZERO, Duration::ZERO)
            .unwrap()
            .starts_with("host_floor"));
        assert!(policy
            .hard_reason(50, 50, false, Duration::from_secs(10), Duration::ZERO)
            .unwrap()
            .starts_with("hard_spin_for"));
        assert!(policy
            .hard_reason(50, 50, false, Duration::ZERO, Duration::from_secs(30))
            .unwrap()
            .starts_with("hard_unresponsive_for"));
    }

    #[test]
    fn restart_rate_limit_expires_at_window_boundary() {
        let now = Instant::now();
        let policy = RestartPolicy {
            max_restarts: 2,
            window_secs: 10,
            ..Default::default()
        };
        let mut history = vec![now, now];
        assert!(rate_limited(&mut history, now, &policy));
        assert!(!rate_limited(
            &mut history,
            now + Duration::from_secs(10),
            &policy
        ));
    }
}

/// Stop-hook evidence retained without retaining reply contents.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct RestartCheckpoint {
    pub written_ms: u64,
    pub reply_hash: Option<u64>,
    pub transcript_path: Option<std::path::PathBuf>,
    pub background_tasks: Vec<String>,
    pub tool: Option<String>,
}

pub(crate) fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    text.trim().hash(&mut hash);
    hash.finish()
}

fn checkpoint_path(dir: &std::path::Path, session: &str, kind: &str) -> Option<std::path::PathBuf> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(dir.join(format!("{session}.{kind}.json")))
}

pub(crate) fn checkpoint(session: &str, kind: &str) -> Option<RestartCheckpoint> {
    serde_json::from_slice(
        &std::fs::read(checkpoint_path(
            &crate::config::state_dir().join("restart-checkpoints"),
            session,
            kind,
        )?)
        .ok()?,
    )
    .ok()
}

pub(crate) fn record_checkpoint(input: &serde_json::Value, event: &str) {
    record_checkpoint_in(
        &crate::config::state_dir().join("restart-checkpoints"),
        input,
        event,
    );
}

fn record_checkpoint_in(dir: &std::path::Path, input: &serde_json::Value, event: &str) {
    let Some(session) = input.get("session_id").and_then(serde_json::Value::as_str) else {
        return;
    };
    let kind = match event {
        "Stop" | "StopFailure" => "stop",
        "PreToolUse" => "tool",
        _ => return,
    };
    let Some(path) = checkpoint_path(dir, session, kind) else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    let tasks = input
        .get("background_tasks")
        .and_then(serde_json::Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .take(64)
                .map(|task| {
                    let raw = task
                        .as_str()
                        .or_else(|| task.get("description").and_then(serde_json::Value::as_str))
                        .or_else(|| task.get("id").and_then(serde_json::Value::as_str))
                        .unwrap_or("unnamed background task");
                    crate::app::api_helpers::sanitize_reported_prompt(raw)
                        .chars()
                        .take(256)
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();
    let record = RestartCheckpoint {
        written_ms: crate::app::notifications::now_ms(),
        reply_hash: input
            .get("last_assistant_message")
            .and_then(serde_json::Value::as_str)
            .map(fingerprint),
        transcript_path: input
            .get("transcript_path")
            .and_then(serde_json::Value::as_str)
            .map(std::path::PathBuf::from),
        background_tasks: tasks,
        tool: input
            .get("tool_name")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.chars().take(128).collect()),
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(&record) else {
        return;
    };
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

pub(crate) fn checkpoint_flushed(checkpoint: &RestartCheckpoint, requested_ms: u64) -> bool {
    if checkpoint.written_ms < requested_ms {
        return false;
    }
    let Some(expected) = checkpoint.reply_hash else {
        return false;
    };
    let Some(path) = &checkpoint.transcript_path else {
        return false;
    };
    let Ok(read) = crate::agent_transcript::read_tail(path) else {
        return false;
    };
    read.events
        .iter()
        .rev()
        .find_map(|event| match event {
            crate::agent_transcript::TranscriptEvent::Message {
                role: crate::agent_transcript::Role::Assistant,
                blocks,
                ..
            } => Some(
                blocks
                    .iter()
                    .filter_map(|b| {
                        if let crate::agent_transcript::Block::Text(text) = b {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .is_some_and(|text| fingerprint(&text) == expected)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    #[test]
    fn restart_flush_check_requires_the_latest_reply_and_current_stop() {
        let dir =
            std::env::temp_dir().join(format!("flock-restart-checkpoint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        let write = |reply: &str| {
            std::fs::write(&path, format!("{}\n", serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"text", "text":reply}]}}))).unwrap()
        };
        write("old turn");
        let checkpoint = RestartCheckpoint {
            written_ms: 20,
            reply_hash: Some(fingerprint("new turn")),
            transcript_path: Some(path.clone()),
            ..Default::default()
        };
        assert!(!checkpoint_flushed(&checkpoint, 10));
        write("new turn");
        assert!(checkpoint_flushed(&checkpoint, 10));
        assert!(!checkpoint_flushed(&checkpoint, 21));
        record_checkpoint_in(
            &dir,
            &serde_json::json!({"session_id":"fixture-session", "last_assistant_message":"new turn", "transcript_path":path, "background_tasks":[{"description":"fixture watcher"}]}),
            "Stop",
        );
        let captured: RestartCheckpoint =
            serde_json::from_slice(&std::fs::read(dir.join("fixture-session.stop.json")).unwrap())
                .unwrap();
        assert_eq!(captured.background_tasks, vec!["fixture watcher"]);
        assert_eq!(captured.reply_hash, checkpoint.reply_hash);
        assert!(checkpoint_path(&dir, "../escape", "stop").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn restart_dialogs_require_all_controls_and_never_answer_login() {
        let policy = RestartPolicy::default();
        let args = [
            "claude",
            "--dangerously-load-development-channels",
            "server:flock",
        ]
        .map(String::from);
        let screen =
            "WARNING: Loading development channels\n❯ 1. I am using this for local development";
        assert_eq!(
            dialog_answer(&policy, "claude", &args, screen),
            Some(b"\r".as_slice())
        );
        assert!(dialog_answer(&policy, "claude", &[], screen).is_none());
        assert!(dialog_answer(&policy, "codex", &args, screen).is_none());
        assert!(dialog_answer(
            &policy,
            "claude",
            &args,
            "WARNING: Loading development channels"
        )
        .is_none());
        assert!(dialog_answer(
            &policy,
            "claude",
            &args,
            &format!("{screen}\nLogin expired · /login")
        )
        .is_none());
        assert!(dialog_answer(&policy, "claude", &args, "unknown startup dialog").is_none());
    }
    #[test]
    fn restart_dialog_profile_is_operator_approved_and_harness_specific() {
        let policy = RestartPolicy {
            dialogs: vec![RestartDialogProfile {
                agent: "pi".into(),
                lines: vec![
                    "Fixture startup question".into(),
                    "Press Enter to continue".into(),
                ],
                answer: RestartDialogAnswer::Enter,
            }],
            ..Default::default()
        };
        let screen = "Fixture startup question\nPress Enter to continue";
        assert_eq!(
            dialog_answer(&policy, "pi", &[], screen),
            Some(b"\r".as_slice())
        );
        assert!(dialog_answer(&policy, "claude", &[], screen).is_none());
    }
}
