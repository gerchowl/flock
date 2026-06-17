//! Idle "flock" gimmick: when the UI sits idle, grass slowly grows along the
//! sidebar's horizontal separator bars (each tuft at its own pace), sheep wander
//! in and head for the tallest tuft — but never the one another sheep is already
//! making for — crop it down, and move on while it regrows. The moment the user
//! interacts, the flock bolts off-screen.
//!
//! A tiny time-stepped simulation: grass heights and sheep positions persist in
//! [`SheepSim`] (held in `AppState`) and advance by wall-clock `dt` each render,
//! so motion is frame-rate independent. Seeding is hash-deterministic (no RNG
//! state), which keeps it testable.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::state::Palette;

/// Idle time before the flock wanders in.
pub const IDLE_THRESHOLD: Duration = Duration::from_secs(20);
/// How long the rush-off animation runs after interaction resumes.
pub const FLEE_DURATION: Duration = Duration::from_millis(800);

// Nerd Font Material Design glyphs (the same icon class as the status bar's
// cpu/mem/disk), not emoji — 1-wide and theme-consistent.
const SHEEP: &str = "\u{f0cc6}"; // nf-md-sheep
const GRASS_TIERS: [&str; 2] = ["\u{f0e9c}", "\u{f1510}"]; // nf-md-sprout, nf-md-grass

const MAX_GRASS: f32 = 2.0;
/// A sheep will graze anything at least sprouted.
const GRAZE_MIN: f32 = 0.7;
const WALK_CELLS_PER_SEC: f32 = 6.0;
const EAT_PER_SEC: f32 = 1.1;
const FLEE_CELLS_PER_SEC: f32 = 48.0;
/// Cap a single step so a long pause (no renders) doesn't teleport everything.
const MAX_STEP: f32 = 0.25;

/// What the flock is doing this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlockPhase {
    /// Grazing along the bars.
    Grazing,
    /// Bolting off-screen; `progress` runs 0.0 → 1.0.
    Fleeing(f32),
}

/// Resolve the flock phase from interaction timing, or `None` when the user is
/// active (and the brief flee window has elapsed).
pub fn flock_phase(
    last_interaction: Instant,
    flee_until: Option<Instant>,
    now: Instant,
) -> Option<FlockPhase> {
    if let Some(until) = flee_until {
        if now < until {
            let remaining = until.duration_since(now).as_secs_f32();
            let progress = (1.0 - remaining / FLEE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            return Some(FlockPhase::Fleeing(progress));
        }
    }
    (now.duration_since(last_interaction) >= IDLE_THRESHOLD).then_some(FlockPhase::Grazing)
}

/// Cheap deterministic hash so grass/sheep seeding is stable without RNG state.
fn hash(a: u32, b: u32) -> u32 {
    let mut h = a
        .wrapping_mul(2_654_435_761)
        .wrapping_add(b.wrapping_mul(40_503));
    h ^= h >> 15;
    h = h.wrapping_mul(2_246_822_519);
    h ^= h >> 13;
    h
}

/// Glyph for a grass height, or `None` when grazed to the ground.
fn grass_glyph(height: f32) -> Option<&'static str> {
    if height >= 1.6 {
        Some(GRASS_TIERS[1])
    } else if height >= GRAZE_MIN {
        Some(GRASS_TIERS[0])
    } else {
        None
    }
}

