//! Versioned JSON Lines scripts for G3 (#1595), not a server implementation.
//!
//! Every line has v:1 and exactly one step. `expect` supplies a typed example
//! and the top-level fields to compare, always including t. Omitted match fields
//! are ignored; unknown or missing selected fields are invalid. Time advances
//! only through explicit virtual deadlines/faults, never wall-clock sleeps.
//! A runner must fail a missing expectation at its bounded virtual deadline and
//! fail immediately on mismatch, peer exit or unexpected output (ADR-0037).
//! See web/client/test/fixtures/wire/README.md for stage ordering and limits.

use super::*;
use std::collections::BTreeMap;

/// One versioned script line. Additive transport fields are allowed, script typos aren't.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FixtureLine {
    /// Fixture schema version, currently one (not WIRE_JSON_VERSION).
    pub v: u8,
    /// Script operation.
    #[serde(flatten)]
    pub step: FixtureStep,
}

/// Script operations; their effects are supplied by G3's virtual transport/clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureStep {
    /// Initial durable cursors and queued commands; required once, first.
    Given {
        /// Last applied cursor per stream, never a diagnostic skipped_to.
        cursors: BTreeMap<String, AppliedCursor>,
        /// Pending commands restored before any connection attempt.
        outbox: Vec<QueuedCommand>,
    },
    /// Explicit caller input, separate from expected transport output.
    Client {
        /// Operation requested by the caller.
        action: ClientAction,
    },
    /// Inspect client state; G3 owns the state/status vocabularies.
    Observe {
        /// Optional ADR-0038 connection-state assertion.
        #[serde(default)]
        connection: Option<String>,
        /// Assert only the named subscription entries.
        #[serde(default)]
        streams: BTreeMap<String, StreamObservation>,
        /// Assert only the named command statuses.
        #[serde(default)]
        outbox: BTreeMap<String, String>,
    },
    /// Require no client frames or connection attempts before the virtual ceiling.
    Quiet {
        /// Positive duration; JSON number, exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        ceiling_ms: u64,
    },
    /// Expect an attempt to start within the virtual backoff ceiling; no stimulus.
    ExpectConnect {
        /// Positive duration; JSON number, exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        within_ms: u64,
    },
    /// Complete upgrade without compression; starts the handshake deadline.
    Open,
    /// Server sends one typed frame on the current socket.
    Send {
        /// Current attempt or the most recently abandoned socket generation.
        #[serde(default)]
        socket: SocketTarget,
        /// Frame to inject; previous-socket callbacks must be ignored.
        frame: ServerFrame,
    },
    /// Expect the next client frame, matching t plus the named top-level fields.
    Expect {
        /// Typed example; unselected fields are ignored by the matcher.
        frame: ClientFrame,
        /// Selected fields, excluding t which is always compared.
        fields: Vec<String>,
        /// Optional fields that must be absent, rather than ignored.
        #[serde(default)]
        absent: Vec<String>,
    },
    /// Expire exactly the named stage's virtual failure ceiling.
    Deadline {
        /// No duration override, sleep or automatic success exists.
        stage: DeadlineStage,
    },
    /// Withhold all valid server traffic for the liveness window after welcome.
    HeartbeatMissed,
    /// Receive an explicit WebSocket close with its visible reason.
    Close {
        /// WS status, including normal/protocol failure statuses.
        code: u16,
        /// Human diagnosis, never a bearer value.
        reason: String,
    },
    /// Lose the socket without a close frame.
    Drop,
    /// Authentication refusal mapped to 4401 or mid-session revocation to 4403.
    TokenRefused {
        /// Only the two auth close codes are valid here.
        code: u16,
    },
}

/// JSON cursor; exact in JavaScript below 2^53.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct AppliedCursor(
    /// Last applied sequence; JSON number, exact in JavaScript below 2^53.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub u64,
);

/// Pending outbox entry restored by given; no command is inferred from expect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct QueuedCommand {
    /// Stable command identity.
    pub cmd_id: String,
    /// Socket API operation.
    pub verb: String,
    /// Original command arguments.
    pub args: Value,
    /// Principal whose retention window applies.
    pub principal: String,
    /// Virtual creation timestamp; JSON number, exact in JavaScript below 2^53.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_ms: u64,
}

/// Subscription observation; string state vocabulary belongs to G3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct StreamObservation {
    /// Expected reducer state.
    pub state: String,
    /// Last applied cursor, null before any application (zero is meaningful).
    pub cursor: Option<AppliedCursor>,
}

/// Socket generation on which the fake server delivers a frame.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SocketTarget {
    /// Active attempt.
    #[default]
    Current,
    /// Most recently abandoned attempt; must not change any client state.
    Previous,
}

