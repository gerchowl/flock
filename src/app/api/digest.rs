//! `digest.render` handler (#175 phase 5 / S3 commit 2).
//!
//! Pure fold over the durable event log → self-contained HTML file. Cold
//! path (once a morning, or occasionally via the CLI); reads every rotated
//! log file, hands the result straight to `crate::digest::render`.

use crate::api::schema::{DigestRenderParams, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_digest_render(
        &mut self,
        id: String,
        params: DigestRenderParams,
    ) -> String {
        let events = self.event_hub.persisted_events_after(0);
        let since_ms = params.since_ms.unwrap_or(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let date = crate::digest::local_ymd_utc(now_ms);
        let since_label = if since_ms == 0 {
            "epoch".to_string()
        } else {
            format!("ts_ms>={since_ms}")
        };
        let opts = crate::digest::RenderOptions {
            since_ms,
            generated_for_date: &date,
            since_label: &since_label,
        };
        let html =
            crate::digest::render(events.iter().map(|(seq, ts, env)| (*seq, *ts, env)), &opts);

        // Path resolution:
        // 1. Explicit `path` wins. Absolute paths pass through; relative
        //    paths anchor at `data_dir()/digest/`.
        // 2. Otherwise `path_template` (default `{date}.html`) is expanded
        //    and anchored under `data_dir()/digest/`.
        let resolved = match params.path.as_deref() {
            Some(p) => {
                let p = std::path::PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    crate::session::data_dir().join("digest").join(p)
                }
            }
            None => {
                let template = params.path_template.as_deref().unwrap_or("{date}.html");
                let expanded = crate::digest::expand_path_template(template, &date);
                let p = std::path::PathBuf::from(&expanded);
                if p.is_absolute() {
                    p
                } else {
                    crate::session::data_dir().join("digest").join(&expanded)
                }
            }
        };
        if let Some(parent) = resolved.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                return encode_error(
                    id,
                    "digest_write_failed",
                    format!("mkdir {}: {err}", parent.display()),
                );
            }
        }
        let events_considered = events.iter().filter(|(_, ts, _)| *ts >= since_ms).count() as u64;
        if let Err(err) = std::fs::write(&resolved, html) {
            return encode_error(
                id,
                "digest_write_failed",
                format!("write {}: {err}", resolved.display()),
            );
        }
        encode_success(
            id,
            ResponseResult::DigestRendered {
                path: resolved.display().to_string(),
                events_considered,
                generated_for_date: date,
            },
        )
    }
}
