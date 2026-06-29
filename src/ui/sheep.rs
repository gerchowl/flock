//! Idle "flock" gimmick: when the UI sits idle, grass grows slowly along each of
//! the sidebar's separator bars. Every bar is its own strip — a sheep walks in
//! from the left or right **side**, heads for the nearest ripe tuft no closer
//! sheep is claiming, and crops it. Sheep stay with their bar but may stray a
//! couple of rows above/below it to walk around one another, returning to the
//! line to graze; after a few patches they amble off the side. Sheep only arrive
//! when there's spare ripe grass, so none loiter at the edges. Growth ramps up a
//! little over the idle spell. Any interaction and the flock bolts off the sides.
//!
//! A small time-stepped, agent-based sim, one independent strip per bar (with a
//! ±`BAND`-row roaming band). State lives in [`SheepSim`] (held in `AppState`)
//! and advances by wall-clock `dt` each render — frame-rate independent. A
//! seeded LCG drives the organic randomness so it stays testable.

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
const WALK: f32 = 1.6; // horizontal cells/sec — deliberately unhurried
const CLIMB: f32 = 0.9; // vertical rows/sec when arcing around — eased, not snappy
const BAND: f32 = 2.0; // sheep may stray this many rows above/below their bar
const EAT: f32 = 0.9; // grass-height/sec while grazing
const FLEE: f32 = 42.0; // cells/sec bolting off the side
const GRASS_RECEDE: f32 = 5.0; // grass height/sec it withers while fleeing
const ARRIVE: f32 = 0.6; // within this of a tuft (on the line) → start grazing
const EATEN_BEFORE_RETIRE: u32 = 4;
const SHEEP_CELLS: f32 = 2.0; // a sprite is wool + head
const MAX_OCCUPANCY: f32 = 0.30; // sheep cover at most ~30% of a bar's width
const LANE_CAP_HARD: usize = 8; // per-bar sanity ceiling
const SPAWN_COOLDOWN: f32 = 4.0; // sec between a bar's arrivals — flock fills in slowly
const RAMP_FULL_SECS: f32 = 30.0; // idle time to reach max growth speed
const RAMP_MAX: f32 = 2.0;
const MAX_STEP: f32 = 0.25; // clamp a single step after a long pause

/// Max sheep on a bar: ~30% of its width occupied (each sprite is 2 cells).
fn lane_cap(width: u16) -> usize {
    ((width as f32 * MAX_OCCUPANCY / SHEEP_CELLS).floor() as usize).clamp(1, LANE_CAP_HARD)
}

/// What the flock is doing this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlockPhase {
    Grazing,
    Fleeing(f32),
}

