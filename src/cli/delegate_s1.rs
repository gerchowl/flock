//! Event-time, advisory System One client. No endpoint is compiled in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::model::{SystemOneConfig, SystemOneTier};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Advice {
    pub fused: f64,
    pub verdict: String,
}

#[derive(Deserialize)]
struct Questions {
    questions: Value,
    score: Score,
}

#[derive(Deserialize)]
struct Score {
    bias: f64,
    weights: BTreeMap<String, f64>,
}

fn fuse(set: &Questions, response: &Value) -> Option<f64> {
    let answers = response.get("answers")?;
    let mut score = set.score.bias;
    for (feature, weight) in &set.score.weights {
        let probability = match feature.split_once(':') {
            None => answers.get(feature)?.get("noul")?.as_f64()?,
            Some((name, labels)) => {
                let probabilities = answers.get(name)?.get("probabilities")?;
                let mut sum = 0.0;
                for label in labels.split('+') {
                    sum += probabilities.get(label)?.as_f64()?;
                }
                sum
            }
        };
        if !probability.is_finite() {
            return None;
        }
        score += weight * probability.clamp(0.0, 1.0);
    }
    let fused = 1.0 / (1.0 + (-score).exp());
    fused.is_finite().then_some(fused)
}

/// Why an endpoint gave no advice. Only an endpoint that answered and refused
/// the request, or answered with something unusable, is reported. An endpoint
/// that is down or busy stays silent, as the advice is optional.
#[derive(Debug, PartialEq)]
enum Failure {
    /// Never connected, or a 5xx: the only failures the breaker counts.
    Down,
    /// Connected but did not answer in time, or 408/429. Clef serves one
    /// request at a time, so a queue of other clients' requests looks like
    /// this, and a healthy endpoint must not be skipped for it.
    Busy,
    Rejected {
        status: u16,
        error: String,
    },
    Unanswered,
}

impl Failure {
    fn counts_against_breaker(&self) -> bool {
        *self == Self::Down
    }

    fn report(&self, url: &str) -> Option<String> {
        let what = match self {
            Self::Down | Self::Busy => return None,
            Self::Rejected { status, error } => {
                format!("rejected the request (HTTP {status}): {error}")
            }
            Self::Unanswered => "answered without the questions' probabilities".into(),
        };
        Some(format!("flk delegate: System One at {url} {what}"))
    }
}

// Reads `answers` and nothing else, so extra fields such as `model`, `usage`
// or a cache count from either Kev or Clef never matter. Kev refused an
// over-long state with 422, Clef refuses it with 400 or 413 and never
// truncates, so a refusal is the only over-length signal to handle.
fn interpret(set: &Questions, status: u16, body: &[u8]) -> Result<f64, Failure> {
    let response = serde_json::from_slice::<Value>(body).ok();
    if !(200..300).contains(&status) {
        if status == 408 || status == 429 {
            return Err(Failure::Busy);
        }
        if !(400..500).contains(&status) {
            return Err(Failure::Down);
        }
        let error = match response.as_ref().and_then(|value| value.get("error")) {
            Some(Value::String(error)) => error.clone(),
            Some(error) => error.to_string(),
            None => String::from_utf8_lossy(body).chars().take(200).collect(),
        };
        return Err(Failure::Rejected { status, error });
    }
    fuse(set, &response.ok_or(Failure::Unanswered)?).ok_or(Failure::Unanswered)
}

/// Classifies one curl run. curl appends `<status> <connects>` after the
/// body, see `--write-out` below, and prints it even when the transfer fails.
fn outcome(set: &Questions, succeeded: bool, stdout: &[u8]) -> Result<f64, Failure> {
    let (body, trailer) = match stdout.iter().rposition(|byte| *byte == b'\n') {
        Some(newline) => (&stdout[..newline], &stdout[newline + 1..]),
        None => (stdout, &[][..]),
    };
    let mut fields = std::str::from_utf8(trailer)
        .unwrap_or("")
        .split_whitespace();
    let status = fields
        .next()
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let connected = fields
        .next()
        .and_then(|count| count.parse::<u32>().ok())
        .is_some_and(|count| count > 0);
    if !succeeded {
        return Err(if connected {
            Failure::Busy
        } else {
            Failure::Down
        });
    }
    interpret(set, status, body)
}