/// Caller stimuli; supplied subscription identities are deterministic runner inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientAction {
    /// Ask to connect; begins the initial attempt.
    Connect,
    /// Ask to subscribe; cursor comes from applied state, not the expectation.
    Subscribe {
        /// Deterministic subscription identity supplied by the caller.
        sub_id: String,
        /// Stream requested by the caller.
        stream: String,
    },
    /// Caller explicitly ends a subscription.
    Unsubscribe {
        /// Subscription to remove.
        sub_id: String,
    },
    /// Queue a new command; resume_only is computed by the client.
    Command {
        /// Stable command identity.
        cmd_id: String,
        /// Existing socket API operation.
        verb: String,
        /// Original arguments.
        args: Value,
    },
}

/// Independently bounded stages from ADR-0036 §5a, driven by the fake clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeadlineStage {
    /// Advance 10 seconds from attempt start without open.
    Connect,
    /// Advance 5 seconds from open without welcome.
    Handshake,
    /// Advance 30 seconds without progress on the selected subscription.
    Subscription {
        /// Stream whose progress ceased; unrelated traffic cannot reset it.
        sub_id: String,
    },
    /// Advance 30 seconds from send without its ack.
    CommandAck {
        /// Sent command whose outcome becomes uncertain, without reconnecting.
        cmd_id: String,
    },
}

impl FixtureLine {
    /// Validate a complete script, including mandatory initial state and final silence.
    pub fn parse_script(script: &str) -> Result<Vec<Self>, String> {
        let lines: Vec<Self> = script.lines().map(Self::parse).collect::<Result<_, _>>()?;
        if !matches!(
            lines.first().map(|line| &line.step),
            Some(FixtureStep::Given { .. })
        ) || !matches!(
            lines.last().map(|line| &line.step),
            Some(FixtureStep::Quiet { .. })
        ) || lines
            .iter()
            .skip(1)
            .any(|line| matches!(line.step, FixtureStep::Given { .. }))
        {
            return Err("script requires exactly one given first and quiet last".into());
        }
        Ok(lines)
    }

    /// Parse and validate a line; failures include a visible reason, never a fallback.
    pub fn parse(line: &str) -> Result<Self, String> {
        let raw: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let parsed: Self = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        let emitted = serde_json::to_value(&parsed).map_err(|e| e.to_string())?;
        // Serde's flattened unit variants otherwise accept stray script fields.
        for (input, output) in [
            (Some(&raw), Some(&emitted)),
            (raw.get("stage"), emitted.get("stage")),
            (raw.get("action"), emitted.get("action")),
        ] {
            if let (Some(Value::Object(input)), Some(Value::Object(output))) = (input, output) {
                if input.keys().any(|key| !output.contains_key(key)) {
                    return Err("unknown script field".into());
                }
            }
        }
        if parsed.v != 1 {
            return Err("unsupported fixture version".into());
        }
        match &parsed.step {
            FixtureStep::TokenRefused { code }
                if ![AUTH_REFUSED_CLOSE, AUTH_REVOKED_CLOSE].contains(code) =>
            {
                return Err("token_refused requires 4401 or 4403".into())
            }
            FixtureStep::Expect {
                frame,
                fields,
                absent,
            } => {
                let value = serde_json::to_value(frame).map_err(|e| e.to_string())?;
                if fields
                    .iter()
                    .any(|field| field == "t" || value.get(field).is_none())
                {
                    return Err("expect selects a missing field or repeats implicit t".into());
                }
                let optional = match frame {
                    ClientFrame::Hello { .. } => "token",
                    ClientFrame::Sub { .. } => "after",
                    _ => "",
                };
                if absent.iter().any(|field| {
                    field != optional || value.get(field).is_some() || fields.contains(field)
                }) || absent.len() > 1
                {
                    return Err("absent must name an omitted optional field".into());
                }
            }
            FixtureStep::Close { code, .. }
                if [AUTH_REFUSED_CLOSE, AUTH_REVOKED_CLOSE].contains(code) =>
            {
                return Err("authentication closes require token_refused".into());
            }
            FixtureStep::Quiet { ceiling_ms: 0 } | FixtureStep::ExpectConnect { within_ms: 0 } => {
                return Err("virtual ceiling must be positive".into());
            }
            FixtureStep::Send {
                frame: ServerFrame::Ev {
                    stream, seq, ev, ..
                },
                ..
            } => {
                let live = stream.starts_with("agent/") && stream.ends_with("/live");
                if live == seq.is_some() {
                    return Err("seq required except on agent live stream".into());
                }
                if stream.starts_with("agent/") && matches!(ev, WireEvent::Json(_)) {
                    return Err("agent event requires known SessionEvent or unknown wrapper".into());
                }
            }
            FixtureStep::Send {
                frame:
                    ServerFrame::Ack {
                        ok, result, error, ..
                    },
                ..
            } if (*ok && error.is_some()) || (!ok && (error.is_none() || result.is_some())) => {
                return Err("ack must carry success/result or failure/error exclusively".into());
            }
            _ => {}
        }
        Ok(parsed)
    }
}
