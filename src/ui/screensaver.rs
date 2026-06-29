//! Idle screensaver — **stage 2** of the flock gimmick. Stage 1 ([`super::sheep`])
//! is the subtle grazing on the sidebar's separator bars after a short idle.
//! After a *longer* idle this scene takes over the sidebar column: sheep graze
//! along several fence lines, wander off as strays, and now and then a few bolt
//! for the screen edge. A sheepdog guards the field — it commits to the worst
//! offender, swings to the **outside** (the far side from the field centre) and
//! drives it back **inward**, then lets it be once it's within a safe radius;
//! when all is calm it lies down and naps with a drifting `zZz`. Any interaction
//! wipes the scene instantly (sheep bolt off, grass recedes) and the normal UI,
//! already drawn underneath, shows through.
//!
//! A time-stepped, seeded sim (same shape as [`super::sheep`]): state lives in
//! [`ScreensaverSim`] (held in `AppState`) and advances by wall-clock `dt` each
//! render, so it is frame-rate independent and deterministic under test.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::state::Palette;

/// Idle time before the stage-2 screensaver takes over the sidebar — deliberately
/// much longer than the sidebar-bar grazing ([`super::sheep::IDLE_THRESHOLD`]), so
/// it only appears after a genuinely long idle rather than a brief quiet spell.
pub const SCREENSAVER_THRESHOLD: Duration = Duration::from_secs(20 * 60);
/// How long the wipe (sheep bolt off, grass recedes) runs after interaction.
pub const WIPE_DURATION: Duration = Duration::from_millis(900);

// Nerd Font Material Design glyphs (the status bar's icon class), not emoji.
const HEAD: &str = "\u{f0cc6}"; // nf-md-sheep (front-view head)
const WOOL: &str = "\u{f0590}"; // nf-md-cloud (woolly body trailing the head)
const DOG: &str = "\u{f0a43}"; // nf-md-dog
const SPROUT: &str = "\u{f0e9c}"; // nf-md-sprout (young grass)
const WEED: &str = "\u{f1510}"; // nf-md-grass (ripe / overgrown)

const MAX_GRASS: f32 = 1.8;
const SPROUT_AT: f32 = 0.5; // height at which bare ground shows a sprout
const RIPE: f32 = 1.3; // height at which a sprout becomes overgrown
const GRAZE_R: f32 = 1.6; // within this of its line a sheep is "home"
const STRAY_R: f32 = 4.0; // beyond this from home the dog goes to fetch a sheep
const SAFE_R: f32 = 2.5; // once back within this, the dog lets the sheep be
const WANDER: f32 = 1.1; // cells/sec idle drift
const BOLT: f32 = 7.0; // cells/sec breaking for the edge
const RETURN: f32 = 3.2; // cells/sec fleeing the dog (toward the centre)
const DOG_SPD: f32 = 9.0; // dog outpaces any sheep
const FLEE_R: f32 = 5.0; // a targeted sheep reacts to the dog within this
const STANDOFF: f32 = 2.2; // dog parks this far on the outside of its target
const BREAKOUT_EVERY: f32 = 10.0; // seconds between breakout events
const GRASS_RECEDE: f32 = 6.0; // grass height/sec it withers during a wipe
const WIPE_SPD: f32 = 60.0; // cells/sec sheep bolt off during a wipe
const SLEEP_AFTER_CALM: f32 = 5.0; // calm seconds before the dog naps
const SLEEP_MIN: f32 = 3.5;
const SLEEP_MAX: f32 = 7.0;
const MAX_STEP: f32 = 0.25; // clamp a single step after a long pause
const LINE_GAP: u16 = 6; // rows between fence lines
const MAX_LINES: usize = 8;
const SHEEP_PER_LINE_MAX: u16 = 6;
const ZZZ_RATE: f32 = 0.22; // seconds per snore frame
const ZZZ_FRAMES: [&str; 6] = ["z  ", "Zz ", "zZz", " zZ", "  z", "   "];

/// What the screensaver is doing this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScreensaverPhase {
    Active,
    Wiping(f32),
}

