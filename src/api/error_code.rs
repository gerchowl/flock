//! The error-code registry (ADR-0029 §4, #1017).
//!
//! Every `error.code` the socket API and the CLI emit is a variant here, with
//! the retry class that tells a caller whether running the call again is safe.
//! The emit sites still pass string literals: the tests below hold each of
//! those literals to this table, and pin the table itself, so a new code is an
//! addition here and a renamed or dropped one fails in the same diff.

use crate::cli::exit::{self, Retry};

macro_rules! error_codes {
    ($($retry:ident: $($variant:ident = $code:literal),+;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[cfg_attr(not(test), allow(dead_code))]
        pub(crate) enum ErrorCode {
            $($($variant,)+)*
        }

        #[cfg_attr(not(test), allow(dead_code))]
        impl ErrorCode {
            pub(crate) const ALL: &'static [ErrorCode] = &[$($(ErrorCode::$variant,)+)*];

            pub(crate) const fn as_str(self) -> &'static str {
                match self {
                    $($(ErrorCode::$variant => $code,)+)*
                }
            }

            pub(crate) const fn retry(self) -> Retry {
                match self {
                    $($(ErrorCode::$variant => Retry::$retry,)+)*
                }
            }
        }
    };
}

impl ErrorCode {
    pub(crate) fn from_code(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == code)
    }

    /// The exit status a CLI call ending in this error returns. A send-like or
    /// guarded verb returns `REFUSED` instead for a `later` refusal, through
    /// `cli::refusal_exit` (#1025). MCP's `data.exit` reads it through
    /// `cli::error_code_exit` (#1024).
    pub(crate) const fn exit(self) -> i32 {
        match self {
            ErrorCode::InputWriteCancelled
            | ErrorCode::WorktreeCheckoutUnreachable
            | ErrorCode::WorktreeRepositoryUnreachable => exit::REFUSED,
            ErrorCode::InputWriteFailed | ErrorCode::InputWriteUnknown => exit::UNCONFIRMED,
            ErrorCode::ServerVersionGap => exit::PROTOCOL_GAP,
            // ADR-0029 §2: the wrong operation requires a corrected call.
            // A parked restart needs resume; invalid wait parks need new predicates.
            ErrorCode::StructuredMcpLeaseRequired
            | ErrorCode::Parked
            | ErrorCode::WaitPredicateInvalid
            | ErrorCode::WaitPredicateUnsupported
            | ErrorCode::WaitDeadlineTooLong => exit::USAGE,
            // ADR-0029 §2: code 13 covers the ADR-0014 boundary.
            ErrorCode::NotAuthorized | ErrorCode::OperatorOnly | ErrorCode::GoalCallerRefused => {
                exit::POLICY_REFUSED
            }
            ErrorCode::QuestionsWideningRefused | ErrorCode::PermissionGrantLimit => {
                exit::POLICY_REFUSED
            }
            _ => exit::FAILURE,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::error::Error for ErrorCode {}

error_codes! {
    Never:
        AgentHereUnavailable = "agent_here_unavailable",
        AgentNameTaken = "agent_name_taken",
        AgentNoActiveWorkspace = "agent_no_active_workspace",
        AgentNotFound = "agent_not_found",
        AgentPlacementConflict = "agent_placement_conflict",
        AgentPlacementNotFound = "agent_placement_not_found",
        AgentPlacementRequired = "agent_placement_required",
        AgentProfileUnresolved = "agent_profile_unresolved",
        AgentSpawnDisabled = "agent_spawn_disabled",
        AgentTargetAmbiguous = "agent_target_ambiguous",
        AlreadyHibernated = "already_hibernated",
        AmbiguousWorktreeBranch = "ambiguous_worktree_branch",
        AtLineageDepth = "at_lineage_depth",
        ChannelNotOwner = "channel_not_owner",
        ChannelUnauthorized = "channel_unauthorized",
        CheckNotFound = "check_not_found",
        CursorAhead = "cursor_ahead",
        DirtyWorktreeRequiresForce = "dirty_worktree_requires_force",
        GoalAlreadyArmed = "goal_already_armed",
        GoalCallerRefused = "goal_caller_refused",
        GoalNotArmed = "goal_not_armed",
        GoalNotPaused = "goal_not_paused",
        HandoffNotFound = "handoff_not_found",
        HandoffTooLarge = "handoff_too_large",
        HibernateUnsupported = "hibernate_unsupported",
        HopBudgetExhausted = "hop_budget_exhausted",
        IdentityPinConflict = "identity_pin_conflict",
        IdentityUnknown = "identity_unknown",
        InvalidAgent = "invalid_agent",
        InvalidAgentArgv = "invalid_agent_argv",
        InvalidAgentName = "invalid_agent_name",
        InvalidCustodyState = "invalid_custody_state",
        InvalidEnvelope = "invalid_envelope",
        InvalidHeaderField = "invalid_header_field",
        InvalidKey = "invalid_key",
        InvalidMetadataRequest = "invalid_metadata_request",
        InvalidParams = "invalid_params",
        InvalidRegex = "invalid_regex",
        InvalidRequest = "invalid_request",
        InvalidSessionName = "invalid_session_name",
        InvalidSignature = "invalid_signature",
        InvalidStateLabel = "invalid_state_label",
        LineageNotFound = "lineage_not_found",
        LinkedWorktreeSource = "linked_worktree_source",
        LoopDetected = "loop_detected",
        MeshCollectionRefused = "mesh_collection_refused",
        MeshRefused = "mesh_refused",
        MessageExpired = "message_expired",
        MessageKeyConflict = "message_key_conflict",
        MessageNotFound = "message_not_found",
        MsgNotAllowed = "msg_not_allowed",
        MsgTargetAmbiguous = "msg_target_ambiguous",
        MsgTargetNotFound = "msg_target_not_found",
        NoAgentSession = "no_agent_session",
        NoBranch = "no_branch",
        NoCallerPane = "no_caller_pane",
        NoCheckout = "no_checkout",
        NotAnEdge = "not_an_edge",
        NotFound = "not_found",
        NotFromAPane = "not_from_a_pane",
        NotGitWorktree = "not_git_worktree",
        NotHibernated = "not_hibernated",
        NotImplemented = "not_implemented",
        NotLinkedWorktree = "not_linked_worktree",
        OperatorOnly = "operator_only",
        NotAuthorized = "not_authorized",
        PermissionGrantLimit = "permission_grant_limit",
        PermissionInputAlreadyHeld = "permission_input_already_held",
        PermissionRequestTooLarge = "permission_request_too_large",
        PermissionRequestUnknown = "permission_request_unknown",
        PermissionRequestClosed = "permission_request_closed",
        // Retained for compatibility with runtimes predating session grants.
        PermissionDecisionUnsupported = "permission_decision_unsupported",
        OriginMismatch = "origin_mismatch",
        OwnerPolicyChanged = "owner_policy_changed",
        OwnerPolicyUnreadable = "owner_policy_unreadable",
        PaneGone = "pane_gone",
        PaneNotFound = "pane_not_found",
        Parked = "parked",
        PromptControlBytes = "prompt_control_bytes",
        PromptEmpty = "prompt_empty",
        PromptTooLong = "prompt_too_long",
        QuestionsWideningRefused = "questions_widening_refused",
        RecipientGone = "recipient_gone",
        RelayNotFromSsh = "relay_not_from_ssh",
        RelayUnattestable = "relay_unattestable",
        ReplyUnavailable = "reply_unavailable",
        RestartDisabled = "restart_disabled",
        RestartUnsupported = "restart_unsupported",
        SelfCompactInstructionsUnsupported = "self_compact_instructions_unsupported",
        SelfCompactUnavailable = "self_compact_unavailable",
        SelfKillUnconfirmed = "self_kill_unconfirmed",
        SelfMessageForbidden = "self_message_forbidden",
        SenderUnresolved = "sender_unresolved",
        SpawnCallerUnknown = "spawn_caller_unknown",
        StashEmpty = "stash_empty",
        StructuredMcpLeaseRequired = "structured_mcp_lease_required",
        SubmoduleWorktreeRequiresForce = "submodule_worktree_requires_force",
        TabNotFound = "tab_not_found",
        TargetPaneNotFound = "target_pane_not_found",
        TooManyHeaderFields = "too_many_header_fields",
        TranscriptNotFound = "transcript_not_found",
        UnknownAgentKind = "unknown_agent_kind",
        UnsafeText = "unsafe_text",
        UnsupportedForAgent = "unsupported_for_agent",
        UnsupportedInAppMode = "unsupported_in_app_mode",
        WaitDeadlineTooLong = "wait_deadline_too_long",
        WaitPredicateInvalid = "wait_predicate_invalid",
        WaitPredicateUnsupported = "wait_predicate_unsupported",
        WorkspaceNotFound = "workspace_not_found",
        WorktreeNotFound = "worktree_not_found";
    Maybe:
        AgentForkFailed = "agent_fork_failed",
        AgentStartFailed = "agent_start_failed",
        CheckoutPrepareFailed = "checkout_prepare_failed",
        DelegateUnreachable = "delegate_unreachable",
        DigestWriteFailed = "digest_write_failed",
        HandoffFailed = "handoff_failed",
        HandoffUnreadable = "handoff_unreadable",
        IntegrationInstallFailed = "integration_install_failed",
        IntegrationUninstallFailed = "integration_uninstall_failed",
        InternalError = "internal_error",
        MailStoreUnavailable = "mail_store_unavailable",
        MeshDeliveryRefused = "mesh_delivery_refused",
        MeshStoreUnavailable = "mesh_store_unavailable",
        PaneMoveFailed = "pane_move_failed",
        PaneSendFailed = "pane_send_failed",
        PaneSplitFailed = "pane_split_failed",
        PausePersistFailed = "pause_persist_failed",
        SerializationError = "serialization_error",
        ServerUnavailable = "server_unavailable",
        SessionBroken = "session_broken",
        SessionDeleteFailed = "session_delete_failed",
        SessionStopFailed = "session_stop_failed",
        SpawnFailed = "spawn_failed",
        SpawnLocationUnresolved = "spawn_location_unresolved",
        TabCloseFailed = "tab_close_failed",
        TabCreateFailed = "tab_create_failed",
        TranscriptUnreadable = "transcript_unreadable",
        Transport = "transport",
        WorkspaceCreateFailed = "workspace_create_failed",
        WorktreeCreateFailed = "worktree_create_failed",
        WorktreeListFailed = "worktree_list_failed",
        WorktreeOpenFailed = "worktree_open_failed",
        WorktreePruneFailed = "worktree_prune_failed",
        WorktreeRemoveFailed = "worktree_remove_failed",
        WriteFailed = "write_failed";
    Yes:
        Timeout = "timeout";
    Later:
        SessionBusy = "session_busy",
        InputWriteCancelled = "input_write_cancelled",
        AgentNotReady = "agent_not_ready",
        AtAgentCapacity = "at_agent_capacity",
        AtFanoutLimit = "at_fanout_limit",
        ChannelTokenUnavailable = "channel_token_unavailable",
        FleetPaused = "fleet_paused",
        HandoffUndrained = "handoff_undrained",
        InjectionPending = "injection_pending",
        InputNotEmpty = "input_not_empty",
        MailStoreFull = "mail_store_full",
        MailboxFull = "mailbox_full",
        MeshNotEnrolled = "mesh_not_enrolled",
        MsgRateLimited = "msg_rate_limited",
        NoLocalServer = "no_local_server",
        NotSettled = "not_settled",
        OperatorActive = "operator_active",
        RestartBusy = "restart_busy",
        RestartPending = "restart_pending",
        RestartRateLimited = "restart_rate_limited",
        SessionUnconfirmed = "session_unconfirmed",
        TooManyWaits = "too_many_waits",
        UnknownComposer = "unknown_composer",
        WorktreeMergeUnknown = "worktree_merge_unknown",
        WorktreeOpenPr = "worktree_open_pr";
    AfterInspecting:
        InputWriteFailed = "input_write_failed",
        InputWriteUnknown = "input_write_unknown",
        AgentExitedAtStart = "agent_exited_at_start",
        AgentExitedBeforeReady = "agent_exited_before_ready",
        NoResult = "no_result",
        SelfSubmitUnconfirmed = "self_submit_unconfirmed",
        SessionMismatch = "session_mismatch";
    AfterHuman:
        WorktreeCheckoutUnreachable = "worktree_checkout_unreachable",
        WorktreeRepositoryUnreachable = "worktree_repository_unreachable",
        StructuredWorkerPanicked = "structured_worker_panicked",
        StructuredWorkerTimeout = "structured_worker_timeout",
        StructuredWorkerCleanupFailed = "structured_worker_cleanup_failed",
        ClaudeFolderTrust = "claude_folder_trust",
        CodexFolderTrust = "codex_folder_trust",
        CodexHookReview = "codex_hook_review",
        StructuredHarnessVersionUnsupported = "structured_harness_version_unsupported",
        StructuredNeedsAgentUser = "structured_needs_agent_user";
    AfterHandoff:
        MailStoreSchemaTooNew = "mail_store_schema_too_new",
        MeshVersionMismatch = "mesh_version_mismatch",
        PeerIncompatible = "peer_incompatible",
        ServerVersionGap = "server_version_gap";
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    #[test]
    fn unreachable_checkout_requires_recovery_not_force() {
        for code in [
            ErrorCode::WorktreeCheckoutUnreachable,
            ErrorCode::WorktreeRepositoryUnreachable,
        ] {
            assert_eq!(code.retry().as_str(), "after_human");
            assert_eq!(code.exit(), crate::cli::exit::REFUSED);
            assert_eq!(
                crate::cli::error_code_exit(code.as_str(), false),
                crate::cli::exit::REFUSED
            );
        }
    }

    #[test]
    fn parked_restart_requires_a_different_call_and_has_a_usage_exit() {
        let code = ErrorCode::from_code("parked").expect("parked is a registered refusal");
        assert_eq!(code.retry(), crate::cli::exit::Retry::Never);
        assert_eq!(code.exit(), crate::cli::exit::USAGE);
        assert_eq!(
            crate::cli::error_code_exit("parked", false),
            crate::cli::exit::USAGE
        );
        assert_eq!(
            crate::cli::error_code_exit("parked", true),
            crate::cli::exit::USAGE
        );
    }

    /// One line per retry class, codes sorted. Adding a code changes its line,
    /// and so does renaming, reclassifying or dropping one.
    #[test]
    fn every_registered_error_code_and_its_retry_class() {
        let classes = [
            "never",
            "maybe",
            "yes",
            "later",
            "after_inspecting",
            "after_human",
            "after_handoff",
        ];
        let actual: Vec<(&str, String)> = classes
            .iter()
            .map(|class| {
                let mut codes: Vec<&str> = ErrorCode::ALL
                    .iter()
                    .filter(|c| c.retry().as_str() == *class)
                    .map(|c| c.as_str())
                    .collect();
                codes.sort_unstable();
                (*class, codes.join(" "))
            })
            .collect();
        let expected: Vec<(&str, String)> = [
            ("never", "agent_here_unavailable agent_name_taken agent_no_active_workspace agent_not_found agent_placement_conflict agent_placement_not_found agent_placement_required agent_profile_unresolved agent_spawn_disabled agent_target_ambiguous already_hibernated ambiguous_worktree_branch at_lineage_depth channel_not_owner channel_unauthorized check_not_found cursor_ahead dirty_worktree_requires_force goal_already_armed goal_caller_refused goal_not_armed goal_not_paused handoff_not_found handoff_too_large hibernate_unsupported hop_budget_exhausted identity_pin_conflict identity_unknown invalid_agent invalid_agent_argv invalid_agent_name invalid_custody_state invalid_envelope invalid_header_field invalid_key invalid_metadata_request invalid_params invalid_regex invalid_request invalid_session_name invalid_signature invalid_state_label lineage_not_found linked_worktree_source loop_detected mesh_collection_refused mesh_refused message_expired message_key_conflict message_not_found msg_not_allowed msg_target_ambiguous msg_target_not_found no_agent_session no_branch no_caller_pane no_checkout not_an_edge not_authorized not_found not_from_a_pane not_git_worktree not_hibernated not_implemented not_linked_worktree operator_only origin_mismatch owner_policy_changed owner_policy_unreadable pane_gone pane_not_found parked permission_decision_unsupported permission_grant_limit permission_input_already_held permission_request_closed permission_request_too_large permission_request_unknown prompt_control_bytes prompt_empty prompt_too_long questions_widening_refused recipient_gone relay_not_from_ssh relay_unattestable reply_unavailable restart_disabled restart_unsupported self_compact_instructions_unsupported self_compact_unavailable self_kill_unconfirmed self_message_forbidden sender_unresolved spawn_caller_unknown stash_empty structured_mcp_lease_required submodule_worktree_requires_force tab_not_found target_pane_not_found too_many_header_fields transcript_not_found unknown_agent_kind unsafe_text unsupported_for_agent unsupported_in_app_mode wait_deadline_too_long wait_predicate_invalid wait_predicate_unsupported workspace_not_found worktree_not_found"),

            ("maybe", "agent_fork_failed agent_start_failed checkout_prepare_failed delegate_unreachable digest_write_failed handoff_failed handoff_unreadable integration_install_failed integration_uninstall_failed internal_error mail_store_unavailable mesh_delivery_refused mesh_store_unavailable pane_move_failed pane_send_failed pane_split_failed pause_persist_failed serialization_error server_unavailable session_broken session_delete_failed session_stop_failed spawn_failed spawn_location_unresolved tab_close_failed tab_create_failed transcript_unreadable transport workspace_create_failed worktree_create_failed worktree_list_failed worktree_open_failed worktree_prune_failed worktree_remove_failed write_failed"),
            ("yes", "timeout"),
            ("later", "agent_not_ready at_agent_capacity at_fanout_limit channel_token_unavailable fleet_paused handoff_undrained injection_pending input_not_empty input_write_cancelled mail_store_full mailbox_full mesh_not_enrolled msg_rate_limited no_local_server not_settled operator_active restart_busy restart_pending restart_rate_limited session_busy session_unconfirmed too_many_waits unknown_composer worktree_merge_unknown worktree_open_pr"),
            ("after_inspecting", "agent_exited_at_start agent_exited_before_ready input_write_failed input_write_unknown no_result self_submit_unconfirmed session_mismatch"),
            ("after_human", "claude_folder_trust codex_folder_trust codex_hook_review structured_harness_version_unsupported structured_needs_agent_user structured_worker_cleanup_failed structured_worker_panicked structured_worker_timeout worktree_checkout_unreachable worktree_repository_unreachable"),
            ("after_handoff", "mail_store_schema_too_new mesh_version_mismatch peer_incompatible server_version_gap"),
        ]
        .into_iter()
        .map(|(class, codes)| (class, codes.to_string()))
        .collect();
        assert_eq!(actual, expected);
        // Count the independent expected table so concurrent additions cannot
        // silently overwrite each other's aggregate count during a merge.
        let expected_count: usize = expected
            .iter()
            .map(|(_, codes)| codes.split_whitespace().count())
            .sum();
        assert_eq!(ErrorCode::ALL.len(), expected_count);
    }

    #[test]
    fn registry_exits_match_protocol_policy_and_input_write_contract() {
        for code in ErrorCode::ALL {
            let want = match code {
                ErrorCode::InputWriteCancelled
                | ErrorCode::WorktreeCheckoutUnreachable
                | ErrorCode::WorktreeRepositoryUnreachable => 10,
                ErrorCode::InputWriteFailed | ErrorCode::InputWriteUnknown => 8,
                ErrorCode::ServerVersionGap => 78,
                ErrorCode::NotAuthorized
                | ErrorCode::OperatorOnly
                | ErrorCode::GoalCallerRefused
                | ErrorCode::QuestionsWideningRefused
                | ErrorCode::PermissionGrantLimit => 13,
                ErrorCode::StructuredMcpLeaseRequired
                | ErrorCode::Parked
                | ErrorCode::WaitPredicateInvalid
                | ErrorCode::WaitPredicateUnsupported
                | ErrorCode::WaitDeadlineTooLong => 2,
                _ => 1,
            };
            assert_eq!(code.exit(), want, "{code:?}");
        }
        assert_eq!(
            ErrorCode::from_code(crate::api::compatibility::ERROR_CODE),
            Some(ErrorCode::ServerVersionGap)
        );
    }

    /// An error code written as a literal where it is emitted must be
    /// registered. The forms, each matched across line breaks: the first or
    /// second argument of a code-taking call (`encode_error`, `ApiFailure::new`,
    /// `refuse` and the rest of `EMITTERS`), a `code:` field or JSON `"code":`
    /// key, an `Err(("code", …))` tuple, and a value (`=> "code"` or
    /// `{ "code" }`) inside a `fn code()` body or a `let code = …` statement.
    #[test]
    fn no_unregistered_error_code_literal_in_src() {
        const EMITTERS: &str = "encode_error|encode_error_with_data|ApiFailure::new|refuse|\
                                error_response_json|write_relay_error|print_session_error";
        let direct = [
            format!(r#"\b(?:{EMITTERS})\(\s*(?:[^,()"]+(?:\([^()]*\))?,\s*)?"([a-z0-9_]+)""#),
            r#"(?:\bcode|"code")\s*:\s*"([a-z0-9_]+)""#.to_string(),
            r#"\bErr\(\(\s*"([a-z0-9_]+)""#.to_string(),
        ]
        .map(|pattern| regex::Regex::new(&pattern).unwrap());
        let derived =
            regex::Regex::new(r"fn code\([^)]*\)\s*->\s*&(?:'static )?str|let code\s*=").unwrap();
        let value = regex::Regex::new(r#"=>\s*"([a-z0-9_]+)"|\{\s*"([a-z0-9_]+)"\s*\}"#).unwrap();
        let mut unregistered = Vec::new();
        for (path, text) in crate::cli::exit::tests::production_sources() {
            let mut found: Vec<String> = direct
                .iter()
                .flat_map(|emit| emit.captures_iter(&text))
                .map(|m| m[1].to_string())
                .collect();
            for start in derived.find_iter(&text) {
                let span = &text[start.end()..body_end(&text, start.end())];
                found.extend(
                    value
                        .captures_iter(span)
                        .map(|m| m.get(1).or(m.get(2)).unwrap().as_str().to_string()),
                );
            }
            for code in found {
                if ErrorCode::from_code(&code).is_none() {
                    unregistered.push(format!("{path}: {code}"));
                }
            }
        }
        unregistered.dedup();
        assert!(
            unregistered.is_empty(),
            "register these in src/api/error_code.rs: {unregistered:#?}"
        );
    }

    /// Where the `{ … }` body or the `… ;` statement starting at `from` ends.
    fn body_end(text: &str, from: usize) -> usize {
        let is_body = text[from..].trim_start().starts_with('{');
        let mut depth = 0usize;
        for (offset, ch) in text[from..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' if depth <= 1 && is_body => return from + offset + 1,
                '}' => depth = depth.saturating_sub(1),
                ';' if depth == 0 => return from + offset + 1,
                _ => {}
            }
        }
        text.len()
    }

    #[test]
    fn s3a_write_errors_have_explicit_exits_and_retry_classes() {
        for (name, exit, retry) in [
            ("input_write_cancelled", 10, "later"),
            ("input_write_failed", 8, "after_inspecting"),
            ("input_write_unknown", 8, "after_inspecting"),
        ] {
            let code = ErrorCode::from_code(name).expect("write result must be registered");
            assert_eq!(code.exit(), exit);
            assert_eq!(code.retry().as_str(), retry);
            for send_like in [false, true] {
                assert_eq!(crate::cli::error_code_exit(name, send_like), exit);
            }
        }
    }
}
