use chrono::{TimeZone, Utc};
use nico_correlate::diagnosis::Diagnosis;
use nico_correlate::event::{Event, Severity};
use nico_correlate::formatter::format_json;
use nico_correlate::source::StateEntry;
use std::collections::HashMap;

fn event(secs: i64, source: &str, kind: &str, severity: Severity) -> Event {
    Event {
        ts: Utc.timestamp_opt(secs, 0).unwrap(),
        source: source.to_string(),
        kind: kind.to_string(),
        message: format!("{source}/{kind}"),
        severity,
        tags: HashMap::new(),
    }
}

fn state_entry(source: &'static str, key: &str, value: &str) -> StateEntry {
    StateEntry { source, key: key.to_string(), value: value.to_string() }
}

fn no_sources() -> (&'static [&'static str], &'static [&'static str]) {
    (&[], &[])
}

#[test]
fn empty_timeline() {
    let (restricted, unavailable) = no_sources();
    let v: serde_json::Value =
        serde_json::from_str(&format_json("wf-abc123", "workflow", &[], restricted, unavailable, &[], None))
            .unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn one_event_per_severity() {
    let events = vec![
        event(1_700_000_000, "temporal", "WorkflowExecutionStarted", Severity::Info),
        event(1_700_000_060, "k8s", "OOMKilled", Severity::Warning),
        event(1_700_000_120, "temporal", "WorkflowExecutionFailed", Severity::Error),
    ];
    let (restricted, unavailable) = no_sources();
    let v: serde_json::Value =
        serde_json::from_str(&format_json("wf-abc123", "workflow", &events, restricted, unavailable, &[], None))
            .unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn with_diagnosis() {
    let events = vec![
        event(1_700_000_000, "temporal", "WorkflowExecutionStarted", Severity::Info),
        event(1_700_000_060, "temporal", "WorkflowExecutionFailed", Severity::Error),
    ];
    let state = vec![
        state_entry("postgres", "provision_attempt", "3"),
        state_entry("k8s", "worker-xyz", "CrashLoopBackOff (3 restarts)"),
    ];
    let diag = Diagnosis {
        pattern: "k8s_crash_loop".to_string(),
        activity: "(n/a — pod-level failure)".to_string(),
        error_signature: "pod worker-xyz in CrashLoopBackOff (3 restarts)".to_string(),
        next_commands: vec![
            "kubectl describe pod worker-xyz -n <namespace>".to_string(),
            "kubectl logs worker-xyz -n <namespace> --previous".to_string(),
        ],
    };
    let (restricted, unavailable) = no_sources();
    let v: serde_json::Value = serde_json::from_str(&format_json(
        "wf-abc123",
        "workflow",
        &events,
        restricted,
        unavailable,
        &state,
        Some(&diag),
    ))
    .unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn without_diagnosis() {
    let events = vec![
        event(1_700_000_000, "temporal", "WorkflowExecutionStarted", Severity::Info),
        event(1_700_000_300, "postgres", "provision_start", Severity::Info),
    ];
    let restricted = &["redfish"][..];
    let unavailable = &["loki"][..];
    let v: serde_json::Value =
        serde_json::from_str(&format_json("wf-abc123", "workflow", &events, restricted, unavailable, &[], None))
            .unwrap();
    insta::assert_json_snapshot!(v);
}