/// Resolve the screensaver phase from interaction timing, or `None` when the
/// user is active (or only idle enough for stage-1 bar grazing).
/// `screensaver_after` is the configured stage-2 threshold ([`SCREENSAVER_THRESHOLD`]
/// by default, #16).
pub fn phase(
    last_interaction: Instant,
    wipe_until: Option<Instant>,
    now: Instant,
    screensaver_after: Duration,
) -> Option<ScreensaverPhase> {
    if let Some(until) = wipe_until {
        if now < until {
            let remaining = until.duration_since(now).as_secs_f32();
            let progress = (1.0 - remaining / WIPE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            return Some(ScreensaverPhase::Wiping(progress));
        }
    }
    (now.duration_since(last_interaction) >= screensaver_after).then_some(ScreensaverPhase::Active)
}

fn grass_glyph(height: f32) -> Option<&'static str> {
    if height >= RIPE {
        Some(WEED)
    } else if height >= SPROUT_AT {
        Some(SPROUT)
    } else {
        None
    }
}

/// One LCG step → a float in `[0, 1)`. Kept free-standing so the per-sheep
/// movement loop can advance a borrowed copy without re-borrowing the sim.
fn lcg(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 33) % 10_000) as f32 / 10_000.0
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SheepState {
    Graze,
    Stray,
    Bolt,
}

