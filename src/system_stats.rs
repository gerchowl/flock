//! Background sampler for the global status line: cpu, memory, disk space,
//! battery, network throughput, and best-effort GPU utilization. Metrics
//! that cannot be read on the current platform stay `None` and the status
//! line simply omits them.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SystemStats {
    /// Short hostname, shown as the first status-line element.
    pub host: Option<String>,
    /// Global CPU utilization, 0..=100.
    pub cpu_percent: Option<f32>,
    /// Used / total memory in bytes.
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    /// Free space on the volume holding $HOME, in bytes.
    pub disk_free: Option<u64>,
    /// Battery charge 0..=100 and whether we are on AC.
    pub battery_percent: Option<u8>,
    pub battery_charging: Option<bool>,
    /// Network throughput since the previous sample, bytes/sec.
    pub net_rx_per_sec: Option<u64>,
    pub net_tx_per_sec: Option<u64>,
    /// Best-effort GPU utilization, 0..=100 (macOS IOAccelerator).
    pub gpu_percent: Option<u8>,
    /// Host-declared thermal health (#291), gossiped to the fleet and rendered
    /// as a tint on the band's metric glyphs. Filled from the node's own
    /// `thermal_command` (#298) on a slow tick. Nothing here reads a sensor —
    /// the host owns calibration, flock owns transport and colour.
    pub thermal: Option<crate::api::schema::ThermalReport>,
}

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// Wall-clock bound on a sampler subprocess.
///
/// `pmset` and `ioreg` run every SAMPLE_INTERVAL, forever. Unbounded, one hung
/// call wedges the sampler thread and the status line then renders its last
/// values as current indefinitely — a confident lie on the surface users read
/// most. Deliberately shorter than the interval so a slow sample is dropped
/// rather than overlapping the next one.
const SAMPLER_EXEC_TIMEOUT: Duration = Duration::from_millis(1500);

