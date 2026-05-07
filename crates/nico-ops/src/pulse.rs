//! Pure-data per-layer pulse timer for the status-flip flash.
//!
//! When a layer's status flips between consecutive in-process refreshes,
//! its scorecard pip flashes for a short window. This module owns the
//! timing model and stays free of `ratatui` types so the renderer can read
//! `is_active(now)` to decide whether to apply the pulse modifier.

use std::time::{Duration, Instant};

/// Total length of one pulse window.
pub const PULSE_DURATION: Duration = Duration::from_millis(600);

#[derive(Debug, Default, Clone)]
pub struct PulseTimer {
    started_at: Option<Instant>,
}

impl PulseTimer {
    pub fn new() -> Self {
        Self { started_at: None }
    }

    /// Begin a fresh pulse window starting at `now`. Restarts the clock if
    /// a pulse was already running.
    pub fn start(&mut self, now: Instant) {
        self.started_at = Some(now);
    }

    /// True while `now` falls inside `[start, start + PULSE_DURATION)`.
    pub fn is_active(&self, now: Instant) -> bool {
        match self.started_at {
            Some(s) => now.saturating_duration_since(s) < PULSE_DURATION,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_timer_is_inactive() {
        let t = PulseTimer::new();
        assert!(!t.is_active(Instant::now()));
    }

    #[test]
    fn active_immediately_after_start() {
        let mut t = PulseTimer::new();
        let now = Instant::now();
        t.start(now);
        assert!(t.is_active(now));
    }

    #[test]
    fn active_inside_pulse_window() {
        let mut t = PulseTimer::new();
        let now = Instant::now();
        t.start(now);
        assert!(t.is_active(now + Duration::from_millis(300)));
        assert!(t.is_active(now + Duration::from_millis(599)));
    }

    #[test]
    fn inactive_after_pulse_window_elapses() {
        let mut t = PulseTimer::new();
        let now = Instant::now();
        t.start(now);
        assert!(!t.is_active(now + Duration::from_millis(600)));
        assert!(!t.is_active(now + Duration::from_millis(900)));
    }

    #[test]
    fn restart_extends_the_pulse_from_the_new_start() {
        let mut t = PulseTimer::new();
        let now = Instant::now();
        t.start(now);
        // First pulse would have ended at now+600
        let later = now + Duration::from_millis(500);
        t.start(later);
        // Now the pulse runs until later+600 = now+1100.
        assert!(t.is_active(now + Duration::from_millis(1000)));
        assert!(!t.is_active(now + Duration::from_millis(1100)));
    }
}
