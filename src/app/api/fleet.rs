//! `fleet.*` API handlers (#175 phase 5 / S3 commit 3, US-9).

use crate::api::schema::{EventData, EventEnvelope, EventKind, FleetPauseParams, ResponseResult};
use crate::app::fleet_pause::default_pause_path;
use crate::app::App;

use super::responses::{encode_error, encode_success};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

impl App {
    pub(super) fn handle_fleet_pause(&mut self, id: String, params: FleetPauseParams) -> String {
        let ts = now_ms();
        let reason = params.reason.unwrap_or_default();
        // Idempotent: pausing an already-paused fleet is a no-op success —
        // reason update goes through so an operator can retag a running
        // pause without a resume/repause cycle.
        let mut state = self.fleet_pause.clone();
        state.paused = true;
        if state.since_ms == 0 {
            state.since_ms = ts;
        }
        state.reason = reason.clone();
        if let Err(err) = state.save_to(&default_pause_path()) {
            return encode_error(
                id,
                "pause_persist_failed",
                format!("could not write pause.json: {err}"),
            );
        }
        self.fleet_pause = state.clone();
        App::sync_fleet_pause_banner(&self.fleet_pause, &mut self.state);
        self.event_hub.push(EventEnvelope {
            event: EventKind::FleetPaused,
            data: EventData::FleetPaused {
                since_ms: state.since_ms,
                reason: state.reason.clone(),
            },
        });
        encode_success(
            id,
            ResponseResult::FleetStatus {
                paused: state.paused,
                since_ms: state.since_ms,
                reason: state.reason,
            },
        )
    }

    pub(super) fn handle_fleet_resume(&mut self, id: String) -> String {
        let mut state = self.fleet_pause.clone();
        let was_paused = state.paused;
        state.paused = false;
        // Keep `since_ms` and `reason` on the record so the next status
        // read can still see the last window; overwritten on next pause.
        if let Err(err) = state.save_to(&default_pause_path()) {
            return encode_error(
                id,
                "pause_persist_failed",
                format!("could not write pause.json: {err}"),
            );
        }
        self.fleet_pause = state.clone();
        App::sync_fleet_pause_banner(&self.fleet_pause, &mut self.state);
        if was_paused {
            self.event_hub.push(EventEnvelope {
                event: EventKind::FleetResumed,
                data: EventData::FleetResumed {
                    resumed_ms: now_ms(),
                },
            });
        }
        encode_success(
            id,
            ResponseResult::FleetStatus {
                paused: state.paused,
                since_ms: state.since_ms,
                reason: state.reason,
            },
        )
    }

    pub(super) fn handle_fleet_status(&mut self, id: String) -> String {
        let state = self.fleet_pause.clone();
        encode_success(
            id,
            ResponseResult::FleetStatus {
                paused: state.paused,
                since_ms: state.since_ms,
                reason: state.reason,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_returns_epoch_millis_at_or_past_2020() {
        // 2020-01-01 = 1_577_836_800_000 ms. Sanity: this function returns
        // wall-clock, not zero, and we've been past 2020 for a while.
        assert!(now_ms() >= 1_577_836_800_000);
    }
}