/// Wall-clock bound on one thermal reporter run (#298). Longer than
/// [`SAMPLER_EXEC_TIMEOUT`] because a reporter may legitimately shell out to
/// `nvidia-smi`, which re-initialises NVML at ~200-800ms — but still well
/// under the thermal cadence, so a slow reporter cannot overlap its own next
/// run.
const THERMAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Sampler ticks between thermal reads (#298). Temperature moves slowly and a
/// reporter forks a process — `nvidia-smi` alone re-initialises NVML at
/// ~200-800ms — so it must NOT ride the 2s cadence the cheap in-process
/// metrics use. 15 ticks ≈ 30s.
const THERMAL_STRIDE: u32 = 15;

/// Consecutive reporter failures before the WARN fires. A single failed read
/// is usually transient (fork contention, a device wake); complaining at the
/// first one would be noise.
const THERMAL_FAILURES_BEFORE_WARN: u32 = 5;

/// Ceiling on the backoff a failing reporter is pushed to, in sampler ticks.
/// Never disabled permanently — `nvidia-smi` returns after a driver restart
/// and `pmset` after a device wake, and a box that recovers should light back
/// up without a flock restart.
const THERMAL_MAX_BACKOFF_TICKS: u32 = 150;

/// Wall-clock bound on one `gpu_command` run. The same budget pmset/ioreg get
/// — a reporter at this cadence must be a cheap read (a file published by the
/// host, a one-shot NVML query), not a tool that re-initialises its library
/// each call.
const GPU_COMMAND_TIMEOUT: Duration = SAMPLER_EXEC_TIMEOUT;

/// Consecutive `gpu_command` failures before the WARN fires. Matches thermal:
/// a lone failure is usually a device wake or fork contention, not news.
const GPU_COMMAND_FAILURES_BEFORE_WARN: u32 = 5;

/// Ceiling on the backoff a failing `gpu_command` is pushed to, in sampler
/// ticks. Same floor as thermal: a reporter that recovers (file reappears,
/// driver restarts) must light the column back up without a flock restart.
const GPU_COMMAND_MAX_BACKOFF_TICKS: u32 = 150;

/// Resolve which path's volume the disk stat reports (#50): the configured
/// `ui.disk_path` when set (any path — its containing mount is matched), else
/// `$HOME`'s volume (the historical default). Empty/whitespace is treated as
/// unset. `None` only when neither is available, which omits the disk metric.
pub(crate) fn resolve_disk_target(disk_path: Option<&str>) -> Option<std::path::PathBuf> {
    disk_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
}

/// Spawn the sampler thread; it sends a snapshot through `notify` every
/// interval until the receiver disappears. `disk_path` (`ui.disk_path`) picks
/// the volume the disk metric reports; `None` keeps the `$HOME` default.
pub fn spawn_sampler(
    event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    disk_path: Option<String>,
    thermal_command: Option<String>,
    gpu_command: Option<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("system-stats".into())
        .spawn(move || {
            let mut system = sysinfo::System::new();
            let mut networks = sysinfo::Networks::new_with_refreshed_list();
            let disks = sysinfo::Disks::new_with_refreshed_list();
            let disk_target = resolve_disk_target(disk_path.as_deref());
            // First CPU sample needs a baseline.
            system.refresh_cpu_usage();
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);

            let mut disks = disks;
            let mut thermal = ThermalSampler::new(thermal_command);
            let mut gpu_cmd = GpuCommandSampler::new(gpu_command);
            let mut tick: u32 = 0;
            loop {
                system.refresh_cpu_usage();
                system.refresh_memory();
                disks.refresh(true);
                let elapsed = SAMPLE_INTERVAL.as_secs_f64();
                networks.refresh(true);

                let cpu_percent = Some(system.global_cpu_usage());
                let mem_total = Some(system.total_memory());
                let mem_used = Some(system.used_memory());

                let disk_free = disk_target.as_deref().and_then(|target| {
                    disks
                        .iter()
                        .filter(|disk| target.starts_with(disk.mount_point()))
                        .max_by_key(|disk| disk.mount_point().as_os_str().len())
                        .map(|disk| disk.available_space())
                });

                let (mut rx, mut tx) = (0u64, 0u64);
                for data in networks.values() {
                    rx = rx.saturating_add(data.received());
                    tx = tx.saturating_add(data.transmitted());
                }
                let net_rx_per_sec = Some((rx as f64 / elapsed) as u64);
                let net_tx_per_sec = Some((tx as f64 / elapsed) as u64);

                let (battery_percent, battery_charging) = read_battery();
                // A configured `gpu_command` is the whole GPU reading on this
                // node: #291's reason to add this field is a microVM guest
                // publishing its host's number, and a macOS host that sets it
                // deliberately prefers the host-authored value over its own
                // IOAccelerator sample. The local sampler is NOT a fallback —
                // a failed command declares `None`, not a synthesized reading.
                let gpu_percent = if gpu_cmd.is_set() {
                    gpu_cmd.sample()
                } else {
                    read_gpu_percent()
                };
                let thermal_report = thermal.sample(tick);
                tick = tick.wrapping_add(1);

                let stats = SystemStats {
                    host: Some(crate::app::short_host_name()),
                    cpu_percent,
                    mem_used,
                    mem_total,
                    disk_free,
                    battery_percent,
                    battery_charging,
                    net_rx_per_sec,
                    net_tx_per_sec,
                    gpu_percent,
                    thermal: thermal_report,
                };
                if event_tx
                    .blocking_send(crate::events::AppEvent::SystemStatsUpdated(stats))
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(SAMPLE_INTERVAL);
            }
        })
        .expect("system stats sampler thread should spawn")
}

