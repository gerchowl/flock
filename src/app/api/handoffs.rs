//! `handoff.list` / `handoff.read` (#286, ADR-0017).
//!
//! The read side of the handed-over file record. Both verbs are read-only —
//! neither appears in `request_changes_ui`, neither marks a pane seen — so an
//! agent may poll them the way it polls `agent.history`.
//!
//! Neither takes a path. `handoff.read` addresses bytes only by an id flock
//! minted when a human handed the file over, which is what keeps a resource
//! surface from becoming a file-read surface.

use crate::api::schema::{
    HandoffEncoding, HandoffListParams, HandoffReadParams, HandoffSummary, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

/// Above this, `handoff.read` refuses and points at the staged path instead.
///
/// The bytes and the agent are on the same machine by construction, so the
/// refusal costs nothing: the caller reads the file with its own tool. What
/// it buys is that one dropped video cannot put 20 MB of base64 on a
/// line-delimited JSON-RPC pipe that every other tool call shares.
pub(crate) const MAX_HANDOFF_READ_BYTES: u64 = 4 * 1024 * 1024;

impl App {
    pub(super) fn handle_handoff_list(&mut self, id: String, params: HandoffListParams) -> String {
        // `target` filters by identity, not by placement: an agent_id
        // survives pane renumbering and a pane id does not.
        let filter = match params.target.as_deref() {
            Some(target) => match self.resolve_terminal_target(target) {
                Ok(resolved) => {
                    let pane_id = self.public_pane_id(resolved.ws_idx, resolved.pane_id);
                    let agent_id = self
                        .state
                        .terminals
                        .iter()
                        .find(|(terminal_id, _)| terminal_id.to_string() == resolved.terminal_id)
                        .map(|(_, terminal)| terminal.agent_id.to_string());
                    Some((pane_id, agent_id))
                }
                Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
            },
            None => None,
        };

        let total = self.state.handoffs.len();
        let limit = params.limit.unwrap_or(usize::MAX);
        let files: Vec<HandoffSummary> = self
            .state
            .handoffs
            .newest_first()
            .filter(|entry| match &filter {
                None => true,
                Some((pane_id, agent_id)) => {
                    (agent_id.is_some() && entry.agent_id == *agent_id)
                        || (pane_id.is_some() && entry.pane_id == *pane_id)
                }
            })
            .take(limit)
            .map(|entry| HandoffSummary {
                file_id: entry.id.clone(),
                name: entry.name.clone(),
                mime: entry.mime.clone(),
                bytes: entry.bytes,
                path: entry.path.to_string_lossy().into_owned(),
                workspace_id: entry.workspace_id.clone(),
                pane_id: entry.pane_id.clone(),
                agent_id: entry.agent_id.clone(),
                origin_host: entry.origin_host.clone(),
                received_at_ms: entry.received_at_ms,
            })
            .collect();

        encode_success(id, ResponseResult::HandoffList { files, total })
    }

    pub(super) fn handle_handoff_read(&mut self, id: String, params: HandoffReadParams) -> String {
        let Some(entry) = self.state.handoffs.get(&params.file_id).cloned() else {
            return encode_error(
                id,
                "handoff_not_found",
                format!(
                    "no handed-over file with id {} — list them with handoff.list",
                    params.file_id
                ),
            );
        };

        if entry.bytes > MAX_HANDOFF_READ_BYTES {
            return encode_error(
                id,
                "handoff_too_large",
                format!(
                    "{} is {} bytes, over the {MAX_HANDOFF_READ_BYTES}-byte inline limit; read it \
                     from {} instead",
                    entry.name,
                    entry.bytes,
                    entry.path.display()
                ),
            );
        }

        let bytes = match std::fs::read(&entry.path) {
            Ok(bytes) => bytes,
            Err(err) => {
                // The record outlived its bytes between the boot-time
                // reconciliation and now. Say which, rather than serving an
                // empty file.
                return encode_error(
                    id,
                    "handoff_unreadable",
                    format!("{} could not be read: {err}", entry.path.display()),
                );
            }
        };

        // Text when it really is text. An agent handed `content` as a string
        // can use it; one handed base64 has to decode it first, so a wrong
        // guess in that direction costs a round trip. Images and PDFs go
        // straight to base64 — a PDF's header is valid UTF-8 often enough to
        // matter, and "text" would be a lie about all of it.
        let is_opaque = entry.mime.starts_with("image/") || entry.mime == "application/pdf";
        let (encoding, content) = match std::str::from_utf8(&bytes) {
            Ok(text) if !is_opaque => (HandoffEncoding::Utf8, text.to_string()),
            _ => (HandoffEncoding::Base64, base64_encode(&bytes)),
        };

        encode_success(
            id,
            ResponseResult::HandoffRead {
                file_id: entry.id,
                name: entry.name,
                mime: entry.mime,
                bytes: entry.bytes,
                path: entry.path.to_string_lossy().into_owned(),
                encoding,
                content,
            },
        )
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{Method, Request};
    use crate::app::App;

    fn handoff_test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app
    }

    fn request(id: &str, method: Method) -> Request {
        Request {
            id: id.into(),
            method,
        }
    }

    fn call(app: &mut App, method: Method) -> serde_json::Value {
        let raw = app.handle_api_request(request("req", method));
        serde_json::from_str(&raw).expect("api response is json")
    }

    /// A file staged on disk, the way a drop stages one.
    fn stage(extension: &str, data: &[u8]) -> crate::server::clipboard_image::StagedClipboardImage {
        crate::server::clipboard_image::stage(7, extension, data).expect("stage")
    }

    /// The whole point of #286, driven from where the bytes really arrive:
    /// stage a file the way a drop does, then read it back by id.
    ///
    /// The paste that used to be the entire record is not involved. Before
    /// this, nothing could answer "what was I handed" at all.
    #[tokio::test]
    async fn a_staged_file_is_listed_and_readable_by_id() {
        let mut app = handoff_test_app();
        let staged = stage("md", b"# handed over\n");
        let file_id = app.record_file_handoff(None, staged.path.clone(), "md", 14);

        let listed = call(&mut app, Method::HandoffList(Default::default()));
        assert_eq!(listed["result"]["total"], 1);
        let row = &listed["result"]["files"][0];
        assert_eq!(row["file_id"], file_id);
        assert_eq!(row["mime"], "text/markdown");
        assert_eq!(row["bytes"], 14);
        assert!(
            row["pane_id"].is_string(),
            "the drop landed on the focused pane and the record says so: {row}"
        );

        let read = call(
            &mut app,
            Method::HandoffRead(crate::api::schema::HandoffReadParams {
                file_id: file_id.clone(),
            }),
        );
        assert_eq!(read["result"]["encoding"], "utf8");
        assert_eq!(read["result"]["content"], "# handed over\n");
        let _ = std::fs::remove_file(&staged.path);
    }

    /// The record is derived, so it has to be reconstructible from the events
    /// it queued and nothing else — otherwise a restart is a different truth.
    #[tokio::test]
    async fn a_handoff_rebuilds_itself_from_the_durable_log() {
        let mut app = handoff_test_app();
        let staged = stage("pdf", b"%PDF-1.7 x");
        let file_id = app.record_file_handoff(None, staged.path.clone(), "pdf", 10);
        app.drain_pending_ui_events();

        let published: Vec<crate::api::schema::EventEnvelope> = app
            .event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, envelope)| envelope)
            .collect();
        let mut rebuilt = crate::app::handoffs::HandoffLog::default();
        rebuilt.seed_from_events(published.iter(), &|path| path.exists());

        assert_eq!(rebuilt.len(), 1, "the record reached the durable log");
        assert_eq!(
            rebuilt.newest_first().next().map(|entry| entry.id.clone()),
            Some(file_id)
        );
        let _ = std::fs::remove_file(&staged.path);
    }

    /// Binary stays binary. A PDF's header is valid UTF-8 often enough that
    /// "is it text" cannot be decided by decoding alone.
    #[tokio::test]
    async fn a_pdf_comes_back_base64_even_though_its_header_decodes() {
        let mut app = handoff_test_app();
        let staged = stage("pdf", b"%PDF-1.7 hello");
        let file_id = app.record_file_handoff(None, staged.path.clone(), "pdf", 14);

        let read = call(
            &mut app,
            Method::HandoffRead(crate::api::schema::HandoffReadParams { file_id }),
        );
        assert_eq!(read["result"]["encoding"], "base64");
        assert_eq!(read["result"]["mime"], "application/pdf");
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(read["result"]["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"%PDF-1.7 hello");
        let _ = std::fs::remove_file(&staged.path);
    }

    #[tokio::test]
    async fn an_unknown_id_refuses_rather_than_returning_an_empty_file() {
        let mut app = handoff_test_app();
        let read = call(
            &mut app,
            Method::HandoffRead(crate::api::schema::HandoffReadParams {
                file_id: "file:nope:0".into(),
            }),
        );
        assert_eq!(read["error"]["code"], "handoff_not_found");
    }

    /// Over the inline limit the caller gets the path, not a truncated file.
    /// The bytes and the agent share a machine, so this is a redirection
    /// rather than a denial.
    #[tokio::test]
    async fn an_oversized_handoff_points_at_its_path_instead_of_inlining_it() {
        let mut app = handoff_test_app();
        let staged = stage("bin", b"small on disk, large on the record");
        app.state
            .record_handoff(crate::app::handoffs::HandoffEntry {
                id: "file:big:0".into(),
                name: "huge.bin".into(),
                mime: "application/octet-stream".into(),
                bytes: super::MAX_HANDOFF_READ_BYTES + 1,
                path: staged.path.clone(),
                workspace_id: None,
                pane_id: None,
                agent_id: None,
                origin_host: "host".into(),
                received_at_ms: 1,
            });

        let read = call(
            &mut app,
            Method::HandoffRead(crate::api::schema::HandoffReadParams {
                file_id: "file:big:0".into(),
            }),
        );
        assert_eq!(read["error"]["code"], "handoff_too_large");
        assert!(
            read["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(&staged.path.to_string_lossy().into_owned())),
            "the refusal has to say where the bytes are: {}",
            read["error"]["message"]
        );
        let _ = std::fs::remove_file(&staged.path);
    }

    /// `target` narrows to one agent; the default does not narrow at all.
    ///
    /// The unfiltered default is deliberate (ADR-0017): a caller that cannot
    /// be tied to a pane must not be told "nothing was handed over" when
    /// something was.
    #[tokio::test]
    async fn target_narrows_to_one_agent_and_the_default_does_not() {
        let mut app = handoff_test_app();
        let staged = stage("txt", b"mine");
        app.record_file_handoff(None, staged.path.clone(), "txt", 4);
        app.state
            .record_handoff(crate::app::handoffs::HandoffEntry {
                id: "file:someone-else:0".into(),
                name: "theirs.txt".into(),
                mime: "text/plain".into(),
                bytes: 6,
                path: staged.path.clone(),
                workspace_id: Some("ws_9".into()),
                pane_id: Some("ws_9:p9".into()),
                agent_id: Some("agent_elsewhere".into()),
                origin_host: "host".into(),
                received_at_ms: 2,
            });

        let all = call(&mut app, Method::HandoffList(Default::default()));
        assert_eq!(all["result"]["files"].as_array().unwrap().len(), 2);

        let pane_id = app
            .state
            .handoffs
            .newest_first()
            .find(|entry| entry.id != "file:someone-else:0")
            .and_then(|entry| entry.pane_id.clone())
            .expect("the local drop resolved a pane");
        let mine = call(
            &mut app,
            Method::HandoffList(crate::api::schema::HandoffListParams {
                target: Some(pane_id),
                limit: None,
            }),
        );
        let files = mine["result"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "target filtered to one agent's files");
        assert_ne!(files[0]["file_id"], "file:someone-else:0");
        assert_eq!(
            mine["result"]["total"], 2,
            "total still reports the whole projection, so a filtered empty \
             list is distinguishable from an empty flock"
        );
        let _ = std::fs::remove_file(&staged.path);
    }
}
