use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::api::schema::{
    ErrorResponse, EventsSubscribeParams, Method, PingParams, Request, ResponseResult,
    SubscriptionEventEnvelope, SuccessResponse,
};

mod input_write;
pub(crate) use input_write::SUBMIT_CAP;

const MIN_ALLOCATION_PREVIEW_PROTOCOL: u32 = 26;

/// How many times `request_value_retrying` sends a request again after a
/// transient transport error, on top of the first attempt.
const TRANSIENT_RETRIES: u32 = 3;

/// The pause before the first retry, doubled before each one after it.
const TRANSIENT_BACKOFF: Duration = Duration::from_millis(100);

/// API connection target resolved by clients at the process edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    LocalSession(Option<String>),
    SocketPath(PathBuf),
}

impl ConnectionTarget {
    fn socket_path(&self) -> PathBuf {
        match self {
            Self::LocalSession(None) => crate::api::socket_path(),
            Self::LocalSession(Some(name)) => crate::session::api_socket_path_for(Some(name)),
            Self::SocketPath(path) => path.clone(),
        }
    }
}

/// Reusable client for Flock's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    target: ConnectionTarget,
}

impl ApiClient {
    pub fn local() -> Self {
        Self::for_target(ConnectionTarget::LocalSession(None))
    }

    pub fn for_target(target: ConnectionTarget) -> Self {
        Self { target }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.target.socket_path()
    }

    pub fn request(&self, request: Request) -> Result<SuccessResponse, ApiClientError> {
        let value = self.request_value(&request)?;
        parse_response_value(value)
    }

    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        if input_write::requires(&request.method) {
            return input_write::call(self, request, input_write::cap(&request.method), None);
        }
        self.check_allocation_preview_protocol(request, None)?;
        if let Some(refusal) = super::compatibility::refuse_skew(self, request) {
            return Ok(refusal);
        }
        let mut stream = self.connect()?;
        write_request(&mut stream, request)?;

