//! The switch status surface for the DEFAULT (non-slots) path (#282).
//!
//! `[slots] enabled` is false by default, so an ordinary server switch does not
//! flip an in-process slot. The client exits, and the launcher runs a whole
//! fresh leg — remote binary probe, remote server check, ssh bridge, client
//! attach — while the host terminal is deliberately held on the PREVIOUS
//! server's frozen frame so nothing flashes between legs (#63).
//!
//! That hold is what the "switching between servers is stale and slow" report
//! is actually describing. It is doing its job perfectly: the screen is
//! seamless. It is also, for the entire duration of a cold leg, a confident
//! picture of a server the user has already left, with nothing to say where
//! they are going, how long it has been, or whether anything is wrong. A 5s
//! ssh probe is indistinguishable from a hang.
//!
//! So this paints over the held frame while the leg establishes: destination,
//! elapsed seconds, and the stage currently running. It is deliberately a
//! BACKGROUND thread — the preflight it reports on is a sequence of blocking
//! ssh calls, and a surface that can only repaint between them would freeze at
//! exactly the moment it has something to say.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Repaint cadence. The only thing that changes between frames is the elapsed
/// counter and (rarely) the stage, so this is about the counter looking alive
/// rather than about latency.
const REPAINT_INTERVAL: Duration = Duration::from_millis(250);

/// Wait this long before painting anything at all.
///
/// A warm switch finishes well inside this, and flashing a "switching…" box up
/// and instantly away is worse than showing nothing — it reads as a glitch. The
/// surface exists for the SLOW case, so it should only appear once a switch has
/// declared itself slow.
const APPEAR_AFTER: Duration = Duration::from_millis(400);

/// Elapsed thresholds for the tone ramp, matching the slots popup so the two
/// surfaces read as one feature.
const YELLOW_AT: Duration = Duration::from_secs(3);
const UNRESPONSIVE_AT: Duration = Duration::from_secs(10);

/// Which preflight leg is running, as a number the painter thread can read
/// without a lock. Ordered by the sequence the launcher runs them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Resolving (and possibly installing) the remote flock binary — one or
    /// more full ssh round trips, and the usual culprit on a slow switch.
    Probing = 0,
    /// Making sure the remote server is up and accepting clients.
    StartingServer = 1,
    /// Standing up the local ssh-stdio bridge.
    Connecting = 2,
    /// Handing off to the client process, which repaints and takes the screen.
    Attaching = 3,
}

impl Stage {
    /// Kept short ON PURPOSE. The notice box is 50 columns wide and shared with
    /// the slots popup, leaving 46 for text — and the overdue variant appends
    /// to this. A probe caught the first draft ("finding flock on the remote
    /// host — taking longer than usual") being cut to "…taking long", losing
    /// the escalation at the exact moment it was the point.
    fn label(self) -> &'static str {
        match self {
            Stage::Probing => "finding flock on the host",
            Stage::StartingServer => "waiting for remote server",
            Stage::Connecting => "opening the ssh tunnel",
            Stage::Attaching => "attaching",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Stage::StartingServer,
            2 => Stage::Connecting,
            3 => Stage::Attaching,
            _ => Stage::Probing,
        }
    }
}

/// A live switch surface. Dropping it stops the painter and joins its thread,
/// so the client process can never start writing while the painter is mid-box.
pub(crate) struct SwitchProgress {
    stop: Arc<AtomicBool>,
    stage: Arc<AtomicU8>,
    painter: Option<std::thread::JoinHandle<()>>,
    /// Whether the painter ever drew anything. A fast switch never crosses
    /// `APPEAR_AFTER`, and in that case there is nothing to erase.
    painted: Arc<AtomicBool>,
}

impl SwitchProgress {
    /// Start reporting a switch to `destination`.
    ///
    /// `destination` is what the user asked for, spelled the way they asked for
    /// it — the ssh target. Prettifying it into a short host name here would
    /// disagree with the sidebar row they clicked.
    pub(crate) fn start(destination: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stage = Arc::new(AtomicU8::new(Stage::Probing as u8));
        let painted = Arc::new(AtomicBool::new(false));

        let thread_stop = Arc::clone(&stop);
        let thread_stage = Arc::clone(&stage);
        let thread_painted = Arc::clone(&painted);
        let destination = destination.to_string();
        let started = Instant::now();

        let painter = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let elapsed = started.elapsed();
                if elapsed >= APPEAR_AFTER {
                    thread_painted.store(true, Ordering::Release);
                    paint(
                        &destination,
                        Stage::from_u8(thread_stage.load(Ordering::Acquire)),
                        elapsed,
                    );
                }
                std::thread::sleep(REPAINT_INTERVAL);
            }
        });

        Self {
            stop,
            stage,
            painter: Some(painter),
            painted,
        }
    }

    /// Advance to the next preflight leg. Cheap enough to call unconditionally.
    pub(crate) fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as u8, Ordering::Release);
    }

    /// Whether the surface was ever visible, so the caller knows if the held
    /// frame needs repairing.
    pub(crate) fn was_visible(&self) -> bool {
        self.painted.load(Ordering::Acquire)
    }
}

