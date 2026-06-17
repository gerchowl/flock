//! Idle "flock" gimmick: when the UI sits idle, grass grows organically along
//! the sidebar's separator bars; sheep peek in from the edges, wait until a tuft
//! is ripe, then race for it — closest wins, so a nearer sheep steals a claim a
//! farther one hasn't reached yet. They crop it, move to the next, and retire
//! off-screen after a few patches while fresh sheep wander in. Growth ramps up
//! over the idle spell so the scene fills out. Any interaction and the whole
//! flock bolts off the bars.
//!
//! A small time-stepped, agent-based sim. State lives in [`SheepSim`] (held in
//! `AppState`) and advances by wall-clock `dt` each render — frame-rate
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
const WALK: f32 = 3.0; // cells/sec — deliberately unhurried
const EAT: f32 = 0.9; // grass-height/sec while grazing
const FLEE: f32 = 42.0; // cells/sec bolting off
const EATEN_BEFORE_RETIRE: u32 = 4;
const MAX_PER_LANE: usize = 2;
const SPAWN_COOLDOWN: f32 = 2.0; // sec between a lane's arrivals
const RAMP_FULL_SECS: f32 = 18.0; // idle time to reach max growth speed
const RAMP_MAX: f32 = 3.0;
const MAX_STEP: f32 = 0.25; // clamp a single step after a long pause

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
/// a run of box-drawing `─`. The flock only walks on these.
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
    /// Peeking at the entry edge, waiting for a tuft to ripen.
    Peek,
    /// Heading for a claimed ripe tuft (its column).
    Walk(u16),
    /// Grazing the tuft at this column down.
    Eat(u16),
    /// Done; walking off the nearest edge.
    Retire,
}

#[derive(Debug, Clone)]
struct Sheep {
    x: f32,
    /// +1 facing/heading right, -1 left. The wool body trails the head.
    facing: i8,
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
    entry_left: bool,
}

/// Persistent idle-flock simulation, held in `AppState` and stepped each render.
#[derive(Debug)]
pub struct SheepSim {
    lanes: Vec<Lane>,
    signature: Vec<(u16, u16, u16)>,
    last_step: Option<Instant>,
    /// Seconds the current idle spell has been grazing (drives the growth ramp).
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

        // Returning to idle after a flee: send in a fresh flock, grass intact.
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
            // Trickle a new sheep in (alternating edges) once there's foreseeable
            // food and the lane isn't crowded.
            let spawn = {
                let lane = &self.lanes[li];
                !fleeing
                    && lane.sheep.len() < MAX_PER_LANE
                    && lane.spawn_cd <= 0.0
                    && lane.tufts.iter().any(|t| t.height > 0.15)
            };
            if spawn {
                let jitter = self.rand();
                let lane = &mut self.lanes[li];
                let left = lane.entry_left;
                lane.sheep.push(Sheep {
                    x: if left { lane.x0 as f32 } else { lane.x1 as f32 },
                    facing: if left { 1 } else { -1 },
                    state: State::Peek,
                    eaten: 0,
                });
                lane.entry_left = !left;
                lane.spawn_cd = SPAWN_COOLDOWN + jitter;
            }
            self.lanes[li].spawn_cd -= dt;
            tick_lane(&mut self.lanes[li], dt, fleeing, ramp);
        }
    }

    fn seed_lane(&mut self, y: u16, x0: u16, x1: u16) -> Lane {
        // Tufts at organically-spaced columns, each with its own start height
        // and pace, so the bar greens up unevenly.
        let mut tufts = Vec::new();
        let mut col = x0 + 1;
        while col < x1 {
            tufts.push(Tuft {
                x: col,
                height: self.rand() * 0.6,
                grow: 0.05 + self.rand() * 0.28,
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
            entry_left: self.rand() < 0.5,
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
                if col < lane.x0 as f32 || col > lane.x1 as f32 {
                    continue;
                }
                let head = col as u16;
                // Wool trails opposite the heading; off-bar (e.g. a peeking sheep
                // at the very edge) just shows the head poking in.
                let wool_x = head as i32 - s.facing as i32;
                if wool_x >= lane.x0 as i32 && wool_x <= lane.x1 as i32 {
                    buf.set_string(wool_x as u16, lane.y, WOOL, sheep_style);
                }
                buf.set_string(head, lane.y, HEAD, sheep_style);
            }
        }
    }
}

