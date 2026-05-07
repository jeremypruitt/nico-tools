use std::collections::HashMap;
use nico_common::output::Status;
use nico_doctor::baseline::Delta;
use nico_doctor::formatter::format_json;
use nico_doctor::layer::{Check, CheckKind, LayerResult};
use nico_doctor::runner::Report;

fn no_deltas() -> HashMap<String, Delta> {
    HashMap::new()
}

fn ok_check(name: &'static str, value: &str) -> Check {
    Check { name, status: Status::Ok, value: value.to_string(), next_command: None, kind: CheckKind::Headline }
}

fn warn_check(name: &'static str, value: &str) -> Check {
    Check { name, status: Status::Warn, value: value.to_string(), next_command: Some(format!("kubectl get {name}")), kind: CheckKind::Headline }
}

fn fail_check(name: &'static str, value: &str) -> Check {
    Check { name, status: Status::Fail, value: value.to_string(), next_command: Some(format!("kubectl describe {name}")), kind: CheckKind::Headline }
}

fn layer_from_checks(name: &'static str, checks: Vec<Check>) -> LayerResult {
    let status = if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Ok
    };
    LayerResult { name, status, checks, duration_ms: 42 }
}

fn skipped(name: &'static str) -> LayerResult {
    LayerResult { name, status: Status::Skipped, checks: vec![], duration_ms: 0 }
}

fn unknown_timeout(name: &'static str) -> LayerResult {
    LayerResult { name, status: Status::Unknown, checks: vec![], duration_ms: 5000 }
}

fn all_ok_report() -> Report {
    Report {
        layers: vec![
            layer_from_checks("cluster", vec![
                ok_check("pods_ready", "3/3"),
                ok_check("recent_restarts", "0"),
                ok_check("warning_events", "0"),
            ]),
            layer_from_checks("logs", vec![
                ok_check("error_lines", "0 errors"),
                ok_check("source", "loki"),
            ]),
            layer_from_checks("workflows", vec![
                ok_check("stuck", "0 stuck"),
                ok_check("failed", "0 failed"),
            ]),
        ],
    }
}

#[test]
fn all_ok() {
    let report = all_ok_report();
    let v: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn warn_only() {
    let report = Report {
        layers: vec![
            layer_from_checks("cluster", vec![
                warn_check("pods_ready", "2/3"),
                ok_check("recent_restarts", "0"),
            ]),
            layer_from_checks("postgres", vec![
                warn_check("pool", "pool 18/20 in-use"),
                ok_check("locks", "0 lock waits"),
            ]),
        ],
    };
    let v: serde_json::Value = serde_json::from_str(&format_json(&report, "staging", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn fail_report() {
    let report = Report {
        layers: vec![
            layer_from_checks("health", vec![
                fail_check("endpoints", "1/2 healthy, 1 failed"),
                fail_check("service", "core /healthz failed"),
            ]),
            layer_from_checks("grpc", vec![
                fail_check("reachable", "unreachable"),
            ]),
        ],
    };
    let v: serde_json::Value = serde_json::from_str(&format_json(&report, "prod", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn skipped_layer() {
    let report = Report {
        layers: vec![
            layer_from_checks("cluster", vec![ok_check("pods_ready", "2/2")]),
            skipped("logs"),
            skipped("grpc"),
            layer_from_checks("postgres", vec![ok_check("pool", "pool 5/20 in-use")]),
        ],
    };
    let v: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
    insta::assert_json_snapshot!(v);
}

#[test]
fn unknown_timeout_layer() {
    let report = Report {
        layers: vec![
            layer_from_checks("cluster", vec![ok_check("pods_ready", "2/2")]),
            unknown_timeout("workflows"),
            unknown_timeout("grpc"),
        ],
    };
    let v: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
    insta::assert_json_snapshot!(v);
}