impl Drop for SwitchProgress {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(painter) = self.painter.take() {
            // Join, never detach: the next thing to touch this terminal is the
            // client process, and a painter still running when it starts would
            // scribble a stale box over a live frame.
            let _ = painter.join();
        }
        if self.was_visible() {
            // The box sat over a frozen frame, so nothing repaints it away on
            // its own — unlike the slots popup, which has a live slot beneath
            // it. Erase explicitly. The incoming client full-paints immediately
            // afterwards, so a brief cleared region is never seen; skipping the
            // erase, however, would leave the box burned into the frame if the
            // leg fails and we fall back.
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(b"\x1b7\x1b[2J\x1b8");
            let _ = stdout.flush();
        }
    }
}

fn paint(destination: &str, stage: Stage, elapsed: Duration) {
    let Ok(size) = crossterm::terminal::size() else {
        // No tty (a piped/CI launch): there is no held frame to write over and
        // nobody to read it. Staying silent is correct — writing escape codes
        // into a pipe would corrupt whatever is consuming it.
        return;
    };
    let (title, subtitle) = lines(destination, stage, elapsed);
    let (border, text) = tone(elapsed);
    crate::client::paint_centered_notice([title.as_str(), subtitle.as_str()], border, text, size);
}

/// The two text lines, as a pure function so the wording and the escalation
/// schedule are testable without a terminal.
fn lines(destination: &str, stage: Stage, elapsed: Duration) -> (String, String) {
    let title = format!("switching to {destination}…  {}s", elapsed.as_secs());
    let subtitle = if elapsed >= UNRESPONSIVE_AT {
        // Name the stage AND say it is overdue. "Not responding" alone invites
        // the user to kill a switch that is merely installing a binary. The
        // suffix is sized so the longest label still fits the box (see
        // `Stage::label`), because a truncated escalation is no escalation.
        format!("{} · taking longer", stage.label())
    } else {
        stage.label().to_string()
    };
    (title, subtitle)
}

/// Neutral → yellow at 3s, matching the slots popup's ramp exactly.
fn tone(elapsed: Duration) -> (&'static str, &'static str) {
    if elapsed >= YELLOW_AT {
        ("\x1b[33m", "\x1b[1;33m")
    } else {
        ("\x1b[37m", "\x1b[1m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The surface exists because the held frame says nothing. Its whole job is
    /// to answer the three questions #282 lists: where, how long, and is it
    /// healthy — so all three have to be on screen at once.
    #[test]
    fn the_surface_names_destination_elapsed_and_stage() {
        let (title, subtitle) = lines("sage", Stage::Probing, Duration::from_secs(2));
        assert!(title.contains("sage"), "where: {title}");
        assert!(title.contains("2s"), "how long: {title}");
        assert!(
            subtitle.contains("finding flock"),
            "what it is doing: {subtitle}"
        );
    }

    /// Past the unresponsive threshold the wording has to say the switch is
    /// overdue, while still naming the stage — "not responding" on its own
    /// invites killing a switch that is only installing a remote binary.
    #[test]
    fn an_overdue_switch_says_so_without_dropping_the_stage() {
        let (_, subtitle) = lines("sage", Stage::StartingServer, Duration::from_secs(12));
        assert!(subtitle.contains("taking longer"), "{subtitle}");
        assert!(
            subtitle.contains("remote server"),
            "the stage is what makes it actionable: {subtitle}"
        );
    }

    /// Every string this module can put on screen must FIT. A probe caught the
    /// first draft losing its escalation to truncation ("…taking long"), which
    /// is worse than not escalating: it looks like a rendering bug and says
    /// nothing. The box is 50 wide with 4 columns of chrome.
    #[test]
    fn no_line_overflows_the_notice_box() {
        const INNER_WIDTH: usize = 46;
        // The destination is user-supplied and legitimately long — it is
        // allowed to ellipsize. Everything this module AUTHORS must not.
        for stage in [
            Stage::Probing,
            Stage::StartingServer,
            Stage::Connecting,
            Stage::Attaching,
        ] {
            for secs in [0, 3, 12, 600] {
                let (_, subtitle) = lines("sage", stage, Duration::from_secs(secs));
                assert!(
                    subtitle.chars().count() <= INNER_WIDTH,
                    "subtitle overflows at {secs}s for {stage:?}: {subtitle:?} \
                     ({} chars)",
                    subtitle.chars().count()
                );
            }
        }
    }

    /// Same ramp as the slots popup: the two surfaces must read as one feature.
    #[test]
    fn tone_ramps_to_yellow_at_three_seconds() {
        assert_eq!(tone(Duration::from_secs(0)).0, "\x1b[37m");
        assert_eq!(tone(Duration::from_secs(3)).0, "\x1b[33m");
    }

    /// A warm switch must not flash a box up and instantly away — that reads as
    /// a glitch, and the surface is for the slow case.
    #[test]
    fn a_fast_switch_never_paints_and_so_needs_no_erase() {
        let progress = SwitchProgress::start("sage");
        progress.set_stage(Stage::Connecting);
        assert!(
            !progress.was_visible(),
            "nothing should paint before the appear delay"
        );
        drop(progress);
    }

    #[test]
    fn every_stage_has_a_label() {
        for stage in [
            Stage::Probing,
            Stage::StartingServer,
            Stage::Connecting,
            Stage::Attaching,
        ] {
            assert!(!stage.label().is_empty(), "{stage:?}");
            assert_eq!(Stage::from_u8(stage as u8), stage, "round trip: {stage:?}");
        }
    }
}