/// Runs the node's configured `thermal_command` on a slow tick and turns its
/// stdout into a [`ThermalReport`](crate::api::schema::ThermalReport) (#298).
///
/// flock reads no sensors. The host declares what runs hot and how hot counts,
/// because the answer differs per box — 90 °C is normal on Apple Silicon under
/// load, an RTX 5090 is fine at 80 °C, and a microVM guest can read nothing at
/// all. This type only runs the command, bounds it, and parses it.
struct ThermalSampler {
    command: Option<String>,
    /// Last successful report, held for exactly one thermal tick after a
    /// failure so a single fluke does not make a hot node's tint flicker off.
    last: Option<crate::api::schema::ThermalReport>,
    /// Thermal ticks a held value has survived. Past 1 the value is dropped.
    held_ticks: u32,
    /// Consecutive failures, for the WARN threshold and the backoff.
    failures: u32,
    /// Sampler ticks to skip before trying again — a failing reporter is
    /// slowed down, never disabled.
    backoff_ticks: u32,
}

impl ThermalSampler {
    fn new(command: Option<String>) -> Self {
        let command = command
            .map(|command| command.trim().to_string())
            .filter(|command| !command.is_empty());
        Self {
            command,
            last: None,
            held_ticks: 0,
            failures: 0,
            backoff_ticks: 0,
        }
    }

    /// The value to publish on sampler tick `tick`. Between thermal ticks this
    /// repeats the last reading rather than blanking it — the status line
    /// samples every 2s and the reporter every ~30s.
    fn sample(&mut self, tick: u32) -> Option<crate::api::schema::ThermalReport> {
        self.command.as_ref()?;
        if !tick.is_multiple_of(THERMAL_STRIDE) {
            return self.last.clone();
        }
        if self.backoff_ticks > 0 {
            self.backoff_ticks = self.backoff_ticks.saturating_sub(THERMAL_STRIDE);
            return self.last.clone();
        }
        match self.run() {
            Some(report) => {
                self.failures = 0;
                self.backoff_ticks = 0;
                self.held_ticks = 0;
                self.last = Some(report);
            }
            None => {
                self.failures = self.failures.saturating_add(1);
                if self.failures == THERMAL_FAILURES_BEFORE_WARN {
                    tracing::warn!(
                        failures = self.failures,
                        "thermal_command keeps failing; backing off (node declares no thermal health)"
                    );
                }
                // Grace: one thermal tick of the stale value, then nothing.
                // Never synthesize a nominal reading from a failed read — that
                // asserts health nobody observed, which is worst precisely
                // when the node is critical and the tint is the whole point.
                if self.last.is_some() && self.held_ticks == 0 {
                    self.held_ticks = 1;
                } else {
                    self.last = None;
                    self.held_ticks = 0;
                }
                if self.failures >= THERMAL_FAILURES_BEFORE_WARN {
                    let grown = self.backoff_ticks.saturating_mul(2).max(THERMAL_STRIDE * 2);
                    self.backoff_ticks = grown.min(THERMAL_MAX_BACKOFF_TICKS);
                }
            }
        }
        self.last.clone()
    }

    /// One bounded run. Every failure mode collapses to `None`: a node that
    /// cannot measure declares nothing.
    fn run(&self) -> Option<crate::api::schema::ThermalReport> {
        let command = self.command.as_deref()?;
        let output = crate::process::TracedCommand::new("sh", "stats")
            .args(["-c", command])
            .periodic()
            .output_traced_with_timeout(THERMAL_TIMEOUT)
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_thermal_report(&output.stdout)
    }
}

/// Runs the node's configured `gpu_command` on every sampler tick and turns
/// its stdout into a 0..=100 utilization reading (#291 companion to #298).
///
/// The thermal reporter uses a slow stride because its command is slow
/// (`nvidia-smi` re-initialises NVML at ~200-800ms) and the value it reads
/// moves slowly. Utilization moves as fast as CPU and memory; a 30s refresh
/// would be visibly stale next to the two-second CPU column. The expected
/// reporter is a cheap read of a host-published file (one `read` + `trim`),
/// so running it on every tick is in budget — and a reporter that is slow or
/// starts failing is slowed down by the same backoff thermal uses.
struct GpuCommandSampler {
    command: Option<String>,
    /// Consecutive failures, for the WARN threshold and the backoff.
    failures: u32,
    /// Sampler ticks to skip before trying again — a failing reporter is
    /// slowed down, never disabled.
    backoff_ticks: u32,
}

