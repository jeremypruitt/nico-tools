use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, Local};
use nico_common::output::Status;

/// Maximum number of `RunSnapshot`s the dashboard retains in memory.
/// Capped at 32 — the buffer feeds future visualizations (sparklines,
/// breadcrumbs, pulse) which only need a recent window. No disk
/// persistence; oldest entries are evicted on overflow.
pub const RING_CAPACITY: usize = 32;

/// Per-layer slice of a completed dashboard run, kept small so the ring
/// stays cheap to clone for visualizations.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerStat {
    pub name: String,
    pub status: Status,
    pub finding_count: usize,
    pub duration_ms: u64,
}

/// One completed refresh round across all layers.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSnapshot {
    pub timestamp: DateTime<Local>,
    pub total_duration: Duration,
    pub layers: Vec<LayerStat>,
}

/// Bounded FIFO of `RunSnapshot`s. Push-on-the-back; the oldest entry is
/// evicted from the front when capacity is exceeded.
#[derive(Debug, Clone, Default)]
pub struct RingBuffer {
    inner: VecDeque<RunSnapshot>,
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            inner: VecDeque::with_capacity(RING_CAPACITY),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn push(&mut self, snapshot: RunSnapshot) {
        if self.inner.len() == RING_CAPACITY {
            self.inner.pop_front();
        }
        self.inner.push_back(snapshot);
    }

    /// Most-recent snapshot, if any.
    pub fn latest(&self) -> Option<&RunSnapshot> {
        self.inner.back()
    }

    /// Iterate oldest → newest.
    pub fn iter(&self) -> impl Iterator<Item = &RunSnapshot> {
        self.inner.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(layers: Vec<(&str, Status, usize, u64)>) -> RunSnapshot {
        RunSnapshot {
            timestamp: Local::now(),
            total_duration: Duration::from_millis(0),
            layers: layers
                .into_iter()
                .map(|(n, s, fc, d)| LayerStat {
                    name: n.into(),
                    status: s,
                    finding_count: fc,
                    duration_ms: d,
                })
                .collect(),
        }
    }

    #[test]
    fn fresh_ring_is_empty() {
        let r = RingBuffer::new();
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert!(r.latest().is_none());
    }

    #[test]
    fn push_appends_and_latest_is_most_recent() {
        let mut r = RingBuffer::new();
        let first = snap(vec![("cluster", Status::Ok, 0, 10)]);
        let second = snap(vec![("cluster", Status::Warn, 1, 20)]);
        r.push(first.clone());
        r.push(second.clone());
        assert_eq!(r.len(), 2);
        assert_eq!(r.latest(), Some(&second));
    }

    #[test]
    fn ring_caps_at_32_and_evicts_oldest() {
        let mut r = RingBuffer::new();
        for i in 0..40u64 {
            r.push(snap(vec![("cluster", Status::Ok, 0, i)]));
        }
        assert_eq!(r.len(), RING_CAPACITY);
        let oldest = r.iter().next().unwrap();
        // After 40 pushes with capacity 32, the oldest retained run is #8.
        assert_eq!(oldest.layers[0].duration_ms, 8);
        assert_eq!(r.latest().unwrap().layers[0].duration_ms, 39);
    }

    #[test]
    fn iter_yields_oldest_to_newest() {
        let mut r = RingBuffer::new();
        for i in 0..3u64 {
            r.push(snap(vec![("cluster", Status::Ok, 0, i)]));
        }
        let durations: Vec<u64> = r.iter().map(|s| s.layers[0].duration_ms).collect();
        assert_eq!(durations, vec![0, 1, 2]);
    }
}
