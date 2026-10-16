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
        /// Durable resync markers keyed by stream, independent of sub identities.
        #[serde(default)]
        resync: BTreeMap<String, ResyncMarker>,
        /// Initial virtual clock, allowing expiry before a new welcome.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        now_ms: u64,
        /// Last known principal window, restored independently of a connection.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        idempotency_window_ms: Option<u64>,
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
        outbox: BTreeMap<String, OutboxStatus>,
        /// Stream entries whose keys must be absent, including null entries.
        #[serde(default)]
        absent_streams: Vec<String>,
        /// Commands whose keys must be absent (for example after principal change).
        #[serde(default)]
        absent_outbox: Vec<String>,
        /// Exact transitions since previous observe, including transient Stale.
        /// Every observe drains the trace, even if this selector is omitted.
        #[serde(default)]
        connection_transitions: Option<Vec<String>>,
    },
    /// Require no client frames or connection attempts before the virtual ceiling.
    Quiet {
        /// Positive duration; JSON number, exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        ceiling_ms: u64,
    },
    /// Expect an attempt to start within the virtual backoff ceiling; no stimulus.
    ExpectConnect {
        /// Inclusive lower bound, measured from this assertion's virtual instant.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        not_before_ms: u64,
        /// Positive duration; JSON number, exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        within_ms: u64,
    },
    /// Require the next client-initiated close, never inject a server close.
    ExpectClose {
        /// Expected WebSocket close status.
        code: u16,
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
    /// Expire exactly the named stage's original virtual ceiling. Fail any
    /// earlier state change, output, close or attempt, even if final state matches.
    /// The runner checks the entire intermediate trace, never only the endpoint.
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
    /// First send time, distinct from creation. Missing age after restart means
    /// unknown_outcome, never a new execution (ADR-0038 §3).
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub first_sent_ms: Option<u64>,
    /// Persisted client state determines first-send versus resume-only lookup.
    pub status: OutboxStatus,
}

/// Closed client outbox vocabulary (ADR-0038 §3), distinct from wire retry codes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    /// Never sent: first send uses resume_only false.
    Pending,
    /// Sent but not acknowledged: reconnect uses resume_only true.
    Sent,
    /// Ack ceiling elapsed on a live socket; no resend there, late ack can settle.
    Uncertain,
    /// Window expired or session_unconfirmed: never automatically execute again.
    UnknownOutcome,
    /// A successful acknowledgement settled the command.
    Acked,
    /// A terminal refusal settled the command.
    Failed,
}

/// Persisted snapshot progress. Re-subscription returns to Replaying at completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ResyncMarker {
    /// Sequence at which to remove still-unconfirmed entities and clear the marker.
    pub snapshot_to: AppliedCursor,
    /// Entity identifiers still awaiting confirmation by the snapshot.
    pub unconfirmed: Vec<String>,
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
    /// Close any socket, cancel retries and hold the outbox in Disconnected.
    Disconnect,
    /// Replace credentials; only exit from NeedsToken. Changed identity drops outbox.
    SetToken {
        /// Identity supplied by the fixture credential provider, not a bearer value.
        principal: String,
    },
    /// Immediately retry while Reconnecting, without escaping NeedsToken.
    NetworkChange,
    /// Immediately retry while Reconnecting when the client becomes foreground.
    Foreground,
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
            FixtureStep::Quiet { ceiling_ms: 0 }
            | FixtureStep::ExpectConnect { within_ms: 0, .. } => {
                return Err("virtual ceiling must be positive".into());
            }
            FixtureStep::ExpectConnect {
                not_before_ms,
                within_ms,
            } if not_before_ms > within_ms => {
                return Err("connection lower bound exceeds ceiling".into());
            }
            FixtureStep::Observe {
                streams,
                outbox,
                absent_streams,
                absent_outbox,
                ..
            } => {
                for (positive, absent) in [
                    (streams.keys().collect::<Vec<_>>(), absent_streams),
                    (outbox.keys().collect::<Vec<_>>(), absent_outbox),
                ] {
                    let mut seen = std::collections::BTreeSet::new();
                    if absent
                        .iter()
                        .any(|key| positive.contains(&key) || !seen.insert(key))
                    {
                        return Err("absent selections overlap or repeat".into());
                    }
                }
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