impl GpuCommandSampler {
    fn new(command: Option<String>) -> Self {
        let command = command
            .map(|command| command.trim().to_string())
            .filter(|command| !command.is_empty());
        Self {
            command,
            failures: 0,
            backoff_ticks: 0,
        }
    }

    /// `true` iff the node declared a `gpu_command`. When true, this sampler
    /// is the ONLY source of `gpu_percent` — the local IOAccelerator read is
    /// not a fallback (see call site).
    fn is_set(&self) -> bool {
        self.command.is_some()
    }

    /// The utilization to publish on this tick. `None` for an unset command,
    /// a backed-off reporter, or any run that fails parse/exit/timeout — the
    /// same "declare nothing on any failure" rule the thermal reporter uses.
    fn sample(&mut self) -> Option<u8> {
        let command = self.command.as_deref()?;
        if self.backoff_ticks > 0 {
            self.backoff_ticks = self.backoff_ticks.saturating_sub(1);
            return None;
        }
        match self.run(command) {
            Some(value) => {
                self.failures = 0;
                self.backoff_ticks = 0;
                Some(value)
            }
            None => {
                self.failures = self.failures.saturating_add(1);
                if self.failures == GPU_COMMAND_FAILURES_BEFORE_WARN {
                    tracing::warn!(
                        failures = self.failures,
                        "gpu_command keeps failing; backing off (node declares no GPU utilization)"
                    );
                }
                if self.failures >= GPU_COMMAND_FAILURES_BEFORE_WARN {
                    let grown = self.backoff_ticks.saturating_mul(2).max(2);
                    self.backoff_ticks = grown.min(GPU_COMMAND_MAX_BACKOFF_TICKS);
                }
                None
            }
        }
    }

    /// One bounded run. Every failure mode collapses to `None` so a node that
    /// cannot measure declares nothing instead of a synthesized number.
    fn run(&self, command: &str) -> Option<u8> {
        let output = crate::process::TracedCommand::new("sh", "stats")
            .args(["-c", command])
            .periodic()
            .output_traced_with_timeout(GPU_COMMAND_TIMEOUT)
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_gpu_command_output(&output.stdout)
    }
}

/// Parse a `gpu_command` reporter's stdout: a single integer 0..=100 with
/// surrounding whitespace trimmed. Anything else — a float, a trailing unit,
/// a value past 100 — is a reporter bug on this contract and declares
/// `None`, matching `parse_thermal_report` sanitation.
fn parse_gpu_command_output(stdout: &[u8]) -> Option<u8> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<u8>().ok().filter(|value| *value <= 100)
}

/// Parse a reporter's stdout: exactly one JSON object, nothing else.
///
/// JSON rather than a delimited line because labels legitimately carry spaces
/// ("fans 45m"), and shell authors get quoting rules wrong. Sanitized here so
/// the invariant travels with the value from the moment it enters the process.
fn parse_thermal_report(stdout: &[u8]) -> Option<crate::api::schema::ThermalReport> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str::<crate::api::schema::ThermalReport>(text)
        .ok()
        .map(crate::api::schema::ThermalReport::sanitized)
}