/// One lane's per-tick update: grow grass, then run the flock AI.
fn tick_lane(lane: &mut Lane, dt: f32, fleeing: bool, ramp: f32) {
    let (lo, hi) = (lane.x0 as f32, lane.x1 as f32);

    if !fleeing {
        for t in &mut lane.tufts {
            t.height = (t.height + t.grow * ramp * dt).min(MAX_GRASS);
        }
    }

    if fleeing {
        for s in &mut lane.sheep {
            let dir = if s.x - lo < hi - s.x { -1.0 } else { 1.0 };
            s.facing = dir as i8;
            s.x += dir * FLEE * dt;
        }
        return;
    }

    // Closest-wins claiming: a tuft goes to the nearest free (Peek/Walk) sheep,
    // so a newcomer can steal a claim a farther sheep hasn't reached yet. Sheep
    // already Eating keep their tuft.
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
        .filter(|&i| matches!(lane.sheep[i].state, State::Peek | State::Walk(_)))
        .collect();
    // Greedy nearest pairing.
    let mut claims: Vec<(usize, u16)> = Vec::new();
    while !ripe.is_empty() && !free.is_empty() {
        let mut best: Option<(usize, usize, f32)> = None; // (free_i, ripe_i, dist)
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

    for si in 0..lane.sheep.len() {
        match lane.sheep[si].state {
            State::Eat(tx) => {
                lane.sheep[si].x = tx as f32;
                let done = if let Some(t) = lane.tufts.iter_mut().find(|t| t.x == tx) {
                    t.height = (t.height - EAT * dt).max(0.0);
                    t.height < CROPPED
                } else {
                    true
                };
                if done {
                    lane.sheep[si].eaten += 1;
                    lane.sheep[si].state = if lane.sheep[si].eaten >= EATEN_BEFORE_RETIRE {
                        State::Retire
                    } else {
                        State::Peek
                    };
                }
            }
            State::Peek | State::Walk(_) => {
                if let Some(tx) = claim_of(si) {
                    let target = tx as f32;
                    let s = &mut lane.sheep[si];
                    if (s.x - target).abs() > 0.6 {
                        s.facing = if target > s.x { 1 } else { -1 };
                        s.x += s.facing as f32 * WALK * dt;
                        s.state = State::Walk(tx);
                    } else {
                        s.x = target;
                        s.state = State::Eat(tx);
                    }
                } else {
                    // Nothing ripe to chase: wait, peeking at the entry edge.
                    lane.sheep[si].state = State::Peek;
                }
            }
            State::Retire => {
                let s = &mut lane.sheep[si];
                let dir = if s.x - lo < hi - s.x { -1.0 } else { 1.0 };
                s.facing = dir as i8;
                s.x += dir * WALK * dt;
            }
        }
    }

    // Reap sheep that have retired off the bar.
    lane.sheep
        .retain(|s| !(matches!(s.state, State::Retire) && (s.x < lo - 1.0 || s.x > hi + 1.0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(sim: &mut SheepSim, lanes: &[(u16, u16, u16)], secs: f32, fleeing: bool) {
        let steps = (secs / 0.1) as usize;
        for _ in 0..steps {
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
    fn grass_ripens_and_sheep_arrive_and_eat() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        run(&mut sim, &lanes, 40.0, false);
        let lane = &sim.lanes[0];
        assert!(!lane.sheep.is_empty(), "sheep wandered in");
        // Something got grazed: at least one sheep is eating or has eaten.
        assert!(
            lane.sheep
                .iter()
                .any(|s| matches!(s.state, State::Eat(_)) || s.eaten > 0),
            "a sheep reached and grazed grass"
        );
    }

    #[test]
    fn sheep_wait_at_edge_until_grass_is_ripe() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        // Force a sheep in immediately, but keep all grass below ripe.
        sim.advance(0.1, &lanes, false);
        for t in &mut sim.lanes[0].tufts {
            t.height = RIPE - 0.3;
            t.grow = 0.0;
        }
        sim.lanes[0].sheep.push(Sheep {
            x: 0.0,
            facing: 1,
            state: State::Peek,
            eaten: 0,
        });
        run(&mut sim, &lanes, 3.0, false);
        // No ripe grass → the sheep never leaves Peek and stays put at the edge.
        let s = &sim.lanes[0].sheep[sim.lanes[0].sheep.len() - 1];
        assert_eq!(s.state, State::Peek);
        assert!(s.x <= 1.0, "still peeking at the edge: x={}", s.x);
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
                Sheep {
                    x: 18.0,
                    facing: 1,
                    state: State::Peek,
                    eaten: 0,
                }, // near
                Sheep {
                    x: 2.0,
                    facing: 1,
                    state: State::Peek,
                    eaten: 0,
                }, // far
            ],
            spawn_cd: 999.0,
            entry_left: true,
        };
        tick_lane(&mut lane, 0.1, false, 1.0);
        // The near sheep heads for / reaches the tuft; the far one keeps peeking.
        assert!(matches!(
            lane.sheep[0].state,
            State::Walk(20) | State::Eat(20)
        ));
        assert_eq!(lane.sheep[1].state, State::Peek);
    }

    #[test]
    fn sheep_retire_after_enough_patches() {
        let mut lane = Lane {
            y: 1,
            x0: 0,
            x1: 39,
            tufts: vec![Tuft {
                x: 5,
                height: MAX_GRASS,
                grow: 0.0,
            }],
            sheep: vec![Sheep {
                x: 5.0,
                facing: 1,
                state: State::Eat(5),
                eaten: EATEN_BEFORE_RETIRE - 1,
            }],
            spawn_cd: 999.0,
            entry_left: true,
        };
        // Graze the (non-regrowing) tuft to nothing → counts the last patch.
        for _ in 0..40 {
            tick_lane(&mut lane, 0.1, false, 1.0);
        }
        assert!(
            lane.sheep.is_empty() || matches!(lane.sheep[0].state, State::Retire),
            "sated sheep retires"
        );
    }

    #[test]
    fn fleeing_clears_the_bar() {
        let mut sim = SheepSim::default();
        let lanes = vec![(1u16, 0u16, 39u16)];
        run(&mut sim, &lanes, 40.0, false);
        run(&mut sim, &lanes, 2.0, true);
        let lane = &sim.lanes[0];
        assert!(
            lane.sheep
                .iter()
                .all(|s| s.x < lane.x0 as f32 || s.x > lane.x1 as f32),
            "every sheep bolted off the bar"
        );
    }
}
