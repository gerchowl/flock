use super::schema::{PollerHealth, PollerReason};
use serde::{Deserialize, Serialize};

// The pre-#1722 row shape is frozen here, including its open status string.
#[derive(Debug, Deserialize, Serialize)]
struct LegacyPollerHealth {
    status: String,
    last_success_age_secs: Option<u64>,
    consecutive_failures: u32,
    in_flight: bool,
    in_flight_age_secs: Option<u64>,
    skipped_rounds: u64,
    last_error: Option<String>,
}

#[test]
fn poller_idle_and_future_values_are_additive_for_old_and_new_readers() {
    for (status, reason, expected) in [
        ("idle", "no_viewer", PollerReason::NoViewer),
        ("future_status", "future_reason", PollerReason::Unknown),
    ] {
        let row = serde_json::json!({
            "status":status, "reason":reason, "last_success_age_secs":29_656,
            "consecutive_failures":0, "in_flight":false, "in_flight_age_secs":null,
            "skipped_rounds":0, "last_error":null
        });
        let current: PollerHealth = serde_json::from_value(row.clone()).unwrap();
        assert_eq!(current.status, status);
        assert_eq!(current.reason, Some(expected));
        let older: LegacyPollerHealth = serde_json::from_value(row.clone()).unwrap();
        assert_eq!(older.status, status);
        let mut without_reason = row;
        without_reason.as_object_mut().unwrap().remove("reason");
        assert_eq!(serde_json::to_value(&older).unwrap(), without_reason);
        let current_from_old: PollerHealth = serde_json::from_value(without_reason).unwrap();
        assert_eq!(current_from_old.reason, None);
    }
}
