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
        self.check_allocation_preview_protocol(request, None)?;
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
        self.check_allocation_preview_protocol(request, Some(timeout))?;
        self.exchange_with_timeout(request, timeout)
            .map_err(Attempt::into_error)
    }

    /// `request_value_with_timeout`, sent again with backoff when the socket
    /// answers with a transient error (#910).
    ///
    /// Under load a read timeout surfaces as `EAGAIN` ("Resource temporarily
    /// unavailable", os error 11 on Linux): the server is there, just slow.
    /// One such error used to fail a whole CLI command. Now the request is
    /// retried up to [`TRANSIENT_RETRIES`] times, but only where that is safe:
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
        self.check_allocation_preview_protocol(request, Some(timeout))?;
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
        let mut stream = self
            .connect()
            .map_err(|err| Attempt::BeforeSend(err.into()))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|err| Attempt::BeforeSend(err.into()))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| Attempt::BeforeSend(err.into()))?;
        write_request(&mut stream, request).map_err(Attempt::AfterSend)?;

        let mut reader = BufReader::new(stream);
        let mut response = read_json_line(&mut reader).map_err(Attempt::AfterSend)?;
        if matches!(request.method, Method::Ping(_)) {
            super::compatibility::observe_ping(self, &response);
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
            } => Ok(crate::api::RuntimeStatus {
                version: Some(version),
                protocol: Some(protocol),
                capabilities,
                session_health,
                api_listener,
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
    UnexpectedResult(String),
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::ErrorResponse(response) => write!(f, "{}", response.error.message),
            Self::EmptyResponse => write!(f, "empty api response"),
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
            matches!(&error, ApiClientError::Io(err) if err.kind() == io::ErrorKind::WouldBlock),
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
            matches!(&error, ApiClientError::Io(err) if err.kind() == io::ErrorKind::WouldBlock),
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
}
