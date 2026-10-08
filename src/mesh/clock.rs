//! Persistent unpaused time, independent of clocks on other hosts.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    pub wall_ms: i64,
    pub elapsed_ms: i64,
    pub paused: bool,
}

impl Clock {
    pub fn new(wall_ms: i64) -> Self {
        Self {
            wall_ms,
            elapsed_ms: 0,
            paused: false,
        }
    }

    /// Rollback cannot revive mail. An unpaused forward jump deliberately
    /// consumes budget, even if the wall clock is subsequently corrected.
    pub fn advance(&mut self, wall_ms: i64) -> i64 {
        if !self.paused {
            self.elapsed_ms = self
                .elapsed_ms
                .saturating_add(wall_ms.saturating_sub(self.wall_ms).max(0));
            self.wall_ms = self.wall_ms.max(wall_ms);
        }
        self.elapsed_ms
    }

    pub fn set_paused(&mut self, paused: bool, wall_ms: i64) {
        if self.paused && !paused {
            // Rebase only the wall anchor, never the logical clock. Jumps and
            // corrections during pause consume no custody or retention time.
            self.wall_ms = wall_ms;
        } else {
            self.advance(wall_ms);
        }
        self.paused = paused;
    }
}