/// Horizontal separator lanes (`y`, `x_start`, `x_end` inclusive) within `area`:
/// rows that are a run of box-drawing `─`. The flock only walks on these.
fn separator_lanes(buf: &Buffer, area: Rect) -> Vec<(u16, u16, u16)> {
    let mut lanes = Vec::new();
    let x_end = area.x.saturating_add(area.width);
    let y_end = area.y.saturating_add(area.height);
    for y in area.y..y_end {
        let mut start: Option<u16> = None;
        let mut best: Option<(u16, u16)> = None;
        for x in area.x..x_end {
            let is_rule = buf.cell((x, y)).is_some_and(|c| c.symbol() == "─");
            match (is_rule, start) {
                (true, None) => start = Some(x),
                (false, Some(s)) => {
                    consider_run(&mut best, s, x - 1);
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            consider_run(&mut best, s, x_end - 1);
        }
        // Only count a real bar (avoid one-off `─` glyphs in labels).
        if let Some((s, e)) = best {
            if e - s >= 6 {
                lanes.push((y, s, e));
            }
        }
    }
    lanes
}

fn consider_run(best: &mut Option<(u16, u16)>, s: u16, e: u16) {
    if best.is_none_or(|(bs, be)| (be - bs) < (e - s)) {
        *best = Some((s, e));
    }
}

#[derive(Debug, Clone)]
struct Tuft {
    x: u16,
    height: f32,
    /// Growth per second — varies per tuft so they fill in at different speeds.
    grow: f32,
}

#[derive(Debug, Clone)]
struct Sheep {
    /// Absolute column (float for sub-cell motion).
    x: f32,
    /// Column of the tuft this sheep is making for.
    target: Option<u16>,
}

#[derive(Debug, Clone)]
struct Lane {
    y: u16,
    x0: u16,
    x1: u16,
    tufts: Vec<Tuft>,
    sheep: Vec<Sheep>,
}

impl Lane {
    fn seed(idx: usize, y: u16, x0: u16, x1: u16) -> Self {
        let width = (x1 - x0 + 1) as u32;
        // A tuft roughly every 4 cells, each with its own pace and a low start
        // so the bar visibly greens up while you're away.
        let mut tufts = Vec::new();
        let mut col = x0 + 1;
        let mut slot = 0u32;
        while col < x1 {
            let s = hash(idx as u32, slot);
            tufts.push(Tuft {
                x: col,
                height: (s % 50) as f32 / 100.0, // 0.00..0.50
                grow: 0.15 + (s >> 8) as f32 % 100.0 / 160.0, // ~0.15..0.77 /s
            });
            col += 3 + (s % 3) as u16;
            slot += 1;
        }
        // One or two sheep, spaced across the bar.
        let count = 1 + (hash(idx as u32, 99) % 2);
        let sheep = (0..count)
            .map(|i| Sheep {
                x: (x0 + (hash(idx as u32, 200 + i) % width.max(1)) as u16) as f32,
                target: None,
            })
            .collect();
        Lane {
            y,
            x0,
            x1,
            tufts,
            sheep,
        }
    }
}

/// Persistent idle-flock simulation, held in `AppState` and stepped each render.
#[derive(Debug, Default)]
pub struct SheepSim {
    lanes: Vec<Lane>,
    /// Geometry the lanes were seeded for; reseed when it changes.
    signature: Vec<(u16, u16, u16)>,
    last_step: Option<Instant>,
}

impl SheepSim {
    /// Advance the simulation and draw it onto the separator bars in `area`.
    /// `fleeing` is true while the flock is bolting off after interaction.
    pub fn step(&mut self, buf: &mut Buffer, area: Rect, fleeing: bool, palette: &Palette) {
        let lanes = separator_lanes(buf, area);
        if lanes.is_empty() {
            self.lanes.clear();
            self.signature.clear();
            return;
        }
        let now = Instant::now();
        let dt = self
            .last_step
            .map_or(0.0, |t| now.duration_since(t).as_secs_f32())
            .min(MAX_STEP);
        self.last_step = Some(now);
        self.advance(dt, &lanes, fleeing);
        self.draw(buf, palette);
    }

    /// Pure time-step: grow grass, then move each sheep toward the tallest tuft
    /// no other sheep is already heading for, grazing it down on arrival.
    fn advance(&mut self, dt: f32, lanes: &[(u16, u16, u16)], fleeing: bool) {
        if self.signature != lanes {
            self.signature = lanes.to_vec();
            self.lanes = lanes
                .iter()
                .enumerate()
                .map(|(i, &(y, x0, x1))| Lane::seed(i, y, x0, x1))
                .collect();
        }

        for (idx, lane) in self.lanes.iter_mut().enumerate() {
            let (lo, hi) = (lane.x0 as f32, lane.x1 as f32);

            if !fleeing {
                for t in &mut lane.tufts {
                    t.height = (t.height + t.grow * dt).min(MAX_GRASS);
                }
            }

            for si in 0..lane.sheep.len() {
                if fleeing {
                    let toward_left = lane.sheep[si].x - lo < hi - lane.sheep[si].x;
                    let dir = if toward_left { -1.0 } else { 1.0 };
                    lane.sheep[si].x += dir * FLEE_CELLS_PER_SEC * dt;
                    lane.sheep[si].target = None;
                    continue;
                }

                // Returned from a flee (or never placed): drop back onto the bar.
                if lane.sheep[si].x < lo || lane.sheep[si].x > hi {
                    let span = (lane.x1 - lane.x0 + 1) as u32;
                    lane.sheep[si].x =
                        lane.x0 as f32 + (hash(idx as u32, 300 + si as u32) % span.max(1)) as f32;
                    lane.sheep[si].target = None;
                }

                // (Re)choose a target: tallest grazeable tuft no one else claims.
                let stale = match lane.sheep[si].target {
                    None => true,
                    Some(tx) => lane
                        .tufts
                        .iter()
                        .find(|t| t.x == tx)
                        .is_none_or(|t| t.height < GRAZE_MIN),
                };
                if stale {
                    let claimed: Vec<u16> = lane
                        .sheep
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != si)
                        .filter_map(|(_, s)| s.target)
                        .collect();
                    lane.sheep[si].target = lane
                        .tufts
                        .iter()
                        .filter(|t| t.height >= GRAZE_MIN && !claimed.contains(&t.x))
                        .max_by(|a, b| a.height.total_cmp(&b.height))
                        .map(|t| t.x);
                }

                // Walk toward the target and graze it down on arrival.
                if let Some(tx) = lane.sheep[si].target {
                    let target = tx as f32;
                    if (lane.sheep[si].x - target).abs() > 0.6 {
                        let dir = (target - lane.sheep[si].x).signum();
                        lane.sheep[si].x += dir * WALK_CELLS_PER_SEC * dt;
                    } else {
                        lane.sheep[si].x = target;
                        if let Some(t) = lane.tufts.iter_mut().find(|t| t.x == tx) {
                            t.height = (t.height - EAT_PER_SEC * dt).max(0.0);
                            if t.height < GRAZE_MIN {
                                lane.sheep[si].target = None;
                            }
                        }
                    }
                }
            }
        }
    }

    fn draw(&self, buf: &mut Buffer, palette: &Palette) {
        let grass_style = Style::default().fg(palette.green);
        let sheep_style = Style::default().add_modifier(Modifier::BOLD);
        for lane in &self.lanes {
            for t in &lane.tufts {
                if let Some(glyph) = grass_glyph(t.height) {
                    buf.set_string(t.x, lane.y, glyph, grass_style);
                }
            }
            for s in &lane.sheep {
                let col = s.x.round();
                if col >= lane.x0 as f32 && col <= lane.x1 as f32 {
                    buf.set_string(col as u16, lane.y, SHEEP, sheep_style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_is_none_until_idle_then_grazes() {
        let now = Instant::now();
        let fresh = now - Duration::from_secs(5);
        assert_eq!(flock_phase(fresh, None, now), None);
        let stale = now - (IDLE_THRESHOLD + Duration::from_secs(1));
        assert_eq!(flock_phase(stale, None, now), Some(FlockPhase::Grazing));
    }

    #[test]
    fn flee_window_overrides_idle_and_reports_progress() {
        let now = Instant::now();
        let stale = now - (IDLE_THRESHOLD + Duration::from_secs(1));
        let until = now + FLEE_DURATION / 2;
        match flock_phase(stale, Some(until), now) {
            Some(FlockPhase::Fleeing(p)) => assert!((0.4..=0.6).contains(&p), "p={p}"),
            other => panic!("expected fleeing, got {other:?}"),
        }
        let elapsed = now - Duration::from_millis(1);
        assert_eq!(flock_phase(now, Some(elapsed), now), None);
    }

    #[test]
    fn separator_lanes_finds_the_bar_not_stray_glyphs() {
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "spaces ─ x", Style::default());
        for x in 0..20 {
            buf[(x, 1)].set_symbol("─");
        }
        let lanes = separator_lanes(&buf, area);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].0, 1);
    }

    fn lane_with_grass() -> (SheepSim, Vec<(u16, u16, u16)>) {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 29u16)];
        // Grow for a while so grass is tall and sheep have something to target.
        for _ in 0..200 {
            sim.advance(0.1, &lanes, false);
        }
        (sim, lanes)
    }

    #[test]
    fn grass_grows_then_a_sheep_grazes_it_down() {
        let (mut sim, lanes) = lane_with_grass();
        let tallest = sim.lanes[0]
            .tufts
            .iter()
            .map(|t| t.height)
            .fold(0.0_f32, f32::max);
        assert!(tallest > GRAZE_MIN, "grass grew in: {tallest}");

        // Every sheep should be targeting some tuft now that grass is up.
        assert!(sim.lanes[0].sheep.iter().all(|s| s.target.is_some()));

        // Run the sim; the total grass mass must drop as sheep crop it.
        let mass = |s: &SheepSim| s.lanes[0].tufts.iter().map(|t| t.height).sum::<f32>();
        let before = mass(&sim);
        for _ in 0..40 {
            sim.advance(0.1, &lanes, false);
        }
        // Sheep ate faster than regrowth somewhere, or at least kept it in check.
        assert!(mass(&sim) <= before + 0.5, "grazing keeps grass in check");
    }

    #[test]
    fn two_sheep_never_share_a_target() {
        let (sim, _) = lane_with_grass();
        let lane = &sim.lanes[0];
        if lane.sheep.len() >= 2 {
            let targets: Vec<_> = lane.sheep.iter().filter_map(|s| s.target).collect();
            let mut uniq = targets.clone();
            uniq.sort_unstable();
            uniq.dedup();
            assert_eq!(targets.len(), uniq.len(), "each sheep claims its own tuft");
        }
    }

    #[test]
    fn fleeing_sheep_leave_the_bar() {
        let (mut sim, lanes) = lane_with_grass();
        for _ in 0..30 {
            sim.advance(0.1, &lanes, true);
        }
        let lane = &sim.lanes[0];
        assert!(
            lane.sheep
                .iter()
                .all(|s| s.x < lane.x0 as f32 || s.x > lane.x1 as f32),
            "all sheep bolted off the bar"
        );
    }
}
