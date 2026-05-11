//! Shared fixture generators for PRD-005 Slice 0a.2 benches.
//!
//! Slice 0a.1 (#346) is expected to ship a richer fixture generator
//! alongside the capture script; this module is the minimum needed to
//! make Slice 0a.2's four benches runnable end-to-end. When 0a.1 lands,
//! this can be reduced to a re-export of the canonical fixture API.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use nico_common::output::Status;
use nico_doctor::layer::{Check, CheckKind, Layer, LayerOutcome, RunOpts};
use nico_ops::action::Action;
use nico_ops::app::App;
use nico_ops::model::{Finding, LayerSnapshot, LogLine};
use nico_correlate::event::{Event, Severity};
use chrono::Utc;
use std::collections::HashMap;

/// Layer order mirroring the `nico ops` Layout A / B grid. Five backed
/// layers + one synthetic activity quadrant.
pub const LAYER_NAMES: &[&str] = &[
    "cluster",
    "workflows",
    "health",
    "postgres",
    "logs",
    "infiniband",
];

/// Build N synthetic fleet-style LayerSnapshots. Half are mixed
/// status (Warn/Fail) so the renderer exercises the finding paths;
/// the rest are Ok with a one-line evidence summary.
pub fn fleet_snapshots(n: usize) -> Vec<LayerSnapshot> {
    (0..n)
        .map(|i| {
            let status = match i % 4 {
                0 => Status::Ok,
                1 => Status::Warn,
                2 => Status::Fail,
                _ => Status::Unknown,
            };
            let name = LAYER_NAMES[i % LAYER_NAMES.len()].to_string();
            let findings = if status == Status::Ok {
                vec![]
            } else {
                (0..3)
                    .map(|k| Finding {
                        status: status.clone(),
                        message: format!("synthetic_finding_{i}_{k}: wf-{i:04}{k}"),
                        next_command: Some(format!("kubectl logs synthetic-pod-{i}-{k}")),
                        link: None,
                    })
                    .collect()
            };
            LayerSnapshot {
                name,
                status: status.clone(),
                evidence: format!("{} synthetic finding(s)", findings.len()),
                findings,
                duration_ms: ((i * 17) % 250) as u64,
            }
        })
        .collect()
}

/// Build a fleet of N synthetic LogLines for the snapshot logs panel.
pub fn fleet_log_lines(n: usize) -> Vec<LogLine> {
    (0..n)
        .map(|i| LogLine {
            ts: Utc::now(),
            pod: format!("synthetic-pod-{i}"),
            level: match i % 3 {
                0 => Status::Warn,
                1 => Status::Fail,
                _ => Status::Unknown,
            },
            message: format!("synthetic log message {i}: ERROR something happened"),
        })
        .collect()
}

/// Build a fleet of N synthetic namespace events for Layout B's Activity
/// quadrant.
pub fn fleet_namespace_events(n: usize) -> Vec<Event> {
    (0..n)
        .map(|i| Event {
            ts: Utc::now(),
            source: "k8s".into(),
            kind: "BackOff".into(),
            message: format!("synthetic event {i}: BackOff"),
            severity: Severity::Warning,
            tags: HashMap::new(),
        })
        .collect()
}

/// A synthetic Layer that returns a predetermined set of checks with
/// zero I/O. Drives the fan-out bench without any real cluster.
pub struct BenchLayer {
    name: &'static str,
    status: Status,
    finding_count: usize,
}

impl BenchLayer {
    pub fn new(name: &'static str, status: Status, finding_count: usize) -> Self {
        Self { name, status, finding_count }
    }
}

#[async_trait]
impl Layer for BenchLayer {
    fn name(&self) -> &'static str { self.name }
    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        let status = self.status.clone();
        let checks: Vec<Check> = (0..self.finding_count)
            .map(|i| Check {
                name: "bench_check",
                status: status.clone(),
                value: format!("synthetic_finding_{i}"),
                next_command: None,
                kind: CheckKind::Headline,
            })
            .collect();
        if checks.is_empty() {
            LayerOutcome::Checks(vec![Check {
                name: "bench_ok",
                status: Status::Ok,
                value: "ok".into(),
                next_command: None,
                kind: CheckKind::Headline,
            }])
        } else {
            LayerOutcome::Checks(checks)
        }
    }
}

/// Build N synthetic layers for the fan-out bench. Each layer has a
/// stable name (cycled through LAYER_NAMES with a numeric suffix for
/// uniqueness beyond 6) and returns the same shape of synthetic checks
/// as `fleet_snapshots`.
pub fn fleet_layers(n: usize) -> Arc<Vec<Box<dyn Layer>>> {
    // Box::leak gives us a 'static name without a runtime registry.
    let layers: Vec<Box<dyn Layer>> = (0..n)
        .map(|i| {
            let raw = format!("bench_layer_{i}");
            let name: &'static str = Box::leak(raw.into_boxed_str());
            let status = match i % 4 {
                0 => Status::Ok,
                1 => Status::Warn,
                2 => Status::Fail,
                _ => Status::Unknown,
            };
            let findings = if status == Status::Ok { 0 } else { 3 };
            Box::new(BenchLayer::new(name, status, findings)) as Box<dyn Layer>
        })
        .collect();
    Arc::new(layers)
}

/// Pre-warm an `App` with N fleet snapshots so subsequent benches start
/// from a steady state (post-first-paint) rather than from `App::new`.
pub fn warmed_app(n: usize) -> App {
    let mut app = App::new();
    let now = std::time::Instant::now();
    app.handle(Action::Tick(now));
    app.handle(Action::Snapshots(fleet_snapshots(n)));
    app.handle(Action::LogLines(fleet_log_lines(n.min(50))));
    app.handle(Action::NamespaceEvents(fleet_namespace_events(n.min(20))));
    app.handle(Action::Tick(now + std::time::Duration::from_millis(100)));
    app
}