#[derive(Debug, Clone)]
struct Sheep {
    x: f32,
    y: f32,
    home: f32, // the fence line this sheep belongs to
    facing: i8,
    state: SheepState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DogMode {
    Guard,
    Sleep,
}

#[derive(Debug, Clone)]
struct Dog {
    x: f32,
    y: f32,
    facing: i8,
    mode: DogMode,
    sleep_t: f32, // remaining nap time
    calm_t: f32,  // seconds since the last stray
    patrol: f32,  // perimeter-patrol phase
    zzz_t: f32,   // snore-animation clock
    target: Option<usize>,
}

#[derive(Debug, Clone)]
struct Tuft {
    x: u16,
    y: u16,
    height: f32,
    grow: f32,
}

/// Persistent screensaver simulation (drawn into the sidebar column), held in `AppState`.
#[derive(Debug)]
pub struct ScreensaverSim {
    area: Rect,
    seeded: bool,
    was_wiping: bool,
    lines: Vec<u16>,
    sheep: Vec<Sheep>,
    grass: Vec<Tuft>,
    dog: Dog,
    breakout_cd: f32,
    last_step: Option<Instant>,
    rng: u64,
}

impl Default for ScreensaverSim {
    fn default() -> Self {
        Self {
            area: Rect::new(0, 0, 0, 0),
            seeded: false,
            was_wiping: false,
            lines: Vec::new(),
            sheep: Vec::new(),
            grass: Vec::new(),
            dog: Dog {
                x: 0.0,
                y: 0.0,
                facing: 1,
                mode: DogMode::Guard,
                sleep_t: 0.0,
                calm_t: 0.0,
                patrol: 0.0,
                zzz_t: 0.0,
                target: None,
            },
            breakout_cd: BREAKOUT_EVERY,
            last_step: None,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Fence rows the sheep call home: every `LINE_GAP` rows, inset from the edges.
fn compute_lines(area: Rect) -> Vec<u16> {
    let top = area.y.saturating_add(3);
    let bottom = area.y.saturating_add(area.height).saturating_sub(3);
    let mut lines = Vec::new();
    let mut y = top;
    while y <= bottom && lines.len() < MAX_LINES {
        lines.push(y);
        y = y.saturating_add(LINE_GAP);
    }
    if lines.is_empty() {
        lines.push(area.y.saturating_add(area.height / 2));
    }
    lines
}

impl ScreensaverSim {
    fn rand(&mut self) -> f32 {
        lcg(&mut self.rng)
    }

    fn field_x(&self) -> (f32, f32) {
        (
            self.area.x as f32,
            self.area
                .x
                .saturating_add(self.area.width)
                .saturating_sub(1) as f32,
        )
    }

    fn field_y(&self) -> (f32, f32) {
        (
            self.area.y as f32,
            self.area
                .y
                .saturating_add(self.area.height)
                .saturating_sub(1) as f32,
        )
    }

    fn center(&self) -> (f32, f32) {
        (
            self.area.x as f32 + self.area.width as f32 / 2.0,
            self.area.y as f32 + self.area.height as f32 / 2.0,
        )
    }

    /// Advance and draw the screensaver over the whole `area`.
    pub fn step(&mut self, buf: &mut Buffer, area: Rect, wiping: bool, palette: &Palette) {
        if area.width < 16 || area.height < 8 {
            return; // too cramped to be worth it
        }
        if !self.seeded || self.area != area {
            self.reseed(area);
        }
        let now = Instant::now();
        let dt = self
            .last_step
            .map_or(0.0, |t| now.duration_since(t).as_secs_f32())
            .min(MAX_STEP);
        self.last_step = Some(now);
        self.advance(dt, wiping);
        self.draw(buf, palette);
    }

    /// Fresh field: lines, grass, flock, and a guarding dog for `area`.
    fn reseed(&mut self, area: Rect) {
        self.area = area;
        self.seeded = true;
        self.was_wiping = false;

        let lines = compute_lines(area);
        let lo = area.x as f32 + 1.0;
        let hi = area.x.saturating_add(area.width).saturating_sub(2) as f32;
        let span = (hi - lo).max(1.0);

        let mut grass = Vec::new();
        let gcount = ((area.width as usize * lines.len()) / 6).clamp(8, 160);
        for _ in 0..gcount {
            let x = (lo + self.rand() * span).round() as u16;
            let li = (self.rand() * lines.len() as f32) as usize % lines.len();
            let height = self.rand() * 0.6;
            let grow = 0.01 + self.rand() * 0.04;
            grass.push(Tuft {
                x,
                y: lines[li],
                height,
                grow,
            });
        }

        let mut sheep = Vec::new();
        let per = (area.width / 14).clamp(2, SHEEP_PER_LINE_MAX);
        let inner = (span - 8.0).max(1.0);
        for &line in &lines {
            for _ in 0..per {
                let x = lo + 4.0 + self.rand() * inner;
                let facing = if self.rand() < 0.5 { 1 } else { -1 };
                sheep.push(Sheep {
                    x,
                    y: line as f32,
                    home: line as f32,
                    facing,
                    state: SheepState::Graze,
                });
            }
        }

        let cx = area.x as f32 + area.width as f32 / 2.0;
        self.dog = Dog {
            x: cx,
            y: area.y as f32 + 1.5,
            facing: 1,
            mode: DogMode::Guard,
            sleep_t: 0.0,
            calm_t: 0.0,
            patrol: 0.0,
            zzz_t: 0.0,
            target: None,
        };
        self.breakout_cd = BREAKOUT_EVERY * (0.3 + self.rand());
        self.lines = lines;
        self.grass = grass;
        self.sheep = sheep;
    }

    /// Worst offender, but *sticky*: keep the current target until it's back
    /// within the safe radius, so the dog commits to one job at a time rather
    /// than darting between sheep.
    fn pick_target(&self) -> Option<usize> {
        let (lo, hi) = self.field_x();
        let near_edge = |s: &Sheep| s.x < lo + 3.0 || s.x > hi - 3.0;

        if let Some(i) = self.dog.target {
            if let Some(s) = self.sheep.get(i) {
                if s.state == SheepState::Bolt || (s.y - s.home).abs() > SAFE_R || near_edge(s) {
                    return Some(i);
                }
            }
        }

        let any_bolt = self.sheep.iter().any(|s| s.state == SheepState::Bolt);
        let mut best: Option<(usize, f32)> = None;
        for (i, s) in self.sheep.iter().enumerate() {
            let off = (s.y - s.home).abs();
            let candidate = if any_bolt {
                s.state == SheepState::Bolt
            } else {
                off > STRAY_R || near_edge(s)
            };
            if !candidate {
                continue;
            }
            let score = off
                + if s.state == SheepState::Bolt {
                    50.0
                } else {
                    0.0
                };
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((i, score));
            }
        }
        best.map(|(i, _)| i)
    }

    fn advance(&mut self, dt: f32, wiping: bool) {
        if self.was_wiping && !wiping {
            self.reseed(self.area); // returning after a wipe → a fresh field
        }
        self.was_wiping = wiping;
        if wiping {
            self.wipe(dt);
            return;
        }

        for g in &mut self.grass {
            g.height = (g.height + g.grow * dt).min(MAX_GRASS);
        }

        // Occasional breakout: a few sheep make a run for the edge.
        self.breakout_cd -= dt;
        if self.breakout_cd <= 0.0 {
            self.breakout_cd = BREAKOUT_EVERY + (self.rand() * 3.0 - 1.5);
            let n = self.sheep.len();
            for _ in 0..3.min(n) {
                let i = (self.rand() * n as f32) as usize % n;
                self.sheep[i].state = SheepState::Bolt;
            }
        }

        // Dog brain: pick a job, or nap when nothing needs doing.
        let mut tgt = self.pick_target();
        if tgt.is_none() {
            self.dog.calm_t += dt;
        } else {
            self.dog.calm_t = 0.0;
            self.dog.mode = DogMode::Guard; // any stray/breakout wakes the dog
        }
        if self.dog.mode != DogMode::Sleep && tgt.is_none() && self.dog.calm_t > SLEEP_AFTER_CALM {
            self.dog.mode = DogMode::Sleep;
            self.dog.sleep_t = SLEEP_MIN + self.rand() * (SLEEP_MAX - SLEEP_MIN);
        }
        if self.dog.mode == DogMode::Sleep {
            self.dog.sleep_t -= dt;
            if self.dog.sleep_t <= 0.0 {
                self.dog.mode = DogMode::Guard;
            }
            tgt = None; // asleep: not actively herding
        }
        self.dog.target = tgt;
        let awake = self.dog.mode != DogMode::Sleep;
        self.dog.zzz_t = if awake { 0.0 } else { self.dog.zzz_t + dt };

        // Sheep movement (a borrowed rng copy keeps the sim un-borrowed in here).
        let (lo, hi) = self.field_x();
        let (y_min, y_max) = self.field_y();
        let cx = self.center().0;
        let (dogx, dogy) = (self.dog.x, self.dog.y);
        let mut rng = self.rng;
        for (i, s) in self.sheep.iter_mut().enumerate() {
            let near_home = (s.y - s.home).abs() <= GRAZE_R;
            let ddog = ((s.x - dogx).powi(2) + (s.y - dogy).powi(2)).sqrt();
            if Some(i) == tgt && awake && ddog < FLEE_R {
                // Flee the dog. The dog parks on the outside, so "away" → inward.
                let (ax, ay) = (s.x - dogx, s.y - dogy);
                let n = (ax * ax + ay * ay).sqrt().max(1e-4);
                s.x += ax / n * RETURN * dt;
                s.y += ay / n * RETURN * dt;
                s.facing = if ax >= 0.0 { 1 } else { -1 };
            } else if s.state == SheepState::Bolt {
                let d = if s.x < cx { -1.0 } else { 1.0 };
                s.facing = d as i8;
                s.x += d * BOLT * dt;
                if s.x <= lo + 1.0 || s.x >= hi - 1.0 {
                    s.state = SheepState::Stray; // reached the edge: a stray to fetch
                }
            } else {
                // Idle wander, gently biased back onto the home line.
                s.x += (lcg(&mut rng) * 2.0 - 1.0) * WANDER * dt;
                s.y += (s.home - s.y) * 0.6 * dt + (lcg(&mut rng) * 2.0 - 1.0) * WANDER * dt;
                s.state = if near_home {
                    SheepState::Graze
                } else {
                    SheepState::Stray
                };
            }
            s.x = s.x.clamp(lo, hi);
            s.y = s.y.clamp(y_min, y_max);
        }
        self.rng = rng;

        if !awake {
            return; // napping: the dog stays put (zZz)
        }

        // On duty: always work from OUTSIDE toward the centre. Stand on the far
        // side of the target from the field centre so the fleeing sheep is
        // driven inward; with no target, patrol the perimeter.
        let (cx, cy) = self.center();
        let (px, py) = if let Some(i) = tgt {
            let s = &self.sheep[i];
            let (ox, oy) = (s.x - cx, s.y - cy);
            let n = (ox * ox + oy * oy).sqrt().max(1e-4);
            (s.x + ox / n * STANDOFF, s.y + oy / n * STANDOFF)
        } else {
            self.dog.patrol += dt * 0.6;
            let rx = (self.area.width as f32 / 2.0 - 3.0).max(1.0);
            let ry = (self.area.height as f32 / 2.0 - 2.0).max(1.0);
            (
                cx + self.dog.patrol.cos() * rx,
                cy + self.dog.patrol.sin() * ry,
            )
        };
        let (dx, dy) = (px - self.dog.x, py - self.dog.y);
        let n = (dx * dx + dy * dy).sqrt().max(1e-4);
        self.dog.x = (self.dog.x + dx / n * DOG_SPD * dt).clamp(lo, hi);
        self.dog.y = (self.dog.y + dy / n * DOG_SPD * dt).clamp(y_min, y_max);
        self.dog.facing = if dx >= 0.0 { 1 } else { -1 };
    }

    /// Interaction wipe: grass withers while the whole flock (and the dog) bolts
    /// off the nearest side.
    fn wipe(&mut self, dt: f32) {
        for g in &mut self.grass {
            g.height = (g.height - GRASS_RECEDE * dt).max(0.0);
        }
        let (cx, _) = self.center();
        let (lo, hi) = self.field_x();
        for s in &mut self.sheep {
            let d = if s.x < cx { -1.0 } else { 1.0 };
            s.facing = d as i8;
            s.x += d * WIPE_SPD * dt;
        }
        self.sheep.retain(|s| s.x > lo - 2.0 && s.x < hi + 2.0);
        let d = if self.dog.x < cx { -1.0 } else { 1.0 };
        self.dog.x += d * WIPE_SPD * dt;
    }

    fn draw(&self, buf: &mut Buffer, palette: &Palette) {
        let buf_area = *buf.area();
        let area = self.area.intersection(buf_area);
        if area.is_empty() {
            return;
        }
        let (x0, x1) = (area.x, area.x.saturating_add(area.width));
        let (y0, y1) = (area.y, area.y.saturating_add(area.height));
        let in_x = |x: i32| x >= x0 as i32 && x < x1 as i32;
        let in_y = |y: i32| y >= y0 as i32 && y < y1 as i32;

        // Clear to a clean field so the scene doesn't sit on the terminal text.
        let bg = Style::default().bg(palette.panel_bg);
        for y in y0..y1 {
            for x in x0..x1 {
                let cell = &mut buf[(x, y)];
                cell.reset();
                cell.set_style(bg);
            }
        }

        let line_style = bg.fg(palette.overlay0);
        let grass_style = bg.fg(palette.green);
        let sheep_style = bg.add_modifier(Modifier::BOLD);
        let dog_style = bg.fg(palette.peach).add_modifier(Modifier::BOLD);
        let zzz_style = bg.fg(palette.subtext0);

        for &y in &self.lines {
            if !in_y(y as i32) {
                continue;
            }
            for x in x0..x1 {
                buf.set_string(x, y, "─", line_style);
            }
        }

        for g in &self.grass {
            if let Some(glyph) = grass_glyph(g.height) {
                if in_x(g.x as i32) && in_y(g.y as i32) {
                    buf.set_string(g.x, g.y, glyph, grass_style);
                }
            }
        }

        for s in &self.sheep {
            let (col, row) = (s.x.round() as i32, s.y.round() as i32);
            if !in_x(col) || !in_y(row) {
                continue;
            }
            let wx = col - s.facing as i32;
            if in_x(wx) {
                buf.set_string(wx as u16, row as u16, WOOL, sheep_style);
            }
            buf.set_string(col as u16, row as u16, HEAD, sheep_style);
        }

        let (dcol, drow) = (self.dog.x.round() as i32, self.dog.y.round() as i32);
        if in_x(dcol) && in_y(drow) {
            buf.set_string(dcol as u16, drow as u16, DOG, dog_style);
            if self.dog.mode == DogMode::Sleep {
                let idx = (self.dog.zzz_t / ZZZ_RATE) as usize % ZZZ_FRAMES.len();
                let zy = drow - 1;
                for (i, ch) in ZZZ_FRAMES[idx].chars().enumerate() {
                    let zx = dcol + 1 + i as i32;
                    if ch != ' ' && in_x(zx) && in_y(zy) {
                        buf.set_string(zx as u16, zy as u16, ch.to_string(), zzz_style);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(w: u16, h: u16) -> ScreensaverSim {
        let mut sim = ScreensaverSim::default();
        sim.reseed(Rect::new(0, 0, w, h));
        sim
    }

    fn run(sim: &mut ScreensaverSim, secs: f32, wiping: bool) {
        for _ in 0..(secs / 0.1) as usize {
            sim.advance(0.1, wiping);
        }
    }

    #[test]
    fn phase_transitions() {
        let now = Instant::now();
        assert_eq!(
            phase(
                now - Duration::from_secs(3),
                None,
                now,
                SCREENSAVER_THRESHOLD
            ),
            None
        );
        let stale = now - (SCREENSAVER_THRESHOLD + Duration::from_secs(1));
        assert_eq!(
            phase(stale, None, now, SCREENSAVER_THRESHOLD),
            Some(ScreensaverPhase::Active)
        );
        let until = now + WIPE_DURATION / 2;
        assert!(matches!(
            phase(stale, Some(until), now, SCREENSAVER_THRESHOLD),
            Some(ScreensaverPhase::Wiping(_))
        ));
    }

    #[test]
    fn seeds_lines_and_sheep_within_the_area() {
        let sim = seeded(80, 24);
        assert!(!sim.lines.is_empty());
        for &y in &sim.lines {
            assert!(y >= sim.area.y && y < sim.area.y + sim.area.height);
        }
        assert!(!sim.sheep.is_empty());
        for s in &sim.sheep {
            assert!(sim.lines.contains(&(s.home as u16)), "home is a fence line");
        }
    }

    #[test]
    fn dog_naps_when_the_field_is_calm() {
        let mut sim = seeded(80, 24);
        // Park every sheep exactly on its line and stop breakouts: nothing for
        // the dog to do, so after SLEEP_AFTER_CALM it should lie down.
        for s in &mut sim.sheep {
            s.y = s.home;
            s.state = SheepState::Graze;
        }
        sim.breakout_cd = 1.0e6;
        run(&mut sim, SLEEP_AFTER_CALM + 2.0, false);
        assert_eq!(sim.dog.mode, DogMode::Sleep);
        assert!(sim.dog.zzz_t > 0.0, "snore clock advances while asleep");
    }

    #[test]
    fn dog_detaches_within_the_safe_radius() {
        let mut sim = seeded(80, 24);
        let home = sim.lines[1];
        // A settled sheep just inside the safe radius is left alone...
        sim.sheep = vec![Sheep {
            x: 40.0,
            y: home as f32 + (SAFE_R - 0.5),
            home: home as f32,
            facing: 1,
            state: SheepState::Stray,
        }];
        sim.dog.target = Some(0);
        assert_eq!(sim.pick_target(), None, "released inside the safe radius");
        // ...but a sheep beyond the stray radius is fetched.
        sim.sheep[0].y = home as f32 + (STRAY_R + 1.0);
        assert_eq!(
            sim.pick_target(),
            Some(0),
            "fetched beyond the stray radius"
        );
    }

    #[test]
    fn breakout_sheep_are_brought_back_and_turn_over_to_grazing() {
        let mut sim = seeded(80, 24);
        for s in &mut sim.sheep {
            s.state = SheepState::Bolt;
        }
        run(&mut sim, 30.0, false);
        let home = sim
            .sheep
            .iter()
            .filter(|s| (s.y - s.home).abs() <= STRAY_R)
            .count();
        assert!(
            home > 0,
            "the dog gets at least some of the flock back home"
        );
    }

    #[test]
    fn draw_paints_the_scene_onto_the_buffer() {
        let area = Rect::new(0, 0, 80, 24);
        let mut sim = seeded(area.width, area.height);
        sim.advance(0.1, false);
        let mut buf = Buffer::empty(area);
        sim.draw(&mut buf, &Palette::catppuccin());
        let painted: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(painted.contains('─'), "fence lines are drawn");
        assert!(painted.contains(DOG), "the dog is drawn");
        assert!(painted.contains(HEAD), "sheep are drawn");
    }

    #[test]
    fn wiping_clears_sheep_and_grass() {
        let mut sim = seeded(60, 20);
        run(&mut sim, 40.0, false);
        for t in &mut sim.grass {
            t.height = MAX_GRASS;
        }
        run(&mut sim, WIPE_DURATION.as_secs_f32() + 0.5, true);
        assert!(sim.sheep.is_empty(), "every sheep bolted off the field");
        assert!(
            sim.grass.iter().all(|t| t.height < SPROUT_AT),
            "grass withered away on interaction"
        );
    }
}