        let mut reader = BufReader::new(stream);
        let mut response = read_json_line(&mut reader)?;
        if matches!(request.method, Method::Ping(_)) {
            super::compatibility::observe_ping(self, &response);
        } else {
            super::compatibility::normalize(self, &mut response);
        }
        Ok(response)
    }

    pub fn request_value_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        if input_write::requires(&request.method) {
            return input_write::call(self, request, timeout, None);
        }
        self.check_allocation_preview_protocol(request, Some(timeout))?;
        if let Some(refusal) = super::compatibility::refuse_skew(self, request) {
            return Ok(refusal);
        }
        self.exchange_with_timeout(request, timeout)
            .map_err(Attempt::into_error)
    }

    /// `request_value_with_timeout`, sent again with backoff when the socket
    /// answers with a transient error (#910).
    ///
    /// Completed-input calls are never retried, including before dispatch;
    /// their shared path reports lost post-dispatch replies as unknown effect.
    ///
    /// An elapsed bounded reply wait reports `TimedOut`; the server may still
    /// be working. The request is retried up to [`TRANSIENT_RETRIES`] times,
    /// but only where that is safe:
    /// a failure before the request was written (the connect) retries for any
    /// method, one after it only for a [`Method::is_retry_safe`] read, since
    /// the server may already have applied a write it never answered.
    ///
    /// `timeout` bounds each attempt and `deadline`, when given, bounds all of
    /// them: no attempt is granted more than the time left, and no retry
    /// starts that its backoff would carry past the deadline.
    pub fn request_value_retrying(
        &self,
        request: &Request,
        timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<serde_json::Value, ApiClientError> {
        if input_write::requires(&request.method) {
            return input_write::call(self, request, timeout, deadline);
        }
        self.check_allocation_preview_protocol(request, Some(timeout))?;
        if let Some(refusal) = super::compatibility::refuse_skew(self, request) {
            return Ok(refusal);
        }
        let mut backoff = TRANSIENT_BACKOFF;
        let mut retries = 0;
        loop {
            let attempt_timeout = match deadline {
                None => timeout,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(ApiClientError::Io(io::ErrorKind::TimedOut.into()));
                    }
                    left.min(timeout)
                }
            };
            let failure = match self.exchange_with_timeout(request, attempt_timeout) {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            let retry = retries < TRANSIENT_RETRIES
                && failure.is_transient()
                && (failure.before_send() || request.method.is_retry_safe())
                && deadline.is_none_or(|deadline| Instant::now() + backoff < deadline);
            if !retry {
                return Err(failure.into_error());
            }
            tracing::debug!(
                request = request.id.as_str(),
                retry = retries + 1,
                error = failure.error().to_string().as_str(),
                "api request hit a transient error; retrying",
            );
            std::thread::sleep(backoff);
            backoff *= 2;
            retries += 1;
        }
    }

    /// One bounded round trip, saying whether it failed before the request
    /// reached the socket.
    fn exchange_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, Attempt> {
        self.exchange_until(request, Instant::now() + timeout)
    }

    /// Preserve the requiring input caller's original absolute deadline.
    fn exchange_until(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> Result<serde_json::Value, Attempt> {
        self.exchange_until_after_write(request, deadline, || {})
    }

    /// Test seam: gate the first read until the peer has replied and closed.
    /// Production uses a no-op hook and the same transport path.
    fn exchange_until_after_write(
        &self,
        request: &Request,
        deadline: Instant,
        after_write: impl FnOnce(),
    ) -> Result<serde_json::Value, Attempt> {
        let mut stream = self
            .connect()
            .map_err(|err| Attempt::BeforeSend(err.into()))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Attempt::BeforeSend(ApiClientError::Io(
                io::ErrorKind::TimedOut.into(),
            )));
        }
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|err| Attempt::BeforeSend(err.into()))?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|err| Attempt::BeforeSend(err.into()))?;
        write_request(&mut stream, request).map_err(Attempt::AfterSend)?;
        after_write();
        let mut response =
            read_json_line_until(&stream, deadline, Instant::now).map_err(Attempt::AfterSend)?;
        if matches!(request.method, Method::Ping(_)) {
            super::compatibility::observe_ping(self, &response);
        } else if input_write::requires(&request.method) {
            if input_write::normalize(&request.method, &mut response) {
                response["id"] = serde_json::json!(request.id);
            }
        } else {
            super::compatibility::normalize(self, &mut response);
        }
        Ok(response)
    }

    #[allow(dead_code)] // Kept as the typed subscription API; CLI wait paths use subscribe_value to preserve raw ack errors.
    pub fn subscribe(
        &self,
        id: impl Into<String>,
        params: EventsSubscribeParams,
        read_timeout: Option<Duration>,
    ) -> Result<(SuccessResponse, EventStream), ApiClientError> {
        let request = Request {
            id: id.into(),
            method: Method::EventsSubscribe(params),
        };
        let (ack, stream) = self.subscribe_value(&request, read_timeout)?;
        Ok((parse_response_value(ack)?, stream))
    }

    pub fn subscribe_value(
        &self,
        request: &Request,
        read_timeout: Option<Duration>,
    ) -> Result<(serde_json::Value, EventStream), ApiClientError> {
        if input_write::requires(&request.method) {
            return Err(ApiClientError::UnexpectedResult(
                "input calls are not subscriptions".into(),
            ));
        }
        let mut stream = self.connect()?;
        write_request(&mut stream, request)?;
        if let Some(timeout) = read_timeout {
            stream.set_read_timeout(Some(timeout))?;
        }

        let mut reader = BufReader::new(stream);
        let mut ack = read_json_line(&mut reader)?;
        super::compatibility::normalize(self, &mut ack);
        Ok((ack, EventStream { reader }))
    }

    pub fn status(&self) -> Result<crate::api::RuntimeStatus, ApiClientError> {
        let response = self.request(Request {
            id: "api-client:status".into(),
            method: Method::Ping(PingParams::default()),
        })?;
        match response.result {
            ResponseResult::Pong {
                version,
                protocol,
                capabilities,
                session_health,
                api_listener,
                agent_account_diagnostic,
            } => Ok(crate::api::RuntimeStatus {
                version: Some(version),
                protocol: Some(protocol),
                capabilities,
                session_health,
                api_listener,
                agent_account_diagnostic,
            }),
            result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
        }
    }

    fn check_allocation_preview_protocol(
        &self,
        request: &Request,
        timeout: Option<Duration>,
    ) -> Result<(), ApiClientError> {
        if !request.method.is_allocation_preview() {
            return Ok(());
        }
        let ping = Request {
            id: "api-client:preview-protocol".into(),
            method: Method::Ping(PingParams::default()),
        };
        let response =
            self.request_value_with_timeout(&ping, timeout.unwrap_or(Duration::from_secs(5)))?;
        let protocol = response
            .pointer("/result/protocol")
            .and_then(serde_json::Value::as_u64);
        if protocol.is_none_or(|version| version < u64::from(MIN_ALLOCATION_PREVIEW_PROTOCOL)) {
            return Err(ApiClientError::Io(io::Error::other(format!(
                "allocation preview requires server protocol {} or newer, received {protocol:?}; update the server before retrying --dry-run",
                MIN_ALLOCATION_PREVIEW_PROTOCOL,
            ))));
        }
        Ok(())
    }

    fn connect(&self) -> io::Result<UnixStream> {
        UnixStream::connect(self.socket_path())
    }
}

