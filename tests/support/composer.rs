//! Fixture-specific readiness, composed with the shared readiness waits.
//! Subscribe to exits before allocation so even a pre-connect death is observed.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use super::readiness::{read_socket_line, Stages};
use serde_json::{json, Value};

fn invalid(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason.into())
}

/// Exit monitoring and one failure ceiling for a gated fixture.
pub struct Composer {
    stages: Stages,
    exits: UnixStream,
}

impl Composer {
    /// Install the exit subscription BEFORE requesting workspace creation.
    /// The fixture must block on its gate before emitting READY.
    pub fn watch(socket: &Path) -> io::Result<Self> {
        let stages = Stages::new(Duration::from_secs(120));
        let exits = Self::subscribe(socket, json!([{"type":"pane.exited"}]), |stream| {
            stages.line(stream)
        })?;
        Ok(Self { stages, exits })
    }

    fn subscribe(
        socket: &Path,
        subscriptions: Value,
        read_ack: impl FnOnce(&mut UnixStream) -> io::Result<String>,
    ) -> io::Result<UnixStream> {
        let mut stream = UnixStream::connect(socket)?;
        writeln!(
            stream,
            "{}",
            json!({"id":"composer-ready", "method":"events.subscribe", "params":{"subscriptions":subscriptions}})
        )?;
        let ack: Value = serde_json::from_str(&read_ack(&mut stream)?)?;
        if ack["id"] != "composer-ready" || ack["result"]["type"] != "subscription_started" {
            return Err(invalid(format!(
                "unexpected composer subscription acknowledgement: {ack}"
            )));
        }
        Ok(stream)
    }

    fn readable(&mut self, fd: RawFd, pane: &str, phase: &str) -> io::Result<()> {
        loop {
            if self.stages.readable(&[self.exits.as_raw_fd(), fd])? == 1 {
                return Ok(());
            }
            let event: Value = serde_json::from_str(&self.stages.line(&mut self.exits)?)?;
            if event["event"] != "pane_exited" {
                return Err(invalid(format!("unexpected fixture exit event: {event}")));
            }
            if event["data"]["pane_id"] == pane {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("fixture exited before {phase}"),
                ));
            }
        }
    }

    fn line(&mut self, stream: &mut UnixStream, pane: &str, phase: &str) -> io::Result<String> {
        read_socket_line(stream, |fd| self.readable(fd, pane, phase)).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::new(error.kind(), format!("fixture exited before {phase}"))
            } else {
                error
            }
        })
    }

    /// Wait for boot, subscribe to READY and optional idle/done, then release.
    /// Exits are monitored during accept, boot and every readiness event read.
    pub fn wait(
        &mut self,
        gate: &UnixListener,
        socket: &Path,
        pane: &str,
        require_idle: bool,
    ) -> io::Result<()> {
        self.readable(gate.as_raw_fd(), pane, "connecting")?;
        let (mut producer, _) = gate.accept()?;
        let boot = self.line(&mut producer, pane, "boot")?;
        if boot != "boot" {
            return Err(invalid(format!("expected boot, got {boot}")));
        }
        let mut subscriptions = vec![json!({"type":"pane.output_matched", "pane_id":pane,
            "source":"recent", "lines":40, "match":{"type":"substring", "value":"READY"}})];
        if require_idle {
            for status in ["idle", "done"] {
                subscriptions.push(json!({"type":"pane.agent_status_changed", "pane_id":pane, "agent_status":status}));
            }
        }
        let mut events = Self::subscribe(socket, json!(subscriptions), |stream| {
            self.line(stream, pane, "readiness subscription")
        })?;
        producer.write_all(b"G")?;
        let mut state = ReadyState::new(require_idle);
        loop {
            let event: Value =
                serde_json::from_str(&self.line(&mut events, pane, "READY and idle/done")?)?;
            if state.observe(&event, pane)? {
                return Ok(());
            }
        }
    }
}

/// Idempotent evidence accumulation: presentation changes may repeat idle/done.
pub struct ReadyState {
    output_seen: bool,
    idle_seen: bool,
}

impl ReadyState {
    /// Plain paste fixtures need output only; submit fixtures also need idle/done.
    pub fn new(require_idle: bool) -> Self {
        Self {
            output_seen: false,
            idle_seen: !require_idle,
        }
    }

    /// Accept either event order and repeated evidence, rejecting unrelated data.
    pub fn observe(&mut self, event: &Value, pane: &str) -> io::Result<bool> {
        if event["data"]["pane_id"] != pane {
            return Err(invalid(format!("wrong composer pane: {event}")));
        }
        match event["event"].as_str() {
            Some("pane.output_matched")
                if event["data"]["read"]["text"]
                    .as_str()
                    .is_some_and(|s| s.contains("READY")) =>
            {
                self.output_seen = true
            }
            Some("pane.agent_status_changed")
                if matches!(
                    event["data"]["agent_status"].as_str(),
                    Some("idle" | "done")
                ) =>
            {
                self.idle_seen = true
            }
            _ => {
                return Err(invalid(format!(
                    "unexpected composer readiness event: {event}"
                )))
            }
        }
        Ok(self.output_seen && self.idle_seen)
    }
}