/// Resolve the flock phase from interaction timing, or `None` when active.
/// `idle_after` is the configured idle threshold ([`IDLE_THRESHOLD`] by default,
/// #16) — the grazing flock wanders in once the user has been idle that long.
pub fn flock_phase(
    last_interaction: Instant,
    flee_until: Option<Instant>,
    now: Instant,
    idle_after: Duration,
) -> Option<FlockPhase> {
    if let Some(until) = flee_until {
        if now < until {
            let remaining = until.duration_since(now).as_secs_f32();
            let progress = (1.0 - remaining / FLEE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            return Some(FlockPhase::Fleeing(progress));
        }
    }
    (now.duration_since(last_interaction) >= idle_after).then_some(FlockPhase::Grazing)
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
/// a run of box-drawing `─`. Each is an independent grazing strip.
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
    /// Heading for a claimed ripe tuft (its column).
    Walk(u16),
    /// Grazing the tuft at this column down.
    Eat(u16),
    /// Done (or nothing left to chase): ambling off the nearest side.
    Leaving,
}

#[derive(Debug, Clone)]
struct Sheep {
    x: f32,
    /// Row — usually the bar, but may stray ±`BAND` to walk around others.
    y: f32,
    /// +1 heading right, -1 left. The wool body trails the head.
    facing: i8,
    /// Row offset (±`BAND`) the sheep is currently committed to while arcing
    /// around another; 0 once it's clear and re-joining its bar.
    detour: f32,
    state: State,
    eaten: u32,
}

#[derive(Debug, Clone)]
struct Tuft {
    x: u16,
    height: f32,
    grow: f32, // per-second base rate (varies per tuft)
}

#[derive(Debug, Clone)]
struct Lane {
    y: u16,
    x0: u16,
    x1: u16,
    tufts: Vec<Tuft>,
    sheep: Vec<Sheep>,
    spawn_cd: f32,
    next_left: bool, // alternate entry side
}

/// Persistent idle-flock simulation, held in `AppState` and stepped each render.
#[derive(Debug)]
pub struct SheepSim {
    lanes: Vec<Lane>,
    signature: Vec<(u16, u16, u16)>,
    last_step: Option<Instant>,
    age: f32,
    was_fleeing: bool,
    rng: u64,
}

impl Default for SheepSim {
    fn default() -> Self {
        Self {
            lanes: Vec::new(),
            signature: Vec::new(),
            last_step: None,
            age: 0.0,
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

    /// Advance and draw the flock onto the separator bars in `area`.
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

    fn advance(&mut self, dt: f32, lanes: &[(u16, u16, u16)], fleeing: bool) {
        if self.signature != lanes {
            self.signature = lanes.to_vec();
            let mut seeded = Vec::with_capacity(lanes.len());
            for &(y, x0, x1) in lanes {
                seeded.push(self.seed_lane(y, x0, x1));
            }
            self.lanes = seeded;
            self.age = 0.0;
        }

        if self.was_fleeing && !fleeing {
            for lane in &mut self.lanes {
                lane.sheep.clear();
                lane.spawn_cd = 0.0;
            }
            self.age = 0.0;
        }
        self.was_fleeing = fleeing;

        if !fleeing {
            self.age += dt;
        }
        let ramp = (1.0 + self.age / RAMP_FULL_SECS * (RAMP_MAX - 1.0)).min(RAMP_MAX);

        for li in 0..self.lanes.len() {
            if !fleeing {
                for t in &mut self.lanes[li].tufts {
                    t.height = (t.height + t.grow * ramp * dt).min(MAX_GRASS);
                }
            }
            self.lanes[li].spawn_cd -= dt;
            let ripe = self.lanes[li]
                .tufts
                .iter()
                .filter(|t| t.height >= RIPE)
                .count();
            let lane = &self.lanes[li];
            let cap = lane_cap(lane.x1 - lane.x0 + 1);
            if !fleeing && lane.spawn_cd <= 0.0 && lane.sheep.len() < cap && ripe > lane.sheep.len()
            {
                let jitter = self.rand();
                let lane = &mut self.lanes[li];
                let left = lane.next_left;
                let edge = if left { lane.x0 } else { lane.x1 };
                lane.sheep.push(Sheep {
                    x: edge as f32,
                    y: lane.y as f32,
                    facing: if left { 1 } else { -1 },
                    detour: 0.0,
                    state: State::Walk(edge),
                    eaten: 0,
                });
                lane.next_left = !left;
                lane.spawn_cd = SPAWN_COOLDOWN + jitter;
            }
            tick_lane(&mut self.lanes[li], dt, fleeing);
        }
    }

    fn seed_lane(&mut self, y: u16, x0: u16, x1: u16) -> Lane {
        let mut tufts = Vec::new();
        let mut col = x0 + 1;
        while col < x1 {
            tufts.push(Tuft {
                x: col,
                height: self.rand() * 0.6,
                grow: 0.01 + self.rand() * 0.05,
            });
            col += 2 + (self.rand() * 4.0) as u16;
        }
        Lane {
            y,
            x0,
            x1,
            tufts,
            sheep: Vec::new(),
            spawn_cd: self.rand() * SPAWN_COOLDOWN,
            next_left: self.rand() < 0.5,
        }
    }

    fn draw(&self, buf: &mut Buffer, palette: &Palette) {
        let grass_style = Style::default().fg(palette.green);
        // Set an explicit foreground so the sprite reads as a sheep and not the
        // grey of the separator line it walks on (which is what it inherits with
        // no fg of its own). `palette.text` is the theme's high-contrast main-
        // text token — woolly white on dark themes, legibly dark on light ones.
        let sheep_style = Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD);
        let (min_y, max_y) = (buf.area().top() as i32, buf.area().bottom() as i32);
        for lane in &self.lanes {
            for t in &lane.tufts {
                if let Some(glyph) = grass_glyph(t.height) {
                    buf.set_string(t.x, lane.y, glyph, grass_style);
                }
            }
            for s in &lane.sheep {
                let col = s.x.round();
                if col < lane.x0 as f32 || col > lane.x1 as f32 {
                    continue;
                }
                let row = s.y.round() as i32;
                if row < min_y || row >= max_y {
                    continue;
                }
                let (row, head) = (row as u16, col as u16);
                let wool_x = head as i32 - s.facing as i32;
                if wool_x >= lane.x0 as i32 && wool_x <= lane.x1 as i32 {
                    buf.set_string(wool_x as u16, row, WOOL, sheep_style);
                }
                buf.set_string(head, row, HEAD, sheep_style);
            }
        }
    }
}

/// One independent strip's per-tick update: claim grass (closest-wins), walk to
/// it (straying ±`BAND` rows to step around others), graze, and amble off.
fn tick_lane(lane: &mut Lane, dt: f32, fleeing: bool) {
    let (lo, hi) = (lane.x0 as f32, lane.x1 as f32);
    let line = lane.y as f32;

    if fleeing {
        // The whole scene wipes: grass withers as the flock bolts off the sides.
        for t in &mut lane.tufts {
            t.height = (t.height - GRASS_RECEDE * dt).max(0.0);
        }
        for s in &mut lane.sheep {
            let dir = if s.x - lo < hi - s.x { -1.0 } else { 1.0 };
            s.facing = dir as i8;
            s.x += dir * FLEE * dt;
        }
        lane.sheep.retain(|s| s.x >= lo - 1.0 && s.x <= hi + 1.0);
        return;
    }

    // Closest-wins claiming (by column) over ripe, un-grazed tufts.
    let eating: Vec<u16> = lane
        .sheep
        .iter()
        .filter_map(|s| match s.state {
            State::Eat(tx) => Some(tx),
            _ => None,
        })
        .collect();
    let mut ripe: Vec<u16> = lane
        .tufts
        .iter()
        .filter(|t| t.height >= RIPE && !eating.contains(&t.x))
        .map(|t| t.x)
        .collect();
    let mut free: Vec<usize> = (0..lane.sheep.len())
        .filter(|&i| matches!(lane.sheep[i].state, State::Walk(_)))
        .collect();
    let mut claims: Vec<(usize, u16)> = Vec::new();
    while !ripe.is_empty() && !free.is_empty() {
        let mut best: Option<(usize, usize, f32)> = None;
        for (fi, &si) in free.iter().enumerate() {
            for (ri, &tx) in ripe.iter().enumerate() {
                let d = (lane.sheep[si].x - tx as f32).abs();
                if best.is_none_or(|(_, _, bd)| d < bd) {
                    best = Some((fi, ri, d));
                }
            }
        }
        let (fi, ri, _) = best.unwrap();
        claims.push((free[fi], ripe[ri]));
        free.swap_remove(fi);
        ripe.swap_remove(ri);
    }
    let claim_of = |si: usize| claims.iter().find(|(s, _)| *s == si).map(|(_, tx)| *tx);

    let cells: Vec<(usize, f32, f32)> = lane
        .sheep
        .iter()
        .enumerate()
        .map(|(i, s)| (i, s.x, s.y))
        .collect();
    let occupied = |si: usize, nx: f32, ny: f32| {
        let (cx, cy) = (nx.round(), ny.round());
        cells
            .iter()
            .any(|&(j, ox, oy)| j != si && ox.round() == cx && oy.round() == cy)
    };

    for si in 0..lane.sheep.len() {
        match lane.sheep[si].state {
            State::Eat(tx) => {
                lane.sheep[si].x = tx as f32;
                lane.sheep[si].y = line;
                let done = if let Some(t) = lane.tufts.iter_mut().find(|t| t.x == tx) {
                    t.height = (t.height - EAT * dt).max(0.0);
                    t.height < CROPPED
                } else {
                    true
                };
                if done {
                    lane.sheep[si].eaten += 1;
                    lane.sheep[si].state = if lane.sheep[si].eaten >= EATEN_BEFORE_RETIRE {
                        State::Leaving
                    } else {
                        State::Walk(tx)
                    };
                }
            }
            State::Walk(_) => {
                let Some(tx) = claim_of(si) else {
                    lane.sheep[si].state = State::Leaving;
                    continue;
                };
                let (target, x, y) = (tx as f32, lane.sheep[si].x, lane.sheep[si].y);
                if (x - target).abs() <= ARRIVE && (y - line).abs() <= ARRIVE {
                    lane.sheep[si].x = target;
                    lane.sheep[si].y = line;
                    lane.sheep[si].state = State::Eat(tx);
                    continue;
                }
                let facing = if target > x { 1.0 } else { -1.0 };
                let sx = (target - x).clamp(-WALK * dt, WALK * dt);
                // Commit to a wide arc rather than jittering a row at a time: if
                // the next step along the bar is blocked, swing the full BAND to
                // the clearer side and hold it until the path ahead is clear, then
                // ease back to the line for a straight final approach.
                let mut detour = lane.sheep[si].detour;
                if occupied(si, x + sx, line) {
                    if detour.abs() < 0.5 {
                        let up_room = !occupied(si, x + sx, line - BAND);
                        detour = if up_room { -BAND } else { BAND };
                    }
                } else if (x - target).abs() > ARRIVE {
                    detour = 0.0;
                }
                let aim_y = (line + detour).clamp(line - BAND, line + BAND);
                let dy = (aim_y - y).clamp(-CLIMB * dt, CLIMB * dt);
                let (nx, ny) = (x + sx, y + dy);
                let s = &mut lane.sheep[si];
                s.facing = facing as i8;
                s.detour = detour;
                s.state = State::Walk(tx);
                if !occupied(si, nx, ny) {
                    s.x = nx;
                    s.y = ny;
                } else if !occupied(si, x, ny) {
                    // Can't gain ground yet — settle into the arc and wait.
                    s.y = ny;
                }
            }
            State::Leaving => {
                let s = &mut lane.sheep[si];
                let dir = if s.x - lo < hi - s.x { -1.0 } else { 1.0 };
                s.facing = dir as i8;
                s.x += dir * WALK * dt;
            }
        }
    }

    lane.sheep
        .retain(|s| !(matches!(s.state, State::Leaving) && (s.x < lo - 1.0 || s.x > hi + 1.0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(sim: &mut SheepSim, lanes: &[(u16, u16, u16)], secs: f32, fleeing: bool) {
        for _ in 0..(secs / 0.1) as usize {
            sim.advance(0.1, lanes, fleeing);
        }
    }

    fn sheep(x: f32, y: f32, state: State) -> Sheep {
        Sheep {
            x,
            y,
            facing: 1,
            detour: 0.0,
            state,
            eaten: 0,
        }
    }

    #[test]
    fn phase_transitions() {
        let now = Instant::now();
        assert_eq!(
            flock_phase(now - Duration::from_secs(3), None, now, IDLE_THRESHOLD),
            None
        );
        let stale = now - (IDLE_THRESHOLD + Duration::from_secs(1));
        assert_eq!(
            flock_phase(stale, None, now, IDLE_THRESHOLD),
            Some(FlockPhase::Grazing)
        );
        let until = now + FLEE_DURATION / 2;
        assert!(matches!(
            flock_phase(stale, Some(until), now, IDLE_THRESHOLD),
            Some(FlockPhase::Fleeing(_))
        ));
    }

    #[test]
    fn lane_cap_scales_with_width() {
        assert_eq!(lane_cap(8), 1);
        assert_eq!(lane_cap(20), 3);
        assert_eq!(lane_cap(40), 6);
        assert_eq!(lane_cap(200), LANE_CAP_HARD);
    }

    #[test]
    fn sheep_stay_within_their_bands_band() {
        let mut sim = SheepSim::default();
        let lanes = vec![(6u16, 0u16, 39u16)];
        for _ in 0..1500 {
            sim.advance(0.1, &lanes, false);
            for s in &sim.lanes[0].sheep {
                if !matches!(s.state, State::Leaving) {
                    assert!(
                        (s.y - 6.0).abs() <= BAND + 0.001,
                        "stays within ±{BAND} of its bar: y={}",
                        s.y
                    );
                }
            }
        }
    }

    #[test]
    fn sheep_reach_grass_and_turn_over_without_gridlock() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        let cap = lane_cap(40);
        let mut grazed = false;
        let mut saw_leaving = false;
        for _ in 0..1500 {
            sim.advance(0.1, &lanes, false);
            assert!(sim.lanes[0].sheep.len() <= cap);
            if sim.lanes[0].sheep.iter().any(|s| s.eaten > 0) {
                grazed = true;
            }
            if sim.lanes[0]
                .sheep
                .iter()
                .any(|s| matches!(s.state, State::Leaving))
            {
                saw_leaving = true;
            }
        }
        assert!(
            grazed,
            "sheep reached grass rather than blocking at the edge"
        );
        assert!(saw_leaving, "sated sheep amble off (turnover happens)");
    }

    #[test]
    fn closest_sheep_wins_a_ripe_tuft() {
        let mut lane = Lane {
            y: 1,
            x0: 0,
            x1: 39,
            tufts: vec![Tuft {
                x: 20,
                height: MAX_GRASS,
                grow: 0.0,
            }],
            sheep: vec![
                sheep(18.0, 1.0, State::Walk(0)),
                sheep(2.0, 1.0, State::Walk(0)),
            ],
            spawn_cd: 999.0,
            next_left: true,
        };
        tick_lane(&mut lane, 0.1, false);
        assert!(matches!(
            lane.sheep[0].state,
            State::Walk(20) | State::Eat(20)
        ));
        assert_eq!(lane.sheep[1].state, State::Leaving);
    }

    #[test]
    fn fleeing_clears_sheep_and_grass() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        run(&mut sim, &lanes, 70.0, false);
        assert!(
            sim.lanes[0].tufts.iter().any(|t| t.height >= RIPE),
            "grass was up before the flee"
        );
        run(&mut sim, &lanes, 2.0, true);
        assert!(
            sim.lanes[0].sheep.is_empty(),
            "every sheep bolted off the sides"
        );
        assert!(
            sim.lanes[0].tufts.iter().all(|t| t.height < SPROUT_AT),
            "grass withered away on interaction"
        );
    }
}