/// How one round trip failed, split at the point where the server could
/// first have seen the request.
#[derive(Debug)]
enum Attempt {
    /// Nothing was written: sending again cannot apply anything twice.
    BeforeSend(ApiClientError),
    /// The request may have reached the server, whatever came back.
    AfterSend(ApiClientError),
}

impl Attempt {
    fn error(&self) -> &ApiClientError {
        match self {
            Self::BeforeSend(err) | Self::AfterSend(err) => err,
        }
    }

    fn into_error(self) -> ApiClientError {
        match self {
            Self::BeforeSend(err) | Self::AfterSend(err) => err,
        }
    }

    fn before_send(&self) -> bool {
        matches!(self, Self::BeforeSend(_))
    }

    /// `EAGAIN`/`EWOULDBLOCK` is how a socket timeout surfaces, and `TimedOut`
    /// is the same event on a platform that names it. Anything else (a refused
    /// connection, a missing socket, a reply that does not parse) is not going
    /// to change by asking again a moment later.
    fn is_transient(&self) -> bool {
        matches!(
            self.error(),
            ApiClientError::Io(err)
                if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
        )
    }
}

pub struct EventStream {
    reader: BufReader<UnixStream>,
}

impl EventStream {
    /// Re-arm the per-read timeout mid-stream.
    ///
    /// `subscribe_value` sets the timeout once, which is enough for a wait
    /// that returns on the first event. A wait that has to READ PAST events
    /// it does not want — a readiness wait sees every status and title change
    /// on the pane — would otherwise get the full timeout again on each one,
    /// so a chatty pane could stretch a bounded wait indefinitely. Callers
    /// that loop re-arm this with whatever is left of their own deadline.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)
    }

    pub fn next_value(&mut self) -> Result<Option<serde_json::Value>, ApiClientError> {
        read_optional_json_line(&mut self.reader)
    }

    pub fn next_event(&mut self) -> Result<Option<SubscriptionEventEnvelope>, ApiClientError> {
        self.next_value()?
            .map(serde_json::from_value)
            .transpose()
            .map_err(ApiClientError::Json)
    }
}

#[derive(Debug)]
pub enum ApiClientError {
    Io(io::Error),
    Json(serde_json::Error),
    ErrorResponse(ErrorResponse),
    EmptyResponse,
    /// A bounded API reply exceeded its payload limit, excluding the newline.
    ReplyLineTooLarge {
        limit: usize,
    },
    UnexpectedResult(String),
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::ErrorResponse(response) => write!(f, "{}", response.error.message),
            Self::EmptyResponse => write!(f, "empty api response"),
            Self::ReplyLineTooLarge { limit } => write!(f, "api reply line exceeds {limit} bytes"),
            Self::UnexpectedResult(result) => write!(f, "unexpected api result: {result}"),
        }
    }
}

impl std::error::Error for ApiClientError {}

