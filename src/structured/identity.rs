//! Pure UID boundary, ADR-0036 §1a; ancestry supplies agent identity separately.
use crate::platform::peer;

/// Server-owned account configuration, independent of request claims.
pub(crate) struct UidPolicy {
    /// Effective UID of the server at construction.
    pub operator: u32,
    /// Dedicated account UID, absence, or a directory-service failure.
    pub dedicated: Result<Option<u32>, ()>,
}
/// Local socket UID classification. Unknown is never operator authority.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Principal {
    /// Operator UID, subject to independent agent ancestry checks.
    Operator,
    /// Dedicated agent UID, including callers without known ancestry.
    Agent,
    /// Missing, unrecognized, or unreadable credentials.
    Unknown,
}
impl Default for UidPolicy {
    fn default() -> Self {
        Self {
            operator: peer::effective_uid(),
            dedicated: peer::dedicated_uid().map_err(|error| {
                tracing::warn!("structured account lookup failed: {error}");
            }),
        }
    }
}
impl UidPolicy {
    /// Constrain the caller role by UID; agent identity still requires ancestry.
    pub fn classify(&self, uid: Option<u32>) -> Principal {
        let (Some(uid), Ok(dedicated)) = (uid, self.dedicated) else {
            return Principal::Unknown;
        };
        if dedicated == Some(uid) {
            Principal::Agent
        } else if uid == self.operator {
            Principal::Operator
        } else {
            Principal::Unknown
        }
    }
}

impl UidPolicy {
    /// ADR-0036 §1a: anonymous/unknown UIDs may read, but cannot bypass the
    /// boundary through raw terminal sends or other state-changing methods.
    pub fn refusal(
        &self,
        uid: Option<u32>,
        attributed: bool,
        method: &crate::api::schema::Method,
    ) -> Option<crate::api::error_code::ErrorCode> {
        use crate::api::error_code::ErrorCode;
        use crate::api::schema::Method::*;
        match self.classify(uid) {
            Principal::Operator => return None,
            Principal::Agent if attributed => return None,
            _ => (),
        }
        // is_retry_safe promises reads/previews only. Inbox consumption and
        // cursor/wait lifecycle are reads too, but intentionally not retried.
        if method.is_retry_safe()
            || matches!(
                method,
                ChannelHello(_)
                    | WaitPark(_)
                    | MsgRead(_)
                    | MsgWaitReply(_)
                    | EventsSubscribe(_)
                    | EventsWait(_)
                    | EventsRead(_)
                    | WaitHold(_, _)
                    | WaitRelease(_)
                    | PaneWaitForOutput(_)
                    | PaneStashShow(_)
            )
        {
            return None;
        }
        Some(
            if matches!(
                method,
                MsgSend(_)
                    | MsgReply(_)
                    | MsgWake(_)
                    | AgentSend(_)
                    | PaneSendText(_)
                    | PaneWriteText(_)
                    | PaneWriteKeys(_)
                    | PaneWriteInput(_)
                    | PaneSendInput(_)
                    | PaneSendKeys(_)
                    | PaneSubmit(_)
                    | PaneStashRestore(_)
            ) {
                ErrorCode::MsgNotAllowed
            } else {
                ErrorCode::OperatorOnly
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anonymous_policy_separates_reads_sends_and_other_mutations() {
        use crate::api::{error_code::ErrorCode, schema::*};
        let policy = UidPolicy {
            operator: 7,
            dedicated: Ok(Some(42)),
        };
        let read = Method::WorkspaceList(EmptyParams {});
        let stop = Method::ServerStop(EmptyParams {});
        for uid in [Some(42), Some(99), None] {
            assert_eq!(policy.refusal(uid, false, &read), None);
            assert_eq!(
                policy.refusal(uid, false, &stop),
                Some(ErrorCode::OperatorOnly)
            );
            for method in [
                "pane.send_text",
                "pane.write_text",
                "pane.send_keys",
                "pane.write_keys",
                "pane.send_input",
                "pane.write_input",
                "pane.submit",
            ] {
                let params = if method.ends_with("_keys") {
                    serde_json::json!({"pane_id": "fixture-pane", "keys": ["Enter"]})
                } else {
                    serde_json::json!({"pane_id": "fixture-pane", "text": "fixture"})
                };
                let request: Request = serde_json::from_value(serde_json::json!({
                    "id": "uid", "method": method,
                    "params": params
                }))
                .unwrap();
                assert_eq!(
                    policy.refusal(uid, false, &request.method),
                    Some(ErrorCode::MsgNotAllowed),
                    "{method}"
                );
            }
        }
        assert_eq!(policy.refusal(Some(7), false, &stop), None);
        assert_eq!(policy.refusal(Some(42), true, &stop), None);
        assert_eq!(
            policy.refusal(None, true, &stop),
            Some(ErrorCode::OperatorOnly)
        );
    }

    #[test]
    fn uid_boundary_never_promotes_anonymous_or_unknown_credentials() {
        let policy = UidPolicy {
            operator: 7,
            dedicated: Ok(Some(42)),
        };
        for (uid, expected) in [
            (Some(7), Principal::Operator),
            (Some(42), Principal::Agent),
            (Some(99), Principal::Unknown),
            (None, Principal::Unknown),
        ] {
            assert_eq!(policy.classify(uid), expected);
        }
        assert_eq!(
            UidPolicy {
                operator: 7,
                dedicated: Err(())
            }
            .classify(Some(7)),
            Principal::Unknown
        );
        assert_eq!(
            UidPolicy {
                operator: 7,
                dedicated: Ok(None)
            }
            .classify(Some(7)),
            Principal::Operator
        );
        assert_eq!(
            UidPolicy {
                operator: 7,
                dedicated: Ok(Some(7))
            }
            .classify(Some(7)),
            Principal::Agent
        );
    }
}
