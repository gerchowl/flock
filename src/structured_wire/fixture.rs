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
    /// Begin an attempt; starts the connect deadline.
    Connect,
    /// Complete upgrade without compression; starts the handshake deadline.
    Open,
    /// Server sends one typed frame on the current socket.
    Send {
        /// Frame to deliver; abandoned sockets cannot deliver it.
        frame: ServerFrame,
    },
    /// Expect the next client frame, matching t plus the named top-level fields.
    Expect {
        /// Typed example; unselected fields are ignored by the matcher.
        frame: ClientFrame,
        /// Selected fields, excluding t which is always compared.
        fields: Vec<String>,
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
    /// Parse and validate a line; failures include a visible reason, never a fallback.
    pub fn parse(line: &str) -> Result<Self, String> {
        let raw: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let parsed: Self = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        let emitted = serde_json::to_value(&parsed).map_err(|e| e.to_string())?;
        // Serde's flattened unit variants otherwise accept stray script fields.
        for (input, output) in [
            (Some(&raw), Some(&emitted)),
            (raw.get("stage"), emitted.get("stage")),
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
            FixtureStep::Expect { frame, fields } => {
                let value = serde_json::to_value(frame).map_err(|e| e.to_string())?;
                if fields
                    .iter()
                    .any(|field| field == "t" || value.get(field).is_none())
                {
                    return Err("expect selects a missing field or repeats implicit t".into());
                }
            }
            FixtureStep::Send {
                frame: ServerFrame::Ev {
                    stream, seq, ev, ..
                },
            } => {
                let live = stream.starts_with("agent/") && stream.ends_with("/live");
                if live == seq.is_some() {
                    return Err("seq required except on agent live stream".into());
                }
                if stream.starts_with("agent/") && matches!(ev, WireEvent::Opaque(_)) {
                    return Err("agent event requires known SessionEvent or unknown wrapper".into());
                }
            }
            FixtureStep::Send {
                frame:
                    ServerFrame::Ack {
                        ok, result, error, ..
                    },
            } if (*ok && error.is_some()) || (!ok && (error.is_none() || result.is_some())) => {
                return Err("ack must carry success/result or failure/error exclusively".into());
            }
            _ => {}
        }
        Ok(parsed)
    }
}
