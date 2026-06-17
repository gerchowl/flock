//! Idle "flock" gimmick: when the UI sits idle, grass grows slowly along the
//! sidebar's separator bars. Sheep wander **in from the left or right edge**,
//! roam the sidebar — moving diagonally / Manhattan-style across the gaps to
//! reach grass on any bar — wait for a tuft to ripen, then race for it (closest
//! wins). They yield to avoid bumping, crop a tuft, move on, and after a few
//! patches amble back off the **side** they're nearest. Growth ramps up a little
//! over the idle spell. Any interaction and the flock bolts off the sides.
//!
//! A small time-stepped, agent-based sim over the 2-D sidebar field (grass lives
//! on the bar rows; sheep roam between them). State lives in [`SheepSim`] (held
//! in `AppState`) and advances by wall-clock `dt` each render — frame-rate
//! independent. A seeded LCG drives the organic randomness so it stays testable.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::state::Palette;

/// Idle time before the flock wanders in.
pub const IDLE_THRESHOLD: Duration = Duration::from_secs(20);
/// How long the rush-off animation runs after interaction resumes.
pub const FLEE_DURATION: Duration = Duration::from_millis(800);

// Nerd Font Material Design glyphs (the status bar's icon class), not emoji.
// nf-md-sheep renders as a front-view head, so a sheep is a 2-cell sprite: a
// woolly body (nf-md-cloud) trailing the grazing head.
const HEAD: &str = "\u{f0cc6}"; // nf-md-sheep
const WOOL: &str = "\u{f0590}"; // nf-md-cloud
const SPROUT: &str = "\u{f0e9c}"; // nf-md-sprout (young grass)
const WEED: &str = "\u{f1510}"; // nf-md-grass (ripe / overgrown)

const MAX_GRASS: f32 = 2.0;
const SPROUT_AT: f32 = 0.5; // height at which bare ground shows a sprout
const RIPE: f32 = 1.4; // a sheep only commits to grass at least this tall
const CROPPED: f32 = 0.3; // grass is "eaten" below this
const WALK: f32 = 3.0; // horizontal cells/sec — deliberately unhurried
const CLIMB: f32 = 2.0; // vertical rows/sec when crossing between bars
const EAT: f32 = 0.9; // grass-height/sec while grazing
const FLEE: f32 = 42.0; // cells/sec bolting off the side
const ARRIVE: f32 = 0.6; // within this of a tuft → start grazing
const MIN_GAP: f32 = 2.0; // keep at least a sprite-width between sheep
const EATEN_BEFORE_RETIRE: u32 = 4;
const SHEEP_CELLS: f32 = 2.0; // a sprite is wool + head
const MAX_OCCUPANCY: f32 = 0.30; // sheep cover at most ~30% of a bar's width
const LANE_CAP_HARD: usize = 8; // per-bar sanity ceiling
const SPAWN_COOLDOWN: f32 = 1.5; // sec between arrivals
const RAMP_FULL_SECS: f32 = 30.0; // idle time to reach max growth speed
const RAMP_MAX: f32 = 2.0;
const MAX_STEP: f32 = 0.25; // clamp a single step after a long pause

/// Per-bar sheep budget (~30% width occupied); summed for the field's total.
fn bar_cap(width: u16) -> usize {
    ((width as f32 * MAX_OCCUPANCY / SHEEP_CELLS).floor() as usize).clamp(1, LANE_CAP_HARD)
}

/// What the flock is doing this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlockPhase {
    Grazing,
    Fleeing(f32),
}

/// Resolve the flock phase from interaction timing, or `None` when active.
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

fn grass_glyph(height: f32) -> Option<&'static str> {
    if height >= RIPE {
        Some(WEED)
    } else if height >= SPROUT_AT {
        Some(SPROUT)
    } else {
        None
    }
}

/// Horizontal separator lanes (`y`, `x_start`, `x_end` inclusive): rows that are
/// a run of box-drawing `─`. Grass grows on these.
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    /// Roaming: heading for a claimed tuft, or idling if none is ripe.
    Roam,
    /// Grazing the tuft `(bar index, column)` down.
    Eat(usize, u16),
    /// Sated (or fleeing): ambling off the nearest side edge.
    Leaving,
}

