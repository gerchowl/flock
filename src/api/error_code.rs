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

// The error envelope (ADR-0029 C2) is the first production reader.
#[cfg_attr(not(test), allow(dead_code))]
impl ErrorCode {
    pub(crate) fn from_code(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == code)
    }

    /// The exit status a CLI call ending in this error returns today.
    pub(crate) const fn exit(self) -> i32 {
        match self {
            ErrorCode::ServerVersionGap => exit::PROTOCOL_GAP,
            _ => exit::FAILURE,
        }
    }
}

error_codes! {
    Never:
        AgentHereUnavailable = "agent_here_unavailable",
        AgentNameTaken = "agent_name_taken",
        AgentNoActiveWorkspace = "agent_no_active_workspace",
        AgentNotFound = "agent_not_found",
        AgentPlacementConflict = "agent_placement_conflict",
        AgentPlacementNotFound = "agent_placement_not_found",
        AgentPlacementRequired = "agent_placement_required",
        AgentTargetAmbiguous = "agent_target_ambiguous",
        CheckNotFound = "check_not_found",
        HandoffNotFound = "handoff_not_found",
        HandoffTooLarge = "handoff_too_large",
        IdentityUnknown = "identity_unknown",
        InvalidAgent = "invalid_agent",
        InvalidAgentArgv = "invalid_agent_argv",
        InvalidAgentName = "invalid_agent_name",
        InvalidHeaderField = "invalid_header_field",
        InvalidKey = "invalid_key",
        InvalidMetadataRequest = "invalid_metadata_request",
        InvalidParams = "invalid_params",
        InvalidRegex = "invalid_regex",
        InvalidRequest = "invalid_request",
        InvalidStateLabel = "invalid_state_label",
        LineageNotFound = "lineage_not_found",
        MeshCollectionRefused = "mesh_collection_refused",
        MeshRefused = "mesh_refused",
        MessageExpired = "message_expired",
        MessageNotFound = "message_not_found",
        MsgNotAllowed = "msg_not_allowed",
        MsgTargetNotFound = "msg_target_not_found",
        NoAgentSession = "no_agent_session",
        NoBranch = "no_branch",
        NoCheckout = "no_checkout",
        NotAnEdge = "not_an_edge",
        NotFromAPane = "not_from_a_pane",
        NotImplemented = "not_implemented",
        NotLinkedWorktree = "not_linked_worktree",
        OperatorOnly = "operator_only",
        PaneGone = "pane_gone",
        PaneNotFound = "pane_not_found",
        RecipientGone = "recipient_gone",
        RelayNotFromSsh = "relay_not_from_ssh",
        RelayUnattestable = "relay_unattestable",
        ReplyUnavailable = "reply_unavailable",
        SelfCompactInstructionsUnsupported = "self_compact_instructions_unsupported",
        SelfCompactUnavailable = "self_compact_unavailable",
        SelfMessageForbidden = "self_message_forbidden",
        SenderUnresolved = "sender_unresolved",
        SpawnCallerUnknown = "spawn_caller_unknown",
        StashEmpty = "stash_empty",
        TabNotFound = "tab_not_found",
        TargetPaneNotFound = "target_pane_not_found",
        TooManyHeaderFields = "too_many_header_fields",
        TranscriptNotFound = "transcript_not_found",
        UnknownAgentKind = "unknown_agent_kind",
        UnsafeText = "unsafe_text",
        UnsupportedForAgent = "unsupported_for_agent",
        UnsupportedInAppMode = "unsupported_in_app_mode",
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
        MeshStoreUnavailable = "mesh_store_unavailable",
        PaneMoveFailed = "pane_move_failed",
        PaneSendFailed = "pane_send_failed",
        PaneSplitFailed = "pane_split_failed",
        PausePersistFailed = "pause_persist_failed",
        SerializationError = "serialization_error",
        ServerUnavailable = "server_unavailable",
        SessionBroken = "session_broken",
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
        WriteFailed = "write_failed";
    Yes:
        Timeout = "timeout";
    Later:
        AgentNotReady = "agent_not_ready",
        FleetPaused = "fleet_paused",
        InjectionPending = "injection_pending",
        InputNotEmpty = "input_not_empty",
        MailStoreFull = "mail_store_full",
        MailboxFull = "mailbox_full",
        MeshNotEnrolled = "mesh_not_enrolled",
        MsgRateLimited = "msg_rate_limited",
        NotSettled = "not_settled",
        OperatorActive = "operator_active",
        RestartPending = "restart_pending",
        UnknownComposer = "unknown_composer";
    AfterInspecting:
        AgentExitedAtStart = "agent_exited_at_start",
        AgentExitedBeforeReady = "agent_exited_before_ready",
        NoResult = "no_result",
        SelfSubmitUnconfirmed = "self_submit_unconfirmed",
        SessionMismatch = "session_mismatch";
    AfterHuman:
        ClaudeFolderTrust = "claude_folder_trust",
        CodexFolderTrust = "codex_folder_trust",
        CodexHookReview = "codex_hook_review";
    AfterHandoff:
        MeshVersionMismatch = "mesh_version_mismatch",
        PeerIncompatible = "peer_incompatible",
        ServerVersionGap = "server_version_gap";
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

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
            ("never", "agent_here_unavailable agent_name_taken agent_no_active_workspace agent_not_found agent_placement_conflict agent_placement_not_found agent_placement_required agent_target_ambiguous check_not_found handoff_not_found handoff_too_large identity_unknown invalid_agent invalid_agent_argv invalid_agent_name invalid_header_field invalid_key invalid_metadata_request invalid_params invalid_regex invalid_request invalid_state_label lineage_not_found mesh_collection_refused mesh_refused message_expired message_not_found msg_not_allowed msg_target_not_found no_agent_session no_branch no_checkout not_an_edge not_from_a_pane not_implemented not_linked_worktree operator_only pane_gone pane_not_found recipient_gone relay_not_from_ssh relay_unattestable reply_unavailable self_compact_instructions_unsupported self_compact_unavailable self_message_forbidden sender_unresolved spawn_caller_unknown stash_empty tab_not_found target_pane_not_found too_many_header_fields transcript_not_found unknown_agent_kind unsafe_text unsupported_for_agent unsupported_in_app_mode workspace_not_found worktree_not_found"),
            ("maybe", "agent_fork_failed agent_start_failed checkout_prepare_failed delegate_unreachable digest_write_failed handoff_failed handoff_unreadable integration_install_failed integration_uninstall_failed internal_error mail_store_unavailable mesh_store_unavailable pane_move_failed pane_send_failed pane_split_failed pause_persist_failed serialization_error server_unavailable session_broken spawn_failed spawn_location_unresolved tab_close_failed tab_create_failed transcript_unreadable transport workspace_create_failed worktree_create_failed worktree_list_failed worktree_open_failed worktree_prune_failed write_failed"),
            ("yes", "timeout"),
            ("later", "agent_not_ready fleet_paused injection_pending input_not_empty mail_store_full mailbox_full mesh_not_enrolled msg_rate_limited not_settled operator_active restart_pending unknown_composer"),
            ("after_inspecting", "agent_exited_at_start agent_exited_before_ready no_result self_submit_unconfirmed session_mismatch"),
            ("after_human", "claude_folder_trust codex_folder_trust codex_hook_review"),
            ("after_handoff", "mesh_version_mismatch peer_incompatible server_version_gap"),
        ]
        .into_iter()
        .map(|(class, codes)| (class, codes.to_string()))
        .collect();
        assert_eq!(actual, expected);
        assert_eq!(ErrorCode::ALL.len(), 114);
    }

    #[test]
    fn the_version_gap_keeps_exit_78_and_every_other_code_exits_1() {
        for code in ErrorCode::ALL {
            let want = if *code == ErrorCode::ServerVersionGap {
                78
            } else {
                1
            };
            assert_eq!(code.exit(), want, "{code:?}");
        }
        assert_eq!(
            ErrorCode::from_code(crate::api::compatibility::ERROR_CODE),
            Some(ErrorCode::ServerVersionGap)
        );
    }

    /// An error code written as a literal at an emit site must be registered.
    #[test]
    fn no_unregistered_error_code_literal_in_src() {
        let emits = [
            regex::Regex::new(
                r#"encode_error(?:_with_data)?\(\s*[^,()]+(?:\([^()]*\))?,\s*"([a-z0-9_]+)""#,
            )
            .unwrap(),
            regex::Regex::new(r#"\bcode:\s*"([a-z0-9_]+)""#).unwrap(),
        ];
        let mut unregistered = Vec::new();
        for (path, text) in crate::cli::exit::tests::production_sources() {
            for emit in &emits {
                for m in emit.captures_iter(&text) {
                    if ErrorCode::from_code(&m[1]).is_none() {
                        unregistered.push(format!("{path}: {}", &m[1]));
                    }
                }
            }
        }
        assert!(
            unregistered.is_empty(),
            "register these in src/api/error_code.rs: {unregistered:#?}"
        );
    }
}