pub(super) fn tail(screen: &str) -> &str {
    let mut start = screen.len().saturating_sub(4096);
    while !screen.is_char_boundary(start) {
        start += 1;
    }
    &screen[start..]
}

/// Advice keeps its own budget, but a caller deadline also caps subprocess
/// grace. The injected clock keeps these limits testable without real I/O.
struct AdviceBudget {
    end: Instant,
    caller: Option<Instant>,
}

impl AdviceBudget {
    fn new(timeout: Duration, caller: Option<Instant>, now: Instant) -> Option<Self> {
        let own = now.checked_add(timeout)?;
        let end = caller.map_or(own, |caller| caller.min(own));
        (end > now).then_some(Self { end, caller })
    }

    /// A caller's exhausted budget is not evidence that its endpoint is down.
    fn accepts_response(&self, now: Instant) -> bool {
        self.caller.is_none_or(|caller| now < caller)
    }

    fn limits(&self, now: Instant) -> Option<(Duration, Duration)> {
        let remaining = self.end.checked_duration_since(now)?;
        let grace = remaining.saturating_add(Duration::from_millis(500));
        let process = self.caller.map_or(grace, |caller| {
            grace.min(caller.saturating_duration_since(now))
        });
        (!remaining.is_zero() && !process.is_zero()).then_some((remaining, process))
    }
}

pub(super) struct Client {
    urls: Vec<String>,
    timeout: Duration,
    state_dir: PathBuf,
}

impl Client {
    pub fn from_config() -> Self {
        let config = crate::config::Config::load().config.systemone;
        let env = match config.tier {
            SystemOneTier::Fast => "SYSTEMONE_URL",
            SystemOneTier::Full => "SYSTEMONE_URL_FULL",
        };
        Self::new(
            std::env::var(env).ok().as_deref(),
            &config,
            crate::config::state_dir().join("systemone"),
        )
    }