impl From<io::Error> for ApiClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ApiClientError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn write_request(stream: &mut UnixStream, request: &Request) -> Result<(), ApiClientError> {
    stream.write_all(serde_json::to_string(request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_json_line<T: DeserializeOwned>(
    reader: &mut BufReader<UnixStream>,
) -> Result<T, ApiClientError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 || line.trim().is_empty() {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_str(&line).map_err(ApiClientError::Json)
}

/// Maximum bounded reply payload: 8 MiB, excluding the terminating newline.
/// Legacy unbounded requests and streaming subscriptions retain their own readers.
const MAX_REPLY_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Poll each partial read against one absolute cap without post-write setsockopt.
/// Refuse payloads larger than [`MAX_REPLY_LINE_BYTES`] before growing the buffer.
/// Peer close is readable: queued reply bytes must survive it. The clock seam
/// lets tests drive expiry without sleeps or input replay.
fn read_json_line_until(
    stream: &UnixStream,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> Result<serde_json::Value, ApiClientError> {
    let mut line = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            return Err(ApiClientError::Io(io::ErrorKind::TimedOut.into()));
        }
        let read = match crate::platform::socket::read_ready(stream, &mut chunk, remaining) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error.into()),
        };
        if read == 0 {
            break;
        }
        let chunk = &chunk[..read];
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let payload = end.unwrap_or(chunk.len());
        if payload > MAX_REPLY_LINE_BYTES - line.len() {
            return Err(ApiClientError::ReplyLineTooLarge {
                limit: MAX_REPLY_LINE_BYTES,
            });
        }
        line.extend_from_slice(&chunk[..payload]);
        if end.is_some() {
            break;
        }
    }
    if line.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_slice(&line).map_err(ApiClientError::Json)
}