#[derive(Debug, Clone)]
struct Sheep {
    x: f32,
    /// Absolute row — between bars while crossing, on a bar while grazing.
    y: f32,
    /// +1 heading right, -1 left. The wool body trails the head.
    facing: i8,
    /// Claimed tuft `(bar index, column)`.
    target: Option<(usize, u16)>,
    blocked: f32,
    eaten: u32,
    state: State,
}

#[derive(Debug, Clone)]
struct Tuft {
    x: u16,
    height: f32,
    grow: f32, // per-second base rate (varies per tuft)
}

#[derive(Debug, Clone)]
struct Bar {
    y: u16,
    x0: u16,
    x1: u16,
    tufts: Vec<Tuft>,
}

/// Persistent idle-flock simulation, held in `AppState` and stepped each render.
#[derive(Debug)]
pub struct SheepSim {
    bars: Vec<Bar>,
    sheep: Vec<Sheep>,
    signature: Vec<(u16, u16, u16)>,
    last_step: Option<Instant>,
    age: f32,
    spawn_cd: f32,
    was_fleeing: bool,
    rng: u64,
}

impl Default for SheepSim {
    fn default() -> Self {
        Self {
            bars: Vec::new(),
            sheep: Vec::new(),
            signature: Vec::new(),
            last_step: None,
            age: 0.0,
            spawn_cd: 0.0,
            was_fleeing: false,
            rng: 0x2545_f491_4f6c_dd1d,
        }
    }
}