    // `env` is the tier's own variable, which overrides that tier's URLs.
    fn new(env: Option<&str>, config: &SystemOneConfig, state_dir: PathBuf) -> Self {
        let (configured, timeout_s, fallback) = match config.tier {
            SystemOneTier::Fast => (&config.urls, config.timeout_s, 4),
            SystemOneTier::Full => (&config.full_urls, config.full_timeout_s, 12),
        };
        let urls = match env {
            Some(env) => env
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|url| !url.is_empty())
                .map(str::to_string)
                .collect(),
            None => configured.clone(),
        };
        let timeout = Duration::try_from_secs_f64(timeout_s)
            .ok()
            .filter(|duration| !duration.is_zero())
            .unwrap_or(Duration::from_secs(fallback));
        Self {
            urls,
            timeout,
            state_dir,
        }
    }

    /// Advisory work cannot renew a caller's deadline, including curl's grace.
    /// Responses arriving at or after it return no advice and do not update
    /// endpoint health, whether they succeeded or failed.
    pub fn judge_before(
        &self,
        command: &str,
        screen: &str,
        caller: Option<Instant>,
    ) -> Option<Advice> {
        self.judge_before_with(
            command,
            screen,
            caller,
            Instant::now,
            |set, url, body, remaining, process_timeout| {
                crate::process::TracedCommand::new("curl", "delegate_s1")
                    .args([
                        "--disable",
                        "--silent",
                        "--show-error",
                        "--write-out",
                        "\n%{http_code} %{num_connects}",
                        "--proto",
                        "=http,https",
                        "--max-time",
                        &remaining.as_secs_f64().to_string(),
                        "--header",
                        "Content-Type: application/json",
                        "--data-binary",
                        "@-",
                        "--url",
                        url,
                    ])
                    // The grace lets curl's own `--max-time` fire first, so a slow
                    // answer is reported with its trailer as busy, not as down.
                    // A caller deadline caps that grace too.
                    .output_traced_with_stdin_and_timeout(body, process_timeout)
                    .map_or(Err(Failure::Down), |output| {
                        outcome(set, output.status.success(), &output.stdout)
                    })
            },
        )
    }

    /// Inject time and transport while retaining the real breaker lifecycle.
    fn judge_before_with(
        &self,
        command: &str,
        screen: &str,
        caller: Option<Instant>,
        mut now: impl FnMut() -> Instant,
        mut request: impl FnMut(&Questions, &str, &[u8], Duration, Duration) -> Result<f64, Failure>,
    ) -> Option<Advice> {
        if self.urls.is_empty() {
            return None;
        }
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).ok()?;
        let body = serde_json::to_vec(&json!({
            "state": format!("Command: {command}\n(The exit status is not shown.)\nLast output:\n{}", tail(screen)),
            "questions": set.questions,
        })).ok()?;
        let budget = AdviceBudget::new(self.timeout, caller, now())?;
        for url in &self.urls {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                continue;
            }
            budget.limits(now())?;
            // The lock serialises calls per endpoint across every flk process
            // on this host: a call that finds one in flight skips the endpoint
            // without counting a failure, so a burst of delegates finishing
            // together never queues locally behind a one-at-a-time backend.
            let Some(mut breaker) = Breaker::acquire(&self.state_dir, url, self.timeout) else {
                continue;
            };
            let (remaining, process_timeout) = budget.limits(now())?;
            let response = request(&set, url, &body, remaining, process_timeout);
            // Do not turn our caller's cancellation into an endpoint outage.
            if !budget.accepts_response(now()) {
                return None;
            }
            match &response {
                Ok(_) => breaker.record(true),
                Err(failure) if failure.counts_against_breaker() => breaker.record(false),
                Err(_) => {}
            }
            if let Some(report) = response
                .as_ref()
                .err()
                .and_then(|failure| failure.report(url))
            {
                eprintln!("{report}");
            }
            if let Ok(fused) = response {
                return Some(Advice {
                    fused,
                    verdict: if fused >= 0.5 {
                        "failing"
                    } else {
                        "not_failing"
                    }
                    .into(),
                });
            }
        }
        None
    }
}

#[derive(Default, Serialize, Deserialize)]
struct BreakerState {
    fails: u32,
    failed_at_ms: u64,
}