fn read_optional_json_line<T: DeserializeOwned>(
    reader: &mut BufReader<UnixStream>,
) -> Result<Option<T>, ApiClientError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.trim().is_empty() {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(ApiClientError::Json)
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireResponse {
    Success(Box<SuccessResponse>),
    Error(ErrorResponse),
}

pub(crate) fn parse_response_value(
    value: serde_json::Value,
) -> Result<SuccessResponse, ApiClientError> {
    match serde_json::from_value(value)? {
        WireResponse::Success(response) => Ok(*response),
        WireResponse::Error(response) => Err(ApiClientError::ErrorResponse(response)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reply_line_refuses_oversize_and_accepts_the_exact_limit() {
        // The documented bounded-reply payload limit excludes its newline.
        const LIMIT: usize = 8 * 1024 * 1024;
        fn read_payload(bytes: usize) -> Result<serde_json::Value, ApiClientError> {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let task = std::thread::spawn(move || {
                let data = format!("\"{}\"\n", "x".repeat(bytes - 2));
                if let Err(error) = writer.write_all(data.as_bytes()) {
                    assert!(
                        matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                        ),
                        "{error}"
                    );
                }
            });
            let result = read_json_line_until(
                &reader,
                Instant::now() + Duration::from_secs(10),
                Instant::now,
            );
            drop(reader);
            task.join().unwrap();
            result
        }
        let oversized = read_payload(LIMIT + 1);
        assert!(oversized.is_err(), "oversized JSON reply must be refused");
        let error = oversized.unwrap_err();
        assert!(matches!(&error, ApiClientError::ReplyLineTooLarge { limit } if *limit == LIMIT));
        assert_eq!(
            error.to_string(),
            format!("api reply line exceeds {LIMIT} bytes")
        );
        for bytes in [LIMIT - 1, LIMIT] {
            assert_eq!(
                read_payload(bytes).unwrap().as_str().unwrap().len(),
                bytes - 2
            );
        }
    }

    /// Joining the peer gates the client after write until the reply and EOF exist.
    fn closed_reply(method: Method, result: serde_json::Value) {
        let tag = if matches!(method, Method::PaneSubmit(_)) {
            "pane.submit"
        } else {
            "ping"
        };
        let peer = crate::test_support::ScriptedApi::new(vec![(tag, Some(result.clone()))]);
        let client = peer.client();
        let request = Request {
            id: "closed-peer".into(),
            method,
        };
        let response = client
            .exchange_until_after_write(&request, Instant::now() + input_write::SUBMIT_CAP, || {
                assert_eq!(peer.finish().len(), 1);
            })
            .unwrap_or_else(|error| match error {
                Attempt::BeforeSend(error) => panic!("before dispatch: {error}"),
                Attempt::AfterSend(error) => panic!("discarded a complete reply: {error}"),
            });
        let mut expected = result;
        expected["id"] = serde_json::json!(request.id);
        assert_eq!(response, expected);
    }
    fn closed_submit() -> Method {
        Method::PaneSubmit(
            serde_json::from_value(serde_json::json!({
            "pane_id":"fixture-pane", "text":"brief" }))
            .unwrap(),
        )
    }
    #[test]
    fn r3_closed_peer_submit_refusal_stays_a_refusal() {
        closed_reply(
            closed_submit(),
            serde_json::json!({"error":{
            "code":"input_not_empty", "message":"draft is present"}}),
        );
    }
    #[test]
    fn r3_closed_peer_submit_success_stays_a_success() {
        closed_reply(
            closed_submit(),
            serde_json::json!({"result":{
            "type":"guarded_submit", "outcome":"accepted", "reason":null, "retried":false}}),
        );
    }
    #[test]
    fn r3_closed_peer_ping_refusal_stays_a_refusal() {
        closed_reply(
            Method::Ping(PingParams::default()),
            serde_json::json!({"error":{
            "code":"invalid_params", "message":"invalid probe"}}),
        );
    }
    #[test]
    fn r3_closed_peer_ping_success_stays_a_success() {
        closed_reply(
            Method::Ping(PingParams::default()),
            serde_json::json!({"result":{
            "type":"pong", "pid":1, "version":"fixture", "protocol_version":crate::protocol::PROTOCOL_VERSION,
            "capabilities":{"pane_input_write_ack":true}}}),
        );
    }

    #[test]
    fn fragmented_reply_cannot_renew_the_absolute_cap() {
        let (stream, mut producer) = UnixStream::pair().unwrap();
        producer.write_all(b"{").unwrap();
        let start = Instant::now();
        let deadline = start + Duration::from_millis(100);
        let mut reads = 0;
        let error = read_json_line_until(&stream, deadline, || {
            reads += 1;
            if reads == 1 {
                start
            } else {
                deadline + Duration::from_nanos(1)
            }
        })
        .unwrap_err();
        assert!(matches!(error,ApiClientError::Io(ref e) if e.kind()==io::ErrorKind::TimedOut));
        assert_eq!(reads, 2, "each fragment rechecks the same cap");
        drop(producer);
    }
    #[test]
    fn allocation_dry_run_refuses_old_server_before_sending_allocation() {
        let path = std::env::temp_dir().join(format!("f459-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let ping: Request = read_json_line(&mut reader).unwrap();
            assert!(matches!(ping.method, Method::Ping(_)));
            let response = serde_json::json!({"id": ping.id, "result": {
                "type": "pong", "version": "old", "protocol": MIN_ALLOCATION_PREVIEW_PROTOCOL - 1,
            }});
            writeln!(reader.get_mut(), "{response}").unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let request = Request {
            id: "preview".into(),
            method: Method::WorktreeCreate(crate::api::schema::WorktreeCreateParams {
                dry_run: true,
                ..Default::default()
            }),
        };
        let error = client.request_value(&request).unwrap_err();
        assert!(error.to_string().contains("requires server protocol"));
        let error = client
            .request_value_with_timeout(&request, Duration::from_millis(100))
            .unwrap_err();
        assert!(!error.to_string().is_empty());
        let listener = server.join().unwrap();
        // The second attempt only queued another protocol ping, never an allocation.
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let ping: Request = read_json_line(&mut reader).unwrap();
        assert!(matches!(ping.method, Method::Ping(_)));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn allocation_dry_run_accepts_protocol_27() {
        let path = std::env::temp_dir().join(format!("f459-new-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let ping: Request = read_json_line(&mut reader).unwrap();
            assert!(matches!(ping.method, Method::Ping(_)));
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::json!({"id": ping.id, "result": {
                    "type": "pong", "version": "future", "protocol": 27,
                }})
            )
            .unwrap();
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let request: Request = read_json_line(&mut reader).unwrap();
            assert!(request.method.is_allocation_preview());
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::json!({"id": request.id, "result": {
                    "type": "allocation_plan", "operation": "worktree.create", "plan": {},
                }})
            )
            .unwrap();
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let response = client
            .request_value(&Request {
                id: "preview".into(),
                method: Method::WorktreeCreate(crate::api::schema::WorktreeCreateParams {
                    dry_run: true,
                    ..Default::default()
                }),
            })
            .unwrap();
        assert_eq!(response["result"]["type"], "allocation_plan");
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    /// #1028: a server from before `events.read` rejects it as an unknown
    /// variant, and the client reports the version gap (exit 78), not that.
    #[test]
    fn events_read_against_an_older_server_is_a_version_gap() {
        let path = std::env::temp_dir().join(format!("f1028-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for reply in [
                serde_json::json!({"code": "invalid_request",
                    "message": "invalid request: unknown variant `events.read`"}),
                serde_json::json!({"type": "pong", "version": "1.2.0", "protocol": 27}),
            ] {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let request: Request = read_json_line(&mut reader).unwrap();
                let key = if reply["code"].is_string() {
                    "error"
                } else {
                    "result"
                };
                writeln!(
                    reader.get_mut(),
                    "{}",
                    serde_json::json!({"id": request.id, key: reply})
                )
                .unwrap();
            }
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let response = client
            .request_value(&Request {
                id: "read".into(),
                method: Method::EventsRead(Default::default()),
            })
            .unwrap();
        assert_eq!(
            response["error"]["code"],
            super::super::compatibility::ERROR_CODE
        );
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    /// A fake server for #910 that takes exactly `connections` connections,
    /// reads each request, and leaves the first `stalls` of them unanswered so
    /// the client's read times out with `EAGAIN`, the way a server under load
    /// does. Every later one gets a `pong`. The thread hands back the listener,
    /// switched to non-blocking, so a test can prove nothing else connected.
    fn stalling_server(
        name: &str,
        stalls: usize,
        connections: usize,
    ) -> (PathBuf, std::thread::JoinHandle<StallingServer>) {
        let path = std::env::temp_dir().join(format!("f910-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut held = Vec::new();
            let mut seen = Vec::new();
            for index in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let request: Request = read_json_line(&mut reader).unwrap();
                if index >= stalls {
                    let response = serde_json::json!({"id": request.id, "result": {
                        "type": "pong", "version": "test", "protocol": 27,
                    }});
                    writeln!(reader.get_mut(), "{response}").unwrap();
                } else {
                    held.push(reader);
                }
                seen.push(request);
            }
            listener.set_nonblocking(true).unwrap();
            // The stalled connections are handed back rather than dropped: a
            // close here would reach the client as EOF instead of a timeout.
            (listener, seen, held)
        });
        (path, server)
    }

    type StallingServer = (
        std::os::unix::net::UnixListener,
        Vec<Request>,
        Vec<BufReader<UnixStream>>,
    );

    fn assert_no_further_connection(listener: &std::os::unix::net::UnixListener) {
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "the client connected more often than the test allowed"
        );
    }

    fn agent_get() -> Request {
        Request {
            id: "f910".into(),
            method: Method::AgentGet(crate::api::schema::AgentTarget {
                target: "worker".into(),
            }),
        }
    }

    const ATTEMPT: Duration = Duration::from_millis(50);

    #[test]
    fn a_read_is_retried_through_eagain_until_it_is_answered() {
        let (path, server) = stalling_server("read", 2, 3);
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let response = client
            .request_value_retrying(&agent_get(), ATTEMPT, None)
            .expect("the third attempt is answered");
        assert_eq!(response["result"]["type"], "pong");
        let (listener, seen, _held) = server.join().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|request| request == &agent_get()));
        assert_no_further_connection(&listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_write_that_timed_out_is_not_sent_again() {
        let (path, server) = stalling_server("write", 1, 1);
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let close = Request {
            id: "f910".into(),
            method: Method::PaneClose(crate::api::schema::PaneTarget {
                pane_id: "w1:p1".into(),
            }),
        };
        let error = client
            .request_value_retrying(&close, ATTEMPT, None)
            .unwrap_err();
        assert!(
            matches!(&error, ApiClientError::Io(err) if err.kind() == io::ErrorKind::TimedOut),
            "the timeout is reported as it happened: {error:?}"
        );
        let (listener, seen, _held) = server.join().unwrap();
        assert_eq!(seen, vec![close]);
        assert_no_further_connection(&listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_read_that_never_answers_gives_up_after_the_retries() {
        let attempts = TRANSIENT_RETRIES as usize + 1;
        let (path, server) = stalling_server("never", attempts, attempts);
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let error = client
            .request_value_retrying(&agent_get(), ATTEMPT, None)
            .unwrap_err();
        assert!(
            matches!(&error, ApiClientError::Io(err) if err.kind() == io::ErrorKind::TimedOut),
            "{error:?}"
        );
        let (listener, seen, _held) = server.join().unwrap();
        assert_eq!(seen.len(), attempts);
        assert_no_further_connection(&listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn no_retry_starts_that_the_deadline_cannot_hold() {
        let (path, server) = stalling_server("deadline", 1, 1);
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        // One 50 ms attempt fits; the 100 ms backoff after it does not.
        let deadline = Instant::now() + Duration::from_millis(120);
        let error = client
            .request_value_retrying(&agent_get(), ATTEMPT, Some(deadline))
            .unwrap_err();
        assert!(matches!(&error, ApiClientError::Io(_)), "{error:?}");
        // The connection count below is the real check; this only catches a
        // retry loop that ignored the deadline outright, with room for a slow
        // host.
        assert!(Instant::now() < deadline + Duration::from_secs(2));
        let (listener, seen, _held) = server.join().unwrap();
        assert_eq!(seen.len(), 1);
        assert_no_further_connection(&listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn only_reads_and_previews_are_retry_safe() {
        assert!(agent_get().method.is_retry_safe());
        assert!(Method::Ping(PingParams::default()).is_retry_safe());
        assert!(
            Method::WorktreeCreate(crate::api::schema::WorktreeCreateParams {
                dry_run: true,
                ..Default::default()
            })
            .is_retry_safe()
        );
        assert!(!Method::WorktreeCreate(Default::default()).is_retry_safe());
        assert!(!Method::PaneClose(crate::api::schema::PaneTarget {
            pane_id: "w1:p1".into(),
        })
        .is_retry_safe());
        assert!(!Method::MsgRead(crate::api::schema::MsgReadParams { pane: None }).is_retry_safe());
    }

    #[test]
    fn local_session_target_resolves_named_session_socket() {
        let client = ApiClient::for_target(ConnectionTarget::LocalSession(Some("work".into())));
        assert!(client.socket_path().ends_with("sessions/work/flock.sock"));
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/tmp/flock-test.sock");
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        assert_eq!(client.socket_path(), path);
    }
    #[test]
    fn s3a_r2_malformed_optional_error_data_preserves_the_primary_error() {
        for data in [
            serde_json::json!(17),
            serde_json::json!(true),
            serde_json::json!("future data"),
            serde_json::json!([]),
            serde_json::json!({"input_write": []}),
            serde_json::json!({"input_write": {"bytes_written": -1, "bytes_total": 5,
                "reason": "deadline", "remainder_discarded": true}}),
            serde_json::json!({"input_write": {"reason": "deadline"}}),
        ] {
            let raw = serde_json::json!({"id":"write", "error":{
                "code":"input_write_failed", "message":"inspect the pane", "data":data}});
            let Err(ApiClientError::ErrorResponse(response)) = parse_response_value(raw) else {
                panic!("optional error data must not hide the primary refusal: {data}");
            };
            assert_eq!(response.id, "write");
            assert_eq!(response.error.code, "input_write_failed");
            assert_eq!(response.error.message, "inspect the pane");
            assert_eq!(crate::cli::error_code_exit(&response.error.code, false), 8);
            assert!(
                response.error.data.is_none(),
                "invalid detail must be ignored"
            );
        }
    }
}