/// macOS: `pmset -g batt`. Other platforms: None (omitted from the line).
fn read_battery() -> (Option<u8>, Option<bool>) {
    if !cfg!(target_os = "macos") {
        return (None, None);
    }
    let Ok(output) = crate::process::TracedCommand::new("pmset", "stats")
        .args(["-g", "batt"])
        .periodic()
        .output_traced_with_timeout(SAMPLER_EXEC_TIMEOUT)
    else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let percent = text
        .split_whitespace()
        .find_map(|token| token.strip_suffix("%;").or_else(|| token.strip_suffix('%')))
        .and_then(|token| token.parse::<u8>().ok());
    let charging = if text.contains("AC Power") {
        Some(true)
    } else if text.contains("Battery Power") {
        Some(false)
    } else {
        None
    };
    (percent, charging)
}

/// macOS best effort: IOAccelerator "Device Utilization %" via ioreg. Needs
/// no privileges on Apple Silicon; anything unparseable yields None.
fn read_gpu_percent() -> Option<u8> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = crate::process::TracedCommand::new("ioreg", "stats")
        .args(["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"])
        .periodic()
        .output_traced_with_timeout(SAMPLER_EXEC_TIMEOUT)
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_gpu_utilization(&text)
}

fn parse_gpu_utilization(ioreg_text: &str) -> Option<u8> {
    let idx = ioreg_text.find("\"Device Utilization %\"")?;
    let rest = &ioreg_text[idx..];
    let eq = rest.find('=')?;
    let value: String = rest[eq + 1..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    value.parse::<u8>().ok().filter(|v| *v <= 100)
}

/// Compact human formatting for the status line: bytes -> "312G", "1.4M".
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 || unit == 0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_disk_target_prefers_configured_path() {
        assert_eq!(
            resolve_disk_target(Some("/data")),
            Some(std::path::PathBuf::from("/data"))
        );
        assert_eq!(
            resolve_disk_target(Some("/")),
            Some(std::path::PathBuf::from("/"))
        );
    }

    #[test]
    fn resolve_disk_target_falls_back_to_home_when_unset_or_blank() {
        // Unset and blank both defer to $HOME (the historical default).
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        assert_eq!(resolve_disk_target(None), home);
        assert_eq!(resolve_disk_target(Some("   ")), home);
    }

    #[test]
    fn parses_gpu_utilization_from_ioreg_block() {
        let text = r#"
    "PerformanceStatistics" = {"Device Utilization %"=37,"Renderer Utilization %"=35}
"#;
        assert_eq!(parse_gpu_utilization(text), Some(37));
    }

    #[test]
    fn gpu_parse_rejects_garbage() {
        assert_eq!(parse_gpu_utilization("no gpu here"), None);
        assert_eq!(parse_gpu_utilization("\"Device Utilization %\"=x"), None);
    }

    fn report(json: &str) -> Option<crate::api::schema::ThermalReport> {
        parse_thermal_report(json.as_bytes())
    }

    #[test]
    fn parses_a_reporter_json_line_and_sanitizes_it() {
        use crate::api::schema::{ThermalComponent, THERMAL_LABEL_MAX_BYTES, THERMAL_SEVERITY_MAX};

        let ok = report(r#"{"severity":2,"component":"gpu","label":"GPU 84"}"#).unwrap();
        assert_eq!(ok.severity, 2);
        assert_eq!(ok.component, ThermalComponent::Gpu);
        assert_eq!(ok.label, "GPU 84");

        // Trailing newline is what `printf`/`echo` actually emit.
        assert!(report("{\"severity\":1,\"component\":\"cpu\"}\n").is_some());

        // A broken reporter is clamped here, not trusted downstream.
        let wild = report(r#"{"severity":99,"component":"cpu","label":"aaaaaaaaaaaaaaaaaaaaaaa"}"#)
            .unwrap();
        assert_eq!(wild.severity, THERMAL_SEVERITY_MAX);
        assert_eq!(wild.label.len(), THERMAL_LABEL_MAX_BYTES);
    }

    #[test]
    fn garbage_reporter_output_declares_nothing() {
        // Fail closed on every shape of nonsense. Never a synthesized nominal:
        // asserting health nobody measured is worst exactly when the node is
        // critical and the tint is the whole point.
        assert!(report("").is_none());
        assert!(report("   \n").is_none());
        assert!(report("72°C").is_none());
        assert!(report("{").is_none());
        assert!(
            report(r#"{"component":"cpu"}"#).is_none(),
            "severity required"
        );
        assert!(
            report(r#"{"severity":2,"component":"toaster"}"#).is_none(),
            "unknown component must not parse"
        );
        assert!(
            parse_thermal_report(&[0xff, 0xfe]).is_none(),
            "invalid utf-8"
        );
    }

    #[test]
    fn no_configured_command_never_declares_anything() {
        let mut sampler = ThermalSampler::new(None);
        for tick in 0..40 {
            assert!(sampler.sample(tick).is_none());
        }
        // Whitespace-only is treated as unset, like `ui.disk_path`.
        let mut blank = ThermalSampler::new(Some("   ".into()));
        assert!(blank.sample(0).is_none());
        assert!(blank.command.is_none());
    }

    #[test]
    fn reporter_runs_on_the_slow_tick_and_repeats_between() {
        // The status line samples every 2s; forking a reporter that often is
        // exactly what the stride exists to prevent.
        let mut sampler = ThermalSampler::new(Some(
            r#"printf '{"severity":3,"component":"cpu","label":"hot"}'"#.into(),
        ));
        let first = sampler.sample(0).expect("stride tick reads");
        assert_eq!(first.severity, 3);
        for tick in 1..THERMAL_STRIDE {
            assert_eq!(
                sampler.sample(tick).map(|r| r.severity),
                Some(3),
                "between strides the last reading repeats"
            );
        }
        assert!(sampler.sample(THERMAL_STRIDE).is_some());
    }

    #[test]
    fn a_failing_reporter_holds_one_tick_then_declares_nothing() {
        let mut sampler = ThermalSampler::new(Some("exit 1".into()));
        // Seed a good reading, as if the box was healthy a moment ago.
        sampler.last = Some(crate::api::schema::ThermalReport {
            severity: 3,
            component: crate::api::schema::ThermalComponent::Cpu,
            label: "hot".into(),
        });

        // One thermal tick of grace: a single fluke must not flicker the tint
        // off a genuinely hot box.
        assert!(sampler.sample(0).is_some(), "first failure holds the value");
        // Then it goes, rather than lingering as an unobserved claim.
        assert!(
            sampler.sample(THERMAL_STRIDE).is_none(),
            "a stale reading must not outlive its grace tick"
        );
        assert_eq!(sampler.failures, 2);
    }

    #[test]
    fn a_persistently_failing_reporter_backs_off_but_is_never_disabled() {
        let mut sampler = ThermalSampler::new(Some("exit 1".into()));
        let mut tick = 0u32;
        for _ in 0..THERMAL_FAILURES_BEFORE_WARN {
            sampler.sample(tick);
            tick = tick.wrapping_add(THERMAL_STRIDE);
        }
        assert!(sampler.backoff_ticks > 0, "should have backed off");
        assert!(
            sampler.backoff_ticks <= THERMAL_MAX_BACKOFF_TICKS,
            "backoff is capped so a recovered box lights up again without a restart"
        );

        // A reporter that starts working again clears the backoff — drivers
        // restart, devices wake.
        sampler.command = Some(r#"printf '{"severity":1,"component":"cpu"}'"#.into());
        sampler.backoff_ticks = 0;
        assert!(sampler.sample(tick).is_some());
        assert_eq!(sampler.failures, 0);
        assert_eq!(sampler.backoff_ticks, 0);
    }

    #[test]
    fn human_bytes_formats_compactly() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1500), "1.5K");
        assert_eq!(human_bytes(18 * 1024 * 1024 * 1024), "18G");
    }

    fn gpu_value(stdout: &str) -> Option<u8> {
        parse_gpu_command_output(stdout.as_bytes())
    }

    #[test]
    fn parses_a_gpu_reporter_integer_and_trims_whitespace() {
        assert_eq!(gpu_value("0"), Some(0));
        assert_eq!(gpu_value("37"), Some(37));
        assert_eq!(gpu_value("100"), Some(100));
        // Trailing newline is what `printf '%d\n'`/`echo` actually emit.
        assert_eq!(gpu_value("42\n"), Some(42));
        // Leading whitespace is tolerated too — a file written with a BOM-less
        // utility often has a leading space.
        assert_eq!(gpu_value("  12  \n"), Some(12));
    }

    #[test]
    fn garbage_gpu_reporter_output_declares_nothing() {
        // Fail closed on every shape of nonsense. The servers band would
        // rather show no number than a confident lie.
        assert!(gpu_value("").is_none());
        assert!(gpu_value("   \n").is_none());
        assert!(gpu_value("gpu-busy").is_none());
        assert!(
            gpu_value("42%").is_none(),
            "trailing unit is a reporter bug"
        );
        assert!(
            gpu_value("37.5").is_none(),
            "float is not an integer 0..=100"
        );
        assert!(gpu_value("-1").is_none(), "u8 rejects negatives");
        assert!(gpu_value("101").is_none(), "out of range");
        assert!(gpu_value("256").is_none(), "overflow");
        assert!(
            parse_gpu_command_output(&[0xff, 0xfe]).is_none(),
            "invalid utf-8"
        );
    }

    #[test]
    fn no_configured_gpu_command_never_declares_anything() {
        // Mirrors the thermal case: an unset node must not run anything and
        // must not pretend to.
        let mut sampler = GpuCommandSampler::new(None);
        assert!(!sampler.is_set());
        for _ in 0..40 {
            assert!(sampler.sample().is_none());
        }
        // Whitespace-only is treated as unset, like `ui.disk_path`.
        let blank = GpuCommandSampler::new(Some("   ".into()));
        assert!(!blank.is_set());
        assert!(blank.command.is_none());
    }

    #[test]
    fn a_working_gpu_reporter_reports_each_tick() {
        // The whole point of putting this on the fast tick instead of the
        // thermal stride is that utilization refreshes at the CPU cadence.
        let mut sampler = GpuCommandSampler::new(Some("printf 73".into()));
        assert!(sampler.is_set());
        for _ in 0..5 {
            assert_eq!(sampler.sample(), Some(73));
        }
        assert_eq!(sampler.failures, 0);
        assert_eq!(sampler.backoff_ticks, 0);
    }

    #[test]
    fn a_failing_gpu_reporter_declares_nothing_and_eventually_backs_off() {
        let mut sampler = GpuCommandSampler::new(Some("exit 1".into()));
        for _ in 0..GPU_COMMAND_FAILURES_BEFORE_WARN {
            assert!(sampler.sample().is_none());
        }
        assert!(
            sampler.backoff_ticks > 0,
            "a persistently failing reporter must be slowed down"
        );
        assert!(
            sampler.backoff_ticks <= GPU_COMMAND_MAX_BACKOFF_TICKS,
            "backoff is capped so a recovered reporter lights up again without a restart"
        );

        // A reporter that starts working again clears the backoff — files
        // reappear, drivers restart.
        sampler.command = Some("printf 10".into());
        sampler.backoff_ticks = 0;
        assert_eq!(sampler.sample(), Some(10));
        assert_eq!(sampler.failures, 0);
        assert_eq!(sampler.backoff_ticks, 0);
    }

    #[test]
    fn an_out_of_range_gpu_reporter_declares_nothing_not_a_clamp() {
        // Clamping would mask a reporter bug AND prefix a believable number
        // onto garbage; `parse_thermal_report` does the same thing by
        // sanitising rather than clamping free-form values.
        let mut sampler = GpuCommandSampler::new(Some("printf 999".into()));
        assert!(sampler.sample().is_none());
        assert_eq!(sampler.failures, 1);
    }
}