struct Breaker {
    path: PathBuf,
    lock: PathBuf,
    state: BreakerState,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Breaker {
    fn acquire(dir: &Path, url: &str, timeout: Duration) -> Option<Self> {
        std::fs::create_dir_all(dir).ok()?;
        let key: String = Sha256::digest(url.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = dir.join(format!("{key}.json"));
        let lock = dir.join(format!("{key}.lock"));
        let open = || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
        };
        if open().is_err() {
            let age = std::fs::metadata(&lock)
                .ok()?
                .modified()
                .ok()?
                .elapsed()
                .ok()?;
            if age <= timeout.saturating_add(Duration::from_secs(5)) {
                return None;
            }
            std::fs::remove_file(&lock).ok()?;
            open().ok()?;
        }
        let state = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let breaker = Self { path, lock, state };
        if breaker.state.fails >= 3 && now_ms().saturating_sub(breaker.state.failed_at_ms) < 600_000
        {
            return None;
        }
        Some(breaker)
    }

    fn record(&mut self, success: bool) {
        if success {
            self.state = BreakerState::default();
        } else {
            self.state.fails = self.state.fails.saturating_add(1);
            self.state.failed_at_ms = now_ms();
        }
        if let Ok(bytes) = serde_json::to_vec(&self.state) {
            let temp = self.path.with_extension("tmp");
            if std::fs::write(&temp, bytes).is_ok() {
                let _ = std::fs::rename(temp, &self.path);
            }
        }
    }
}

impl Drop for Breaker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn delegate_s1_test_dir_is_removed_during_unwind() {
        let dir = std::env::temp_dir().join(format!(
            "delegate_s1_unwind_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let cleanup = TestDir(dir.clone());
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("fixture"), b"endpoint health").unwrap();
        let failed = std::panic::catch_unwind(move || {
            let _cleanup = cleanup;
            panic!("fixture assertion failed");
        });
        assert!(failed.is_err());
        let leaked = dir.exists();
        if leaked {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        assert!(!leaked, "failed test must remove its fixture directory");
    }

    #[test]
    fn delegate_s1_advice_and_process_grace_respect_the_caller_budget() {
        let now = Instant::now();
        let own = Duration::from_secs(4);
        let unbounded = AdviceBudget::new(own, None, now).unwrap();
        assert_eq!(
            unbounded.limits(now),
            Some((own, Duration::from_millis(4500)))
        );
        let caller = now + Duration::from_secs(2);
        let bounded = AdviceBudget::new(own, Some(caller), now).unwrap();
        assert_eq!(
            bounded.limits(now),
            Some((Duration::from_secs(2), Duration::from_secs(2)))
        );
        assert_eq!(
            bounded.limits(now + Duration::from_secs(1)),
            Some((Duration::from_secs(1), Duration::from_secs(1)))
        );
        assert!(bounded.accepts_response(now));
        assert!(!bounded.accepts_response(caller));
        assert!(!bounded.accepts_response(caller + Duration::from_secs(1)));
        assert!(unbounded.accepts_response(now + Duration::from_secs(10)));
        assert_eq!(bounded.limits(caller), None);
        assert_eq!(bounded.limits(caller + Duration::from_secs(1)), None);
        assert!(AdviceBudget::new(own, Some(now), now).is_none());
        let later = AdviceBudget::new(own, Some(now + Duration::from_secs(10)), now).unwrap();
        assert_eq!(later.limits(now), unbounded.limits(now));
    }

    #[test]
    fn delegate_s1_discards_late_responses_without_recording_endpoint_health() {
        let dir = std::env::temp_dir().join(format!(
            "delegate_s1_late_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let _cleanup = TestDir(dir.clone());
        let client = Client::new(
            Some("http://s1.invalid"),
            &SystemOneConfig::default(),
            dir.clone(),
        );
        let mut breaker = Breaker::acquire(&dir, &client.urls[0], client.timeout).unwrap();
        breaker.record(false);
        let path = breaker.path.clone();
        let original = std::fs::read(&path).unwrap();
        drop(breaker);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        for response in [Ok(0.25), Err(Failure::Down)] {
            let mut response = Some(response);
            let clock = std::cell::Cell::new(start);
            let mut requests = 0;
            let advice = client.judge_before_with(
                "command",
                "screen",
                Some(deadline),
                || clock.get(),
                |_, _, _, _, _| {
                    requests += 1;
                    clock.set(deadline);
                    response.take().unwrap()
                },
            );
            assert_eq!(requests, 1, "response must reach the discard path");
            assert_eq!(advice, None);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "late success must not reset health; late failure must not count"
            );
            assert!(
                Breaker::acquire(&dir, &client.urls[0], client.timeout).is_some(),
                "discard must release the endpoint lock"
            );
        }
        // A timely response still updates the same persisted breaker state.
        let advice = client.judge_before_with(
            "command",
            "screen",
            Some(deadline),
            || start,
            |_, _, _, _, _| Ok(0.25),
        );
        assert_eq!(advice.unwrap().fused, 0.25);
        let state: BreakerState = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(state.fails, 0);
    }

    #[test]
    fn delegate_s1_fusion_uses_every_scored_answer_and_preserves_utf8_tail() {
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).unwrap();
        assert!(fuse(&set, &json!({"answers": {}})).is_none());
        let mut answers = serde_json::Map::new();
        for key in set.score.weights.keys().filter(|key| !key.contains(':')) {
            answers.insert(key.clone(), json!({"noul": 0.5}));
        }
        answers.insert(
            "outcome".into(),
            json!({"probabilities": {"failure": 0.2, "partial": 0.3}}),
        );
        let score = fuse(&set, &json!({"answers": answers})).unwrap();
        let expected =
            1.0 / (1.0 + (-set.score.bias - set.score.weights.values().sum::<f64>() * 0.5).exp());
        assert!((score - expected).abs() < 1e-12);
        let text = "€".repeat(2000);
        assert!(tail(&text).len() <= 4096);
    }

    #[test]
    fn delegate_s1_env_urls_override_config_without_changing_timeout() {
        let config = SystemOneConfig {
            urls: vec!["http://config.invalid".into()],
            timeout_s: 0.2,
            ..SystemOneConfig::default()
        };
        let client = Client::new(
            Some("http://one.invalid, http://two.invalid\nhttp://three.invalid"),
            &config,
            PathBuf::new(),
        );
        assert_eq!(client.urls.len(), 3);
        assert_eq!(client.timeout, Duration::from_millis(200));
        assert!(Client::new(Some(""), &config, PathBuf::new())
            .urls
            .is_empty());
    }

    #[test]
    fn delegate_s1_full_tier_uses_its_own_urls_and_timeout() {
        let mut config = SystemOneConfig {
            urls: vec!["http://fast.invalid".into()],
            full_urls: vec!["http://full.invalid".into()],
            ..SystemOneConfig::default()
        };
        let fast = Client::new(None, &config, PathBuf::new());
        assert_eq!(fast.urls, ["http://fast.invalid"]);
        assert_eq!(fast.timeout, Duration::from_secs(4));
        config.tier = SystemOneTier::Full;
        let full = Client::new(None, &config, PathBuf::new());
        assert_eq!(full.urls, ["http://full.invalid"]);
        assert_eq!(full.timeout, Duration::from_secs(12));
        let env = Client::new(Some("http://env.invalid"), &config, PathBuf::new());
        assert_eq!(env.urls, ["http://env.invalid"]);
        let parsed: SystemOneConfig =
            toml::from_str("tier = \"full\"\nfull_urls = [\"http://full.invalid\"]").unwrap();
        assert_eq!(parsed.tier, SystemOneTier::Full);
        assert_eq!(parsed.timeout_s, 4.0);
    }

    fn answers(shape: impl Fn(&str) -> Value) -> serde_json::Map<String, Value> {
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).unwrap();
        set.questions
            .as_object()
            .unwrap()
            .keys()
            .map(|name| (name.clone(), shape(name)))
            .collect()
    }

