//! Idle "flock" gimmick: when the UI sits idle, sheep wander onto the sidebar's
//! horizontal separator bars and graze on grass that sprouts along them; the
//! moment the user interacts, the flock bolts off-screen.
//!
//! Pure + tick-driven: positions are a function of `spinner_tick` (the existing
//! ~8 fps animation clock) and the flock phase, so there is no per-sheep mutable
//! state to keep in sync. The app only tracks *when* the user last interacted.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::state::Palette;

/// Idle time before the flock wanders in.
pub const IDLE_THRESHOLD: Duration = Duration::from_secs(20);
/// How long the rush-off animation runs after interaction resumes.
pub const FLEE_DURATION: Duration = Duration::from_millis(800);

const SHEEP: &str = "🐑";
/// Grass sprouts, then is cropped shorter as it's grazed.
const GRASS: [&str; 3] = ["🌱", "🌿", "ᵕ"];

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

/// Cheap deterministic hash so grass/sheep placement is stable per lane without
/// any RNG state.
fn hash(a: u32, b: u32) -> u32 {
    let mut h = a
        .wrapping_mul(2_654_435_761)
        .wrapping_add(b.wrapping_mul(40_503));
    h ^= h >> 15;
    h = h.wrapping_mul(2_246_822_519);
    h ^= h >> 13;
    h
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
                    let run = (s, x - 1);
                    if best.is_none_or(|(bs, be)| (be - bs) < (run.1 - run.0)) {
                        best = Some(run);
                    }
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            let run = (s, x_end - 1);
            if best.is_none_or(|(bs, be)| (be - bs) < (run.1 - run.0)) {
                best = Some(run);
            }
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

/// Draw the flock onto the separator bars in `area`. Non-destructive: the bars
/// are redrawn fresh each frame, so this just overlays the current positions.
pub fn render_flock(buf: &mut Buffer, area: Rect, tick: u32, phase: FlockPhase, palette: &Palette) {
    let lanes = separator_lanes(buf, area);
    if lanes.is_empty() {
        return;
    }

    let sheep_style = Style::default().add_modifier(Modifier::BOLD);
    let grass_style = Style::default().fg(palette.green);

    for (lane_idx, (y, x0, x1)) in lanes.into_iter().enumerate() {
        let lane = lane_idx as u32;
        let width = (x1 - x0 + 1) as i32;
        if width < 8 {
            continue;
        }

        // One or two sheep per bar, seeded per lane.
        let count = 1 + (hash(lane, 7) % 2) as i32;
        for i in 0..count {
            let seed = hash(lane, i as u32 + 1);
            // Grazing: a slow left-right drift. Phase/speed vary per sheep so the
            // flock doesn't move in lockstep.
            let speed = 2 + (seed % 3) as i32; // cells per ~second-ish
            let phase_off = (seed % 64) as i32;
            let base = ((tick as i32 / 4) * speed + phase_off) % (width * 2);
            // Triangle wave 0..width..0 so they pace back and forth, not wrap.
            let drift = if base < width { base } else { width * 2 - base };

            let x = match phase {
                FlockPhase::Grazing => drift,
                // Bolt toward the nearest edge: left half flee left, right flee right.
                FlockPhase::Fleeing(p) => {
                    let dir = if drift * 2 < width { -1 } else { 1 };
                    drift + dir * (p * (width as f32 + 4.0)) as i32
                }
            };
            if x < 0 || x >= width {
                continue; // bolted off this bar
            }
            let sx = x0 + x as u16;

            // A grass tuft sprouts just ahead of a grazing sheep, then is cropped
            // as it's eaten (cycles through the GRASS stages on the tick).
            // `set_string` handles wide-glyph (emoji) cell continuation so the
            // bar doesn't show through the second half.
            if matches!(phase, FlockPhase::Grazing) && sx > x0 {
                let stage = ((tick / 6 + seed) % 4) as usize;
                if stage < GRASS.len() {
                    buf.set_string(sx - 1, y, GRASS[stage], grass_style);
                }
            }
            buf.set_string(sx, y, SHEEP, sheep_style);
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
        // Half-way through the flee window.
        let until = now + FLEE_DURATION / 2;
        match flock_phase(stale, Some(until), now) {
            Some(FlockPhase::Fleeing(p)) => assert!((0.4..=0.6).contains(&p), "p={p}"),
            other => panic!("expected fleeing, got {other:?}"),
        }
        // Past the window: back to active (no flock).
        let elapsed = now - Duration::from_millis(1);
        assert_eq!(flock_phase(now, Some(elapsed), now), None);
    }

    #[test]
    fn separator_lanes_finds_the_bar_not_stray_glyphs() {
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        // Row 1 is a full rule; row 0 has a lone `─` in a label.
        buf.set_string(0, 0, "spaces ─ x", Style::default());
        for x in 0..20 {
            buf[(x, 1)].set_symbol("─");
        }
        let lanes = separator_lanes(&buf, area);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].0, 1, "only the full bar is a lane");
    }

    #[test]
    fn grazing_flock_draws_sheep_on_the_bar() {
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        for x in 0..30 {
            buf[(x, 1)].set_symbol("─");
        }
        let palette = Palette::catppuccin();
        render_flock(&mut buf, area, 0, FlockPhase::Grazing, &palette);
        let row: String = (0..30).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(row.contains(SHEEP), "a sheep grazes on the bar: {row:?}");
    }
}
