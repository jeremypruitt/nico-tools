use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Time source the dashboard consults for auto-refresh cadence and throbber
/// animation. Injected so tests can `MockClock::advance` instead of waiting
/// on real time.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Wall-clock implementation used in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test double — `Instant` cannot be constructed at an arbitrary value, so
/// the mock starts at "now-when-it-was-built" and `advance` moves forward.
#[derive(Debug, Clone)]
pub struct MockClock {
    inner: Arc<Mutex<Instant>>,
}

impl MockClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub fn advance(&self, by: Duration) {
        let mut g = self.inner.lock().unwrap();
        *g += by;
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        *self.inner.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_starts_stable() {
        let c = MockClock::new();
        let a = c.now();
        let b = c.now();
        assert_eq!(a, b);
    }

    #[test]
    fn mock_clock_advances_by_exact_duration() {
        let c = MockClock::new();
        let t0 = c.now();
        c.advance(Duration::from_secs(5));
        assert_eq!(c.now() - t0, Duration::from_secs(5));
    }

    #[test]
    fn mock_clock_clones_share_state() {
        let c = MockClock::new();
        let c2 = c.clone();
        let t0 = c.now();
        c2.advance(Duration::from_secs(1));
        assert_eq!(c.now() - t0, Duration::from_secs(1));
    }
}