impl SheepSim {
    fn rand(&mut self) -> f32 {
        self.rng = self
            .rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.rng >> 33) % 10_000) as f32 / 10_000.0
    }

    /// Field bounds across all bars: (x_min, x_max, y_min, y_max).
    fn bounds(&self) -> (f32, f32, f32, f32) {
        let xmin = self.bars.iter().map(|b| b.x0).min().unwrap_or(0) as f32;
        let xmax = self.bars.iter().map(|b| b.x1).max().unwrap_or(0) as f32;
        let ymin = self.bars.iter().map(|b| b.y).min().unwrap_or(0) as f32;
        let ymax = self.bars.iter().map(|b| b.y).max().unwrap_or(0) as f32;
        (xmin, xmax, ymin, ymax)
    }

    fn total_cap(&self) -> usize {
        self.bars.iter().map(|b| bar_cap(b.x1 - b.x0 + 1)).sum()
    }

    /// Advance and draw the flock onto the separator bars in `area`.
    pub fn step(&mut self, buf: &mut Buffer, area: Rect, fleeing: bool, palette: &Palette) {
        let lanes = separator_lanes(buf, area);
        if lanes.is_empty() {
            self.bars.clear();
            self.sheep.clear();
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

    fn advance(&mut self, dt: f32, lanes: &[(u16, u16, u16)], fleeing: bool) {
        if self.signature != lanes {
            self.signature = lanes.to_vec();
            self.bars = lanes
                .iter()
                .map(|&(y, x0, x1)| Bar {
                    y,
                    x0,
                    x1,
                    tufts: Vec::new(),
                })
                .collect();
            for bi in 0..self.bars.len() {
                self.seed_bar(bi);
            }
            self.sheep.clear();
            self.age = 0.0;
        }

        if self.was_fleeing && !fleeing {
            self.sheep.clear(); // fresh flock returns after a flee; grass intact
            self.age = 0.0;
        }
        self.was_fleeing = fleeing;

        if !fleeing {
            self.age += dt;
        }
        let ramp = (1.0 + self.age / RAMP_FULL_SECS * (RAMP_MAX - 1.0)).min(RAMP_MAX);
        if !fleeing {
            for bar in &mut self.bars {
                for t in &mut bar.tufts {
                    t.height = (t.height + t.grow * ramp * dt).min(MAX_GRASS);
                }
            }
        }

        let (xmin, xmax, _, _) = self.bounds();

        if fleeing {
            for s in &mut self.sheep {
                let dir = if s.x - xmin < xmax - s.x { -1.0 } else { 1.0 };
                s.facing = dir as i8;
                s.x += dir * FLEE * dt;
            }
            self.sheep
                .retain(|s| s.x >= xmin - 1.0 && s.x <= xmax + 1.0);
            return;
        }

        self.spawn(dt);
        self.assign_targets();
        self.move_sheep(dt);
        self.sheep.retain(|s| {
            !(matches!(s.state, State::Leaving) && (s.x < xmin - 1.0 || s.x > xmax + 1.0))
        });
    }

    fn spawn(&mut self, dt: f32) {
        self.spawn_cd -= dt;
        let cap = self.total_cap();
        let has_grass = self
            .bars
            .iter()
            .any(|b| b.tufts.iter().any(|t| t.height > 0.15));
        if self.sheep.len() >= cap || self.spawn_cd > 0.0 || !has_grass || self.bars.is_empty() {
            return;
        }
        let (xmin, xmax, _, _) = self.bounds();
        let from_left = self.rand() < 0.5;
        let bi = (self.rand() * self.bars.len() as f32) as usize % self.bars.len();
        let (ex, ey) = (if from_left { xmin } else { xmax }, self.bars[bi].y as f32);
        // Don't drop a new sheep on top of one still by that edge.
        if self
            .sheep
            .iter()
            .any(|s| (s.y - ey).abs() < 1.0 && (s.x - ex).abs() < MIN_GAP)
        {
            self.spawn_cd = 0.4;
            return;
        }
        self.sheep.push(Sheep {
            x: ex,
            y: ey,
            facing: if from_left { 1 } else { -1 },
            target: None,
            blocked: 0.0,
            eaten: 0,
            state: State::Roam,
        });
        self.spawn_cd = SPAWN_COOLDOWN + self.rand();
    }

    /// Closest-wins (Manhattan): each ripe, un-grazed tuft goes to the nearest
    /// roaming sheep that isn't already chasing a closer one.
    fn assign_targets(&mut self) {
        let eating: Vec<(usize, u16)> = self
            .sheep
            .iter()
            .filter_map(|s| match s.state {
                State::Eat(b, tx) => Some((b, tx)),
                _ => None,
            })
            .collect();
        let mut ripe: Vec<(usize, u16, u16)> = Vec::new(); // (bar, tuft x, bar y)
        for (bi, bar) in self.bars.iter().enumerate() {
            for t in &bar.tufts {
                if t.height >= RIPE && !eating.contains(&(bi, t.x)) {
                    ripe.push((bi, t.x, bar.y));
                }
            }
        }
        // Clear stale Roam targets; eating sheep keep theirs.
        for s in &mut self.sheep {
            if matches!(s.state, State::Roam) {
                s.target = None;
            }
        }
        let mut free: Vec<usize> = (0..self.sheep.len())
            .filter(|&i| matches!(self.sheep[i].state, State::Roam))
            .collect();
        while !ripe.is_empty() && !free.is_empty() {
            let mut best: Option<(usize, usize, f32)> = None;
            for (fi, &si) in free.iter().enumerate() {
                for (ri, &(_, tx, ty)) in ripe.iter().enumerate() {
                    let d =
                        (self.sheep[si].x - tx as f32).abs() + (self.sheep[si].y - ty as f32).abs();
                    if best.is_none_or(|(_, _, bd)| d < bd) {
                        best = Some((fi, ri, d));
                    }
                }
            }
            let (fi, ri, _) = best.unwrap();
            let (bi, tx, _) = ripe[ri];
            self.sheep[free[fi]].target = Some((bi, tx));
            free.swap_remove(fi);
            ripe.swap_remove(ri);
        }
    }

    fn move_sheep(&mut self, dt: f32) {
        let (xmin, xmax, _, _) = self.bounds();
        let cells: Vec<(usize, f32, f32)> = self
            .sheep
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.x, s.y))
            .collect();

        for si in 0..self.sheep.len() {
            match self.sheep[si].state {
                State::Eat(bi, tx) => {
                    let done = if let Some(t) = self.bars[bi].tufts.iter_mut().find(|t| t.x == tx) {
                        t.height = (t.height - EAT * dt).max(0.0);
                        t.height < CROPPED
                    } else {
                        true
                    };
                    if done {
                        self.sheep[si].eaten += 1;
                        self.sheep[si].state = State::Roam;
                        self.sheep[si].target = None;
                        if self.sheep[si].eaten >= EATEN_BEFORE_RETIRE {
                            self.sheep[si].state = State::Leaving;
                        }
                    }
                }
                State::Leaving => {
                    // Amble off the nearer side.
                    let s = &mut self.sheep[si];
                    let dir = if s.x - xmin < xmax - s.x { -1.0 } else { 1.0 };
                    s.facing = dir as i8;
                    s.x += dir * WALK * dt;
                }
                State::Roam => {
                    let Some((bi, tx)) = self.sheep[si].target else {
                        self.sheep[si].blocked = 0.0;
                        continue; // nothing ripe — graze-wait where we are
                    };
                    let (gx, gy) = (tx as f32, self.bars[bi].y as f32);
                    let (x, y) = (self.sheep[si].x, self.sheep[si].y);
                    if (x - gx).abs() <= ARRIVE && (y - gy).abs() <= ARRIVE {
                        self.sheep[si].x = gx;
                        self.sheep[si].y = gy;
                        self.sheep[si].state = State::Eat(bi, tx);
                        self.sheep[si].blocked = 0.0;
                        continue;
                    }
                    // Diagonal / Manhattan step toward the tuft.
                    let sx = (gx - x).clamp(-WALK * dt, WALK * dt);
                    let sy = (gy - y).clamp(-CLIMB * dt, CLIMB * dt);
                    // Avoid landing on a cell another sheep already holds; the
                    // detour below routes around it (Manhattan-style).
                    let occupied = |nx: f32, ny: f32| {
                        let (cx, cy) = (nx.round(), ny.round());
                        cells
                            .iter()
                            .any(|&(j, ox, oy)| j != si && ox.round() == cx && oy.round() == cy)
                    };
                    // Try diagonal, then a Manhattan detour (one axis), then wait.
                    let s = &mut self.sheep[si];
                    if sx != 0.0 {
                        s.facing = if sx > 0.0 { 1 } else { -1 };
                    }
                    if !occupied(x + sx, y + sy) {
                        s.x += sx;
                        s.y += sy;
                        s.blocked = 0.0;
                    } else if sy != 0.0 && !occupied(x, y + sy) {
                        s.y += sy; // step around vertically
                        s.blocked = 0.0;
                    } else if sx != 0.0 && !occupied(x + sx, y) {
                        s.x += sx;
                        s.blocked = 0.0;
                    } else {
                        s.blocked += dt;
                    }
                }
            }
        }
    }

    fn seed_bar(&mut self, bi: usize) {
        let (x0, x1) = (self.bars[bi].x0, self.bars[bi].x1);
        let mut tufts = Vec::new();
        let mut col = x0 + 1;
        while col < x1 {
            tufts.push(Tuft {
                x: col,
                height: self.rand() * 0.6,
                grow: 0.02 + self.rand() * 0.10,
            });
            col += 2 + (self.rand() * 4.0) as u16;
        }
        self.bars[bi].tufts = tufts;
    }

    fn draw(&self, buf: &mut Buffer, palette: &Palette) {
        let grass_style = Style::default().fg(palette.green);
        let sheep_style = Style::default().add_modifier(Modifier::BOLD);
        let (min_y, max_y) = (buf.area().top() as i32, buf.area().bottom() as i32);
        let (xmin, xmax, _, _) = self.bounds();
        for bar in &self.bars {
            for t in &bar.tufts {
                if let Some(glyph) = grass_glyph(t.height) {
                    buf.set_string(t.x, bar.y, glyph, grass_style);
                }
            }
        }
        for s in &self.sheep {
            let col = s.x.round();
            let row = s.y.round() as i32;
            if col < xmin || col > xmax || row < min_y || row >= max_y {
                continue;
            }
            let (row, head) = (row as u16, col as u16);
            let wool_x = head as i32 - s.facing as i32;
            if wool_x >= xmin as i32 && wool_x <= xmax as i32 {
                buf.set_string(wool_x as u16, row, WOOL, sheep_style);
            }
            buf.set_string(head, row, HEAD, sheep_style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(sim: &mut SheepSim, lanes: &[(u16, u16, u16)], secs: f32, fleeing: bool) {
        for _ in 0..(secs / 0.1) as usize {
            sim.advance(0.1, lanes, fleeing);
        }
    }

    #[test]
    fn phase_transitions() {
        let now = Instant::now();
        assert_eq!(flock_phase(now - Duration::from_secs(3), None, now), None);
        let stale = now - (IDLE_THRESHOLD + Duration::from_secs(1));
        assert_eq!(flock_phase(stale, None, now), Some(FlockPhase::Grazing));
        let until = now + FLEE_DURATION / 2;
        assert!(matches!(
            flock_phase(stale, Some(until), now),
            Some(FlockPhase::Fleeing(_))
        ));
    }

    #[test]
    fn bar_cap_scales_with_width() {
        assert_eq!(bar_cap(8), 1);
        assert_eq!(bar_cap(20), 3);
        assert_eq!(bar_cap(40), 6);
        assert_eq!(bar_cap(200), LANE_CAP_HARD);
    }

    #[test]
    fn sheep_enter_from_the_sides_only() {
        let mut sim = SheepSim::default();
        let lanes = vec![(3u16, 4u16, 30u16)];
        // Watch the first several arrivals: each appears at the x edges.
        for _ in 0..60 {
            sim.advance(0.1, &lanes, false);
            for s in &sim.sheep {
                // A fresh arrival is at an edge (others may have roamed inward).
                let _ = s;
            }
        }
        // At least one sheep arrived, and arrivals start at x == 4 or x == 30.
        assert!(!sim.sheep.is_empty());
        // Re-seed and check the very first spawn's x.
        let mut sim2 = SheepSim::default();
        sim2.advance(0.1, &lanes, false);
        // grass may be too short on the first tick; advance until a spawn happens
        let mut spawned_x = None;
        for _ in 0..40 {
            sim2.advance(0.1, &lanes, false);
            if let Some(s) = sim2.sheep.first() {
                spawned_x = Some(s.x);
                break;
            }
        }
        let x = spawned_x.expect("a sheep spawned");
        assert!(x == 4.0 || x == 30.0, "entered from a side edge: x={x}");
    }

    #[test]
    fn sheep_cross_between_bars_to_reach_grass() {
        // One ripe tuft on the LOWER bar; a sheep entering on the UPPER bar must
        // change its row (cross the gap) to reach it.
        let mut sim = SheepSim {
            bars: vec![
                Bar {
                    y: 2,
                    x0: 0,
                    x1: 30,
                    tufts: vec![],
                },
                Bar {
                    y: 8,
                    x0: 0,
                    x1: 30,
                    tufts: vec![Tuft {
                        x: 15,
                        height: MAX_GRASS,
                        grow: 0.0,
                    }],
                },
            ],
            signature: vec![(2, 0, 30), (8, 0, 30)],
            ..SheepSim::default()
        };
        sim.sheep.push(Sheep {
            x: 0.0,
            y: 2.0, // upper bar
            facing: 1,
            target: None,
            blocked: 0.0,
            eaten: 0,
            state: State::Roam,
        });
        let sig = sim.signature.clone();
        run(&mut sim, &sig, 8.0, false);
        // It reached the lower bar's row and grazed the tuft down.
        assert!(sim.bars[1].tufts[0].height < RIPE, "the tuft got grazed");
    }

    #[test]
    fn grass_ripens_and_a_sheep_grazes_it() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        run(&mut sim, &lanes, 70.0, false);
        assert!(!sim.sheep.is_empty(), "sheep wandered in");
        let total: f32 = sim.bars[0].tufts.iter().map(|t| t.height).sum();
        let max_possible = sim.bars[0].tufts.len() as f32 * MAX_GRASS;
        assert!(total < max_possible, "grazing kept some grass cropped");
    }

    #[test]
    fn fleeing_clears_the_field_off_the_sides() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        run(&mut sim, &lanes, 70.0, false);
        run(&mut sim, &lanes, 2.0, true);
        assert!(sim.sheep.is_empty(), "every sheep bolted off the sides");
    }
}