    #[test]
    fn delegate_s1_reads_only_answers_from_kev_and_clef_shaped_responses() {
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).unwrap();
        let kev = json!({
            "model": "kev-latest",
            "answers": answers(|name| if name == "outcome" {
                json!({"probabilities": {"success": 0.05, "failure": 0.85, "partial": 0.05, "info": 0.05}})
            } else {
                json!({"noul": 0.9})
            }),
            "usage": {"input_tokens": 2184},
        });
        // Field for field what clef-flash on :8023 answered on 2026-10-11.
        let clef = json!({
            "model": "s1-fast",
            "answers": answers(|name| if name == "outcome" {
                json!({"type": "choice", "choice": "failure", "probabilities":
                    {"success": 0.05, "failure": 0.85, "partial": 0.05, "info": 0.05},
                    "confidence": 0.66})
            } else {
                json!({"type": "noul", "noul": 0.9})
            }),
            "usage": {"input_tokens": 2184, "output_tokens": 0},
            "prompt_eval_cached_count": 2184,
        });
        let kev = interpret(&set, 200, kev.to_string().as_bytes()).unwrap();
        let clef = interpret(&set, 200, clef.to_string().as_bytes()).unwrap();
        assert!(kev > 0.5);
        assert!((kev - clef).abs() < 1e-12);
        let mut partial = answers(|_| json!({"type": "noul", "noul": 0.9}));
        partial.remove("red");
        assert_eq!(
            interpret(
                &set,
                200,
                json!({"answers": partial}).to_string().as_bytes()
            ),
            Err(Failure::Unanswered)
        );
        assert_eq!(interpret(&set, 200, b"not json"), Err(Failure::Unanswered));
    }

