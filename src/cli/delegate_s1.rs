//! Event-time, advisory System One client. No endpoint is compiled in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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

pub(super) fn tail(screen: &str) -> &str {
    let mut start = screen.len().saturating_sub(4096);
    while !screen.is_char_boundary(start) {
        start += 1;
    }
    &screen[start..]
}

pub(super) struct Client {
    urls: Vec<String>,
    timeout: Duration,
    state_dir: PathBuf,
}

impl Client {
    pub fn from_config() -> Self {
        let config = crate::config::Config::load().config.systemone;
        Self::new(
            std::env::var("SYSTEMONE_URL").ok().as_deref(),
            &config,
            crate::config::state_dir().join("systemone"),
        )
    }

    fn new(
        env: Option<&str>,
        config: &crate::config::model::SystemOneConfig,
        state_dir: PathBuf,
    ) -> Self {
        let urls = match env {
            Some(env) => env
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|url| !url.is_empty())
                .map(str::to_string)
                .collect(),
            None => config.urls.clone(),
        };
        let timeout = Duration::try_from_secs_f64(config.timeout_s)
            .ok()
            .filter(|duration| !duration.is_zero())
            .unwrap_or(Duration::from_secs(3));
        Self {
            urls,
            timeout,
            state_dir,
        }
    }

    // Called by event emission only. The total budget covers ordered failover.
    pub fn judge(&self, command: &str, screen: &str) -> Option<Advice> {
        if self.urls.is_empty() {
            return None;
        }
        let set: Questions = toml::from_str(include_str!("delegate_s1_questions.toml")).ok()?;
        let body = serde_json::to_vec(&json!({
            "state": format!("Command: {command}\n(The exit status is not shown.)\nLast output:\n{}", tail(screen)),
            "questions": set.questions,
        })).ok()?;
        let deadline = Instant::now().checked_add(self.timeout)?;
        for url in &self.urls {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                continue;
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let Some(mut breaker) = Breaker::acquire(&self.state_dir, url, self.timeout) else {
                continue;
            };
            let response = crate::process::TracedCommand::new("curl", "delegate_s1")
                .args([
                    "--disable",
                    "--silent",
                    "--show-error",
                    "--fail",
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
                .output_traced_with_stdin_and_timeout(&body, remaining)
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
                .and_then(|response| fuse(&set, &response));
            breaker.record(response.is_some());
            if let Some(fused) = response {
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
        let config = crate::config::model::SystemOneConfig {
            urls: vec!["http://config.invalid".into()],
            timeout_s: 0.2,
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
}