    #[test]
    fn delegate_s1_names_rejections_but_not_outages_and_only_outages_trip_the_breaker() {
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).unwrap();
        let clef = br#"{"error":"Clef input has 17097 tokens; context limit is 16384"}"#;
        let failure = interpret(&set, 400, clef).unwrap_err();
        assert_eq!(
            failure.report("http://s1.invalid").unwrap(),
            "flk delegate: System One at http://s1.invalid rejected the request (HTTP 400): \
             Clef input has 17097 tokens; context limit is 16384"
        );
        assert!(!failure.counts_against_breaker());
        let kev = br#"{"error":{"detail":"state too long"}}"#;
        assert!(matches!(
            interpret(&set, 422, kev),
            Err(Failure::Rejected { status: 422, error }) if error.contains("state too long")
        ));
        assert!(matches!(
            interpret(&set, 413, b"too large"),
            Err(Failure::Rejected { status: 413, error }) if error == "too large"
        ));
        assert!(!Failure::Unanswered.counts_against_breaker());
        for (status, expected) in [
            (408, Failure::Busy),
            (429, Failure::Busy),
            (502, Failure::Down),
            (503, Failure::Down),
        ] {
            let failure = interpret(&set, status, b"").unwrap_err();
            assert_eq!(failure, expected);
            assert!(failure.report("u").is_none());
            assert_eq!(failure.counts_against_breaker(), expected == Failure::Down);
        }
    }

    #[test]
    fn delegate_s1_curl_timeout_after_connecting_is_busy_not_down() {
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).unwrap();
        // curl 8 prints these trailers for: response timeout after connecting,
        // refused connection, and connect timeout.
        assert_eq!(outcome(&set, false, b"\n000 1"), Err(Failure::Busy));
        assert_eq!(outcome(&set, false, b"\n000 0"), Err(Failure::Down));
        assert_eq!(outcome(&set, false, b""), Err(Failure::Down));
        assert_eq!(outcome(&set, true, b"garbage"), Err(Failure::Down));
        let body = json!({"answers": answers(|name| if name == "outcome" {
            json!({"probabilities": {"failure": 0.9, "partial": 0.1}})
        } else {
            json!({"noul": 0.9})
        })});
        let stdout = format!("{:#}\n200 1", body);
        assert!(stdout.matches('\n').count() > 1);
        assert!(outcome(&set, true, stdout.as_bytes()).unwrap() > 0.5);
    }

    #[test]
    fn delegate_s1_breaker_skips_an_endpoint_in_flight_and_after_three_outages() {
        let dir = std::env::temp_dir().join(format!(
            "delegate_s1_breaker_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let timeout = Duration::from_secs(4);
        let held = Breaker::acquire(&dir, "http://s1.invalid", timeout).unwrap();
        assert!(Breaker::acquire(&dir, "http://s1.invalid", timeout).is_none());
        assert!(Breaker::acquire(&dir, "http://other.invalid", timeout).is_some());
        drop(held);
        for _ in 0..3 {
            Breaker::acquire(&dir, "http://s1.invalid", timeout)
                .unwrap()
                .record(false);
        }
        assert!(Breaker::acquire(&dir, "http://s1.invalid", timeout).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
