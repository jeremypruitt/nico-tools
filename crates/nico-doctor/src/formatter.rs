use std::collections::HashMap;
use nico_common::output::{OutputMode, Status};
use crate::baseline::Delta;
use crate::layer::CheckKind;
use crate::runner::Report;

/// Maximum detail bullets rendered per layer in the default human-mode findings
/// block. See ADR-0003 (2026-05-07 amendment) and issue #179. `--verbose`
/// bypasses the cap; `--json` is unaffected.
pub const FINDINGS_CAP: usize = 5;

pub fn format_report(
    report: &Report,
    mode: &OutputMode,
    verbose: bool,
    deltas: &HashMap<String, Delta>,
    spotlight: bool,
) -> String {
    let mut out = String::new();

    for layer in &report.layers {
        let delta = deltas.get(layer.name).copied().unwrap_or(Delta::Unchanged);

        if spotlight {
            let is_quiet = matches!(layer.status, Status::Ok | Status::Skipped);
            if is_quiet && delta == Delta::Unchanged {
                continue;
            }
        }

        let icon = layer.status.icon(mode);
        let styled_icon = layer.status.style(icon, mode);
        let summary = if layer.status == Status::Skipped {
            match &layer.skipped_reason {
                Some(reason) => format!("(skipped — {reason})"),
                None => "(skipped)".to_string(),
            }
        } else {
            layer.checks.iter()
                .filter(|c| c.kind == CheckKind::Headline)
                .map(|c| c.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let badge = match delta {
            Delta::New => " [NEW]",
            Delta::Fixed => " [FIXED]",
            Delta::Unchanged => "",
        };
        out.push_str(&format!("  {} {:<12} {}{}\n", styled_icon, layer.name, summary, badge));

        if verbose && layer.status != Status::Skipped {
            for check in &layer.checks {
                let check_icon = check.status.icon(mode);
                let styled_check = check.status.style(check_icon, mode);
                out.push_str(&format!("      {} {:<14} {}\n", styled_check, check.name, check.value));
                if let Some(cmd) = &check.next_command {
                    out.push_str(&format!("        → {}\n", cmd));
                }
            }
        }
    }

    if !verbose {
        let has_findings = report.layers.iter().any(|l| {
            l.checks.iter().any(|c| c.status != Status::Ok)
        });

        if has_findings {
            out.push('\n');
            for layer in &report.layers {
                if layer.status == Status::Skipped { continue; }
                let bad: Vec<_> = layer.checks.iter()
                    .filter(|c| c.status != Status::Ok)
                    .collect();
                if bad.is_empty() { continue; }
                out.push_str(&format!("{}:\n", layer.name));
                let total = bad.len();
                let shown = total.min(FINDINGS_CAP);
                for check in &bad[..shown] {
                    out.push_str(&format!("  • {} ({})\n", check.value, check.name));
                    if let Some(cmd) = &check.next_command {
                        out.push_str(&format!("    → {}\n", cmd));
                    }
                }
                if total > shown {
                    out.push_str(&format!("  … +{} more · --verbose for full list\n", total - shown));
                }
            }
        }
    }

    out.push('\n');
    let status = report.summary_status();
    let icon = status.icon(mode);
    let styled = status.style(icon, mode);
    let warn_count = report.layers.iter()
        .flat_map(|l| &l.checks)
        .filter(|c| c.status == Status::Warn)
        .count();
    let fail_count = report.layers.iter()
        .flat_map(|l| &l.checks)
        .filter(|c| c.status == Status::Fail)
        .count();
    out.push_str(&format!("Summary: {}  {} warnings, {} failures\n", styled, warn_count, fail_count));
    out.push_str("Hint: --verbose for details on passing checks, --json for machine output\n");

    out
}

pub fn format_json(
    report: &Report,
    namespace: &str,
    preflight: serde_json::Value,
    deltas: &HashMap<String, Delta>,
) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "namespace": namespace,
        "preflight": preflight,
        "summary": {
            "ok": report.layers.iter().filter(|l| l.status == Status::Ok).count(),
            "warn": report.layers.iter().filter(|l| l.status == Status::Warn).count(),
            "fail": report.layers.iter().filter(|l| l.status == Status::Fail).count(),
            "skipped": report.layers.iter().filter(|l| l.status == Status::Skipped).count(),
            "unknown": report.layers.iter().filter(|l| l.status == Status::Unknown).count(),
        },
        "layers": report.layers.iter().map(|l| {
            let delta = deltas.get(l.name).copied().unwrap_or(Delta::Unchanged);
            let delta_str = match delta {
                Delta::New => "new",
                Delta::Fixed => "fixed",
                Delta::Unchanged => "unchanged",
            };
            serde_json::json!({
                "name": l.name,
                "status": format!("{:?}", l.status).to_lowercase(),
                "delta": delta_str,
                "duration_ms": l.duration_ms,
                "skipped_reason": l.skipped_reason,
                "checks": l.checks.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "status": format!("{:?}", c.status).to_lowercase(),
                    "value": c.value,
                    "kind": match c.kind {
                        CheckKind::Headline => "headline",
                        CheckKind::Detail => "detail",
                    },
                })).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    })).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::Delta;
    use crate::layer::{Check, CheckKind, LayerResult};
    use crate::runner::Report;

    fn plain() -> OutputMode {
        OutputMode { color: false, ascii: true }
    }

    fn no_deltas() -> HashMap<String, Delta> {
        HashMap::new()
    }

    fn single_delta(name: &str, delta: Delta) -> HashMap<String, Delta> {
        let mut m = HashMap::new();
        m.insert(name.to_string(), delta);
        m
    }

    fn ok_check(name: &'static str, value: &str) -> Check {
        Check { name, status: Status::Ok, value: value.to_string(), next_command: None, kind: CheckKind::Headline }
    }

    fn warn_check(name: &'static str, value: &str, cmd: Option<&str>) -> Check {
        Check { name, status: Status::Warn, value: value.to_string(), next_command: cmd.map(str::to_string), kind: CheckKind::Headline }
    }

    fn fail_check(name: &'static str, value: &str, cmd: Option<&str>) -> Check {
        Check { name, status: Status::Fail, value: value.to_string(), next_command: cmd.map(str::to_string), kind: CheckKind::Headline }
    }

    fn detail_warn(name: &'static str, value: &str, cmd: Option<&str>) -> Check {
        Check { name, status: Status::Warn, value: value.to_string(), next_command: cmd.map(str::to_string), kind: CheckKind::Detail }
    }

    fn layer(name: &'static str, checks: Vec<Check>) -> LayerResult {
        let status = if checks.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else if checks.iter().any(|c| c.status == Status::Warn) {
            Status::Warn
        } else {
            Status::Ok
        };
        LayerResult { name, status, checks, duration_ms: 0, skipped_reason: None }
    }

    fn skipped(name: &'static str) -> LayerResult {
        LayerResult { name, status: Status::Skipped, checks: vec![], duration_ms: 0, skipped_reason: None }
    }

    fn skipped_with_reason(name: &'static str, reason: &str) -> LayerResult {
        LayerResult {
            name,
            status: Status::Skipped,
            checks: vec![],
            duration_ms: 0,
            skipped_reason: Some(reason.to_string()),
        }
    }

    fn all_ok_report() -> Report {
        Report { layers: vec![
            layer("cluster", vec![
                ok_check("pods_ready", "2/2"),
                ok_check("recent_restarts", "0"),
                ok_check("warning_events", "0"),
            ]),
            layer("logs", vec![
                ok_check("error_lines", "0 errors"),
                ok_check("source", "loki"),
            ]),
            layer("workflows", vec![
                ok_check("stuck", "0 stuck"),
                ok_check("failed", "0 failed"),
            ]),
            layer("health", vec![
                ok_check("endpoints", "2/2 healthy"),
            ]),
            layer("grpc", vec![
                ok_check("reachable", "reachable"),
                ok_check("services", "3 services"),
                ok_check("methods", "21 methods"),
            ]),
            layer("postgres", vec![
                ok_check("pool", "pool 5/20 in-use"),
                ok_check("locks", "0 lock waits"),
            ]),
        ]}
    }

    // ── cluster layer snapshots ───────────────────────────────────────────────

    #[test]
    fn cluster_ok_human() {
        let report = Report { layers: vec![layer("cluster", vec![
            ok_check("pods_ready", "2/2"),
            ok_check("recent_restarts", "0"),
            ok_check("warning_events", "0"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out,
            "  ok cluster      2/2, 0, 0\n\
             \n\
             Summary: ok  0 warnings, 0 failures\n\
             Hint: --verbose for details on passing checks, --json for machine output\n"
        );
    }

    #[test]
    fn cluster_warn_human() {
        let report = Report { layers: vec![layer("cluster", vec![
            warn_check("pods_ready", "1/2", Some("kubectl get pods -n nico | grep -v Running")),
            ok_check("recent_restarts", "0"),
            ok_check("warning_events", "0"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  warn cluster      1/2, 0, 0\n",
            "\n",
            "cluster:\n",
            "  • 1/2 (pods_ready)\n",
            "    → kubectl get pods -n nico | grep -v Running\n",
            "\n",
            "Summary: warn  1 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn cluster_ok_json() {
        let report = Report { layers: vec![layer("cluster", vec![
            ok_check("pods_ready", "2/2"),
            ok_check("recent_restarts", "0"),
            ok_check("warning_events", "0"),
        ])] };
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["namespace"], "nico");
        assert_eq!(json["summary"]["ok"], 1);
        assert_eq!(json["summary"]["warn"], 0);
        assert_eq!(json["summary"]["fail"], 0);
        assert_eq!(json["summary"]["skipped"], 0);
        assert_eq!(json["summary"]["unknown"], 0);
        let layer = &json["layers"][0];
        assert_eq!(layer["name"], "cluster");
        assert_eq!(layer["status"], "ok");
        assert_eq!(layer["checks"][0]["name"], "pods_ready");
        assert_eq!(layer["checks"][0]["value"], "2/2");
    }

    // ── logs layer snapshots ──────────────────────────────────────────────────

    #[test]
    fn logs_ok_human() {
        let report = Report { layers: vec![layer("logs", vec![
            ok_check("error_lines", "0 errors"),
            ok_check("source", "loki"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out,
            "  ok logs         0 errors, loki\n\
             \n\
             Summary: ok  0 warnings, 0 failures\n\
             Hint: --verbose for details on passing checks, --json for machine output\n"
        );
    }

    #[test]
    fn logs_warn_human() {
        let report = Report { layers: vec![layer("logs", vec![
            warn_check("error_lines", "2 errors", None),
            ok_check("source", "loki"),
            detail_warn("pod_error", "core-abc: ERROR: disk full", Some("kubectl logs core-abc -n nico")),
            detail_warn("pod_error", "rest-xyz: FATAL: oom", Some("kubectl logs rest-xyz -n nico")),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  warn logs         2 errors, loki\n",
            "\n",
            "logs:\n",
            "  • 2 errors (error_lines)\n",
            "  • core-abc: ERROR: disk full (pod_error)\n",
            "    → kubectl logs core-abc -n nico\n",
            "  • rest-xyz: FATAL: oom (pod_error)\n",
            "    → kubectl logs rest-xyz -n nico\n",
            "\n",
            "Summary: warn  3 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn logs_ok_json() {
        let report = Report { layers: vec![layer("logs", vec![
            ok_check("error_lines", "0 errors"),
            ok_check("source", "loki"),
        ])] };
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        let layer = &json["layers"][0];
        assert_eq!(layer["name"], "logs");
        assert_eq!(layer["status"], "ok");
        assert_eq!(layer["checks"].as_array().unwrap().len(), 2);
    }

    // ── workflows layer snapshots ─────────────────────────────────────────────

    #[test]
    fn workflows_ok_human() {
        let report = Report { layers: vec![layer("workflows", vec![
            ok_check("stuck", "0 stuck"),
            ok_check("failed", "0 failed"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out,
            "  ok workflows    0 stuck, 0 failed\n\
             \n\
             Summary: ok  0 warnings, 0 failures\n\
             Hint: --verbose for details on passing checks, --json for machine output\n"
        );
    }

    #[test]
    fn workflows_warn_human() {
        let report = Report { layers: vec![layer("workflows", vec![
            warn_check("stuck", "1 stuck", None),
            ok_check("failed", "0 failed"),
            detail_warn("stuck_workflow", "wf-001 (HostProvisioning): 47m running, last: ActivityScheduled",
                Some("temporal workflow show -w wf-001")),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  warn workflows    1 stuck, 0 failed\n",
            "\n",
            "workflows:\n",
            "  • 1 stuck (stuck)\n",
            "  • wf-001 (HostProvisioning): 47m running, last: ActivityScheduled (stuck_workflow)\n",
            "    → temporal workflow show -w wf-001\n",
            "\n",
            "Summary: warn  2 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn workflows_ok_json() {
        let report = Report { layers: vec![layer("workflows", vec![
            ok_check("stuck", "0 stuck"),
            ok_check("failed", "0 failed"),
        ])] };
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        let layer = &json["layers"][0];
        assert_eq!(layer["name"], "workflows");
        assert_eq!(layer["status"], "ok");
    }

    // ── health layer snapshots ────────────────────────────────────────────────

    #[test]
    fn health_ok_human() {
        let report = Report { layers: vec![layer("health", vec![
            ok_check("endpoints", "2/2 healthy"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out,
            "  ok health       2/2 healthy\n\
             \n\
             Summary: ok  0 warnings, 0 failures\n\
             Hint: --verbose for details on passing checks, --json for machine output\n"
        );
    }

    #[test]
    fn health_fail_human() {
        let report = Report { layers: vec![layer("health", vec![
            fail_check("endpoints", "1/2 healthy, 0 degraded, 1 failed", None),
            fail_check("service", "core /healthz failed", Some("curl -s http://core:8080/healthz")),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  fail health       1/2 healthy, 0 degraded, 1 failed, core /healthz failed\n",
            "\n",
            "health:\n",
            "  • 1/2 healthy, 0 degraded, 1 failed (endpoints)\n",
            "  • core /healthz failed (service)\n",
            "    → curl -s http://core:8080/healthz\n",
            "\n",
            "Summary: fail  0 warnings, 2 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn health_ok_json() {
        let report = Report { layers: vec![layer("health", vec![
            ok_check("endpoints", "2/2 healthy"),
        ])] };
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        let layer = &json["layers"][0];
        assert_eq!(layer["name"], "health");
        assert_eq!(layer["status"], "ok");
    }

    // ── grpc layer snapshots ──────────────────────────────────────────────────

    #[test]
    fn grpc_ok_human() {
        let report = Report { layers: vec![layer("grpc", vec![
            ok_check("reachable", "reachable"),
            ok_check("services", "3 services"),
            ok_check("methods", "21 methods"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out,
            "  ok grpc         reachable, 3 services, 21 methods\n\
             \n\
             Summary: ok  0 warnings, 0 failures\n\
             Hint: --verbose for details on passing checks, --json for machine output\n"
        );
    }

    #[test]
    fn grpc_fail_human() {
        let report = Report { layers: vec![layer("grpc", vec![
            fail_check("reachable", "unreachable", Some("grpcurl -plaintext localhost:50051 list")),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  fail grpc         unreachable\n",
            "\n",
            "grpc:\n",
            "  • unreachable (reachable)\n",
            "    → grpcurl -plaintext localhost:50051 list\n",
            "\n",
            "Summary: fail  0 warnings, 1 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn grpc_ok_json() {
        let report = Report { layers: vec![layer("grpc", vec![
            ok_check("reachable", "reachable"),
            ok_check("services", "3 services"),
            ok_check("methods", "21 methods"),
        ])] };
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        let layer = &json["layers"][0];
        assert_eq!(layer["name"], "grpc");
        assert_eq!(layer["status"], "ok");
        assert_eq!(layer["checks"][1]["value"], "3 services");
    }

    // ── postgres layer snapshots ──────────────────────────────────────────────

    #[test]
    fn postgres_ok_human() {
        let report = Report { layers: vec![layer("postgres", vec![
            ok_check("pool", "pool 5/20 in-use"),
            ok_check("locks", "0 lock waits"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out,
            "  ok postgres     pool 5/20 in-use, 0 lock waits\n\
             \n\
             Summary: ok  0 warnings, 0 failures\n\
             Hint: --verbose for details on passing checks, --json for machine output\n"
        );
    }

    #[test]
    fn postgres_warn_human() {
        let report = Report { layers: vec![layer("postgres", vec![
            warn_check("pool", "pool 18/20 in-use",
                Some("SELECT * FROM pg_stat_activity WHERE state != 'idle' ORDER BY query_start")),
            ok_check("locks", "0 lock waits"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  warn postgres     pool 18/20 in-use, 0 lock waits\n",
            "\n",
            "postgres:\n",
            "  • pool 18/20 in-use (pool)\n",
            "    → SELECT * FROM pg_stat_activity WHERE state != 'idle' ORDER BY query_start\n",
            "\n",
            "Summary: warn  1 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn postgres_ok_json() {
        let report = Report { layers: vec![layer("postgres", vec![
            ok_check("pool", "pool 5/20 in-use"),
            ok_check("locks", "0 lock waits"),
        ])] };
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        let layer = &json["layers"][0];
        assert_eq!(layer["name"], "postgres");
        assert_eq!(layer["status"], "ok");
        assert_eq!(layer["duration_ms"], 0);
        assert_eq!(layer["checks"][0]["name"], "pool");
        assert_eq!(layer["checks"][0]["status"], "ok");
    }

    // ── all-six-layers composite snapshots ────────────────────────────────────

    #[test]
    fn all_ok_fits_within_20_lines() {
        let report = all_ok_report();
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        let lines = out.lines().count();
        assert!(lines <= 20, "default output has {lines} lines, expected <= 20:\n{out}");
    }

    #[test]
    fn all_ok_human_snapshot() {
        let report = all_ok_report();
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  ok cluster      2/2, 0, 0\n",
            "  ok logs         0 errors, loki\n",
            "  ok workflows    0 stuck, 0 failed\n",
            "  ok health       2/2 healthy\n",
            "  ok grpc         reachable, 3 services, 21 methods\n",
            "  ok postgres     pool 5/20 in-use, 0 lock waits\n",
            "\n",
            "Summary: ok  0 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn all_ok_json_snapshot() {
        let report = all_ok_report();
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["namespace"], "nico");
        assert_eq!(json["summary"]["ok"], 6);
        assert_eq!(json["summary"]["warn"], 0);
        assert_eq!(json["summary"]["fail"], 0);
        assert_eq!(json["summary"]["skipped"], 0);
        assert_eq!(json["summary"]["unknown"], 0);
        assert_eq!(json["layers"].as_array().unwrap().len(), 6);
        // Verify diagnostic order
        let names: Vec<&str> = json["layers"].as_array().unwrap()
            .iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["cluster", "logs", "workflows", "health", "grpc", "postgres"]);
    }

    // ── skip snapshot ─────────────────────────────────────────────────────────

    #[test]
    fn skip_logs_and_grpc_human_snapshot() {
        let report = Report { layers: vec![
            layer("cluster", vec![
                ok_check("pods_ready", "2/2"),
                ok_check("recent_restarts", "0"),
                ok_check("warning_events", "0"),
            ]),
            skipped("logs"),
            layer("workflows", vec![
                ok_check("stuck", "0 stuck"),
                ok_check("failed", "0 failed"),
            ]),
            layer("health", vec![ok_check("endpoints", "2/2 healthy")]),
            skipped("grpc"),
            layer("postgres", vec![
                ok_check("pool", "pool 5/20 in-use"),
                ok_check("locks", "0 lock waits"),
            ]),
        ]};
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  ok cluster      2/2, 0, 0\n",
            "  . logs         (skipped)\n",
            "  ok workflows    0 stuck, 0 failed\n",
            "  ok health       2/2 healthy\n",
            "  . grpc         (skipped)\n",
            "  ok postgres     pool 5/20 in-use, 0 lock waits\n",
            "\n",
            "Summary: ok  0 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn skipped_layer_with_reason_renders_inline_in_human_summary() {
        let report = Report { layers: vec![
            skipped_with_reason("dpu", "n/a in rest-only-mock: no forgedb"),
        ]};
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  . dpu          (skipped — n/a in rest-only-mock: no forgedb)\n",
            "\n",
            "Summary: ok  0 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn skipped_layer_with_reason_renders_inline_in_verbose_mode() {
        let report = Report { layers: vec![
            skipped_with_reason("dpu", "n/a in rest-only-mock: no forgedb"),
            layer("postgres", vec![ok_check("pool", "5/20")]),
        ]};
        let out = format_report(&report, &plain(), true, &no_deltas(), false);
        assert!(
            out.contains("  . dpu          (skipped — n/a in rest-only-mock: no forgedb)"),
            "expected reason in verbose; got:\n{out}",
        );
    }

    #[test]
    fn skipped_layer_without_reason_still_renders_plain_skipped() {
        // No-reason call sites preserved: existing skipped output unchanged.
        let report = Report { layers: vec![skipped("logs")] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert!(out.contains("(skipped)\n"), "plain (skipped) preserved; got:\n{out}");
        assert!(!out.contains("(skipped — "), "no em-dash form when reason is None");
    }

    #[test]
    fn skip_layers_json_snapshot() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
            skipped("logs"),
            skipped("grpc"),
        ]};
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())).unwrap();
        assert_eq!(json["summary"]["ok"], 1);
        assert_eq!(json["summary"]["skipped"], 2);
        assert_eq!(json["layers"][1]["status"], "skipped");
        assert_eq!(json["layers"][1]["name"], "logs");
        assert_eq!(json["layers"][2]["status"], "skipped");
        assert_eq!(json["layers"][2]["name"], "grpc");
    }

    // ── verbose snapshot ──────────────────────────────────────────────────────

    #[test]
    fn verbose_all_ok_human_snapshot() {
        let report = Report { layers: vec![
            layer("cluster", vec![
                ok_check("pods_ready", "2/2"),
                ok_check("recent_restarts", "0"),
                ok_check("warning_events", "0"),
            ]),
            layer("postgres", vec![
                ok_check("pool", "pool 5/20 in-use"),
                ok_check("locks", "0 lock waits"),
            ]),
        ]};
        let out = format_report(&report, &plain(), true, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  ok cluster      2/2, 0, 0\n",
            "      ok pods_ready     2/2\n",
            "      ok recent_restarts 0\n",
            "      ok warning_events 0\n",
            "  ok postgres     pool 5/20 in-use, 0 lock waits\n",
            "      ok pool           pool 5/20 in-use\n",
            "      ok locks          0 lock waits\n",
            "\n",
            "Summary: ok  0 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn verbose_with_warnings_shows_next_commands() {
        let report = Report { layers: vec![
            layer("cluster", vec![
                warn_check("pods_ready", "1/2", Some("kubectl get pods -n nico | grep -v Running")),
                ok_check("recent_restarts", "0"),
                ok_check("warning_events", "0"),
            ]),
        ]};
        let out = format_report(&report, &plain(), true, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  warn cluster      1/2, 0, 0\n",
            "      warn pods_ready     1/2\n",
            "        → kubectl get pods -n nico | grep -v Running\n",
            "      ok recent_restarts 0\n",
            "      ok warning_events 0\n",
            "\n",
            "Summary: warn  1 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn verbose_skipped_layer_shows_no_detail_lines() {
        let report = Report { layers: vec![
            skipped("logs"),
            layer("postgres", vec![ok_check("pool", "pool 5/20 in-use")]),
        ]};
        let out = format_report(&report, &plain(), true, &no_deltas(), false);
        assert_eq!(out, concat!(
            "  . logs         (skipped)\n",
            "  ok postgres     pool 5/20 in-use\n",
            "      ok pool           pool 5/20 in-use\n",
            "\n",
            "Summary: ok  0 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    // ── footer hint always appears ────────────────────────────────────────────

    #[test]
    fn footer_hint_appears_with_no_layers() {
        let report = Report { layers: vec![] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert!(out.contains("Hint: --verbose for details on passing checks, --json for machine output"));
    }

    #[test]
    fn footer_hint_appears_with_failures() {
        let report = Report { layers: vec![layer("grpc", vec![
            fail_check("reachable", "unreachable", None),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert!(out.contains("Hint: --verbose for details on passing checks, --json for machine output"));
    }

    // ── delta badge display ───────────────────────────────────────────────────

    #[test]
    fn new_badge_shown_when_layer_regresses() {
        let report = Report { layers: vec![layer("logs", vec![
            warn_check("error_lines", "2 errors", None),
        ])] };
        let deltas = single_delta("logs", Delta::New);
        let out = format_report(&report, &plain(), false, &deltas, false);
        assert!(out.contains("[NEW]"), "expected [NEW] badge in:\n{out}");
    }

    #[test]
    fn fixed_badge_shown_when_layer_recovers() {
        let report = Report { layers: vec![layer("cluster", vec![
            ok_check("pods_ready", "2/2"),
        ])] };
        let deltas = single_delta("cluster", Delta::Fixed);
        let out = format_report(&report, &plain(), false, &deltas, false);
        assert!(out.contains("[FIXED]"), "expected [FIXED] badge in:\n{out}");
    }

    #[test]
    fn no_badge_for_unchanged_layer() {
        let report = Report { layers: vec![layer("cluster", vec![
            ok_check("pods_ready", "2/2"),
        ])] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        assert!(!out.contains("[NEW]") && !out.contains("[FIXED]"),
            "expected no badge in:\n{out}");
    }

    #[test]
    fn new_badge_snapshot() {
        let report = Report { layers: vec![
            layer("logs", vec![warn_check("error_lines", "2 errors", None)]),
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
        ]};
        let mut deltas = HashMap::new();
        deltas.insert("logs".to_string(), Delta::New);
        let out = format_report(&report, &plain(), false, &deltas, false);
        assert_eq!(out, concat!(
            "  warn logs         2 errors [NEW]\n",
            "  ok cluster      2/2\n",
            "\n",
            "logs:\n",
            "  • 2 errors (error_lines)\n",
            "\n",
            "Summary: warn  1 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    #[test]
    fn fixed_badge_snapshot() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
        ]};
        let deltas = single_delta("cluster", Delta::Fixed);
        let out = format_report(&report, &plain(), false, &deltas, false);
        assert_eq!(out, concat!(
            "  ok cluster      2/2 [FIXED]\n",
            "\n",
            "Summary: ok  0 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    // ── spotlight flag ────────────────────────────────────────────────────────

    #[test]
    fn spotlight_hides_ok_unchanged_layers() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
            layer("logs", vec![warn_check("error_lines", "3 errors", None)]),
        ]};
        let out = format_report(&report, &plain(), false, &no_deltas(), true);
        assert!(!out.contains("cluster"), "ok+unchanged cluster should be hidden by spotlight");
        assert!(out.contains("logs"), "warn layer should still appear");
    }

    #[test]
    fn spotlight_hides_skipped_unchanged_layers() {
        let report = Report { layers: vec![
            skipped("grpc"),
            layer("logs", vec![warn_check("error_lines", "1 error", None)]),
        ]};
        let out = format_report(&report, &plain(), false, &no_deltas(), true);
        assert!(!out.contains("grpc"), "skipped+unchanged grpc should be hidden");
        assert!(out.contains("logs"));
    }

    #[test]
    fn spotlight_always_shows_new_delta_even_if_ok() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
        ]};
        let deltas = single_delta("cluster", Delta::Fixed);
        let out = format_report(&report, &plain(), false, &deltas, true);
        assert!(out.contains("cluster"), "ok+fixed cluster must be shown by spotlight");
        assert!(out.contains("[FIXED]"));
    }

    #[test]
    fn spotlight_always_shows_new_badge_layers() {
        let report = Report { layers: vec![
            layer("logs", vec![warn_check("error_lines", "2 errors", None)]),
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
        ]};
        let deltas = single_delta("logs", Delta::New);
        let out = format_report(&report, &plain(), false, &deltas, true);
        assert!(out.contains("logs"), "warn+new logs must be shown");
        assert!(out.contains("[NEW]"));
        assert!(!out.contains("cluster"), "ok+unchanged cluster hidden by spotlight");
    }

    #[test]
    fn spotlight_snapshot_shows_only_changed_layers() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
            layer("logs", vec![warn_check("error_lines", "2 errors", None)]),
            layer("grpc", vec![ok_check("reachable", "reachable")]),
        ]};
        let deltas = single_delta("logs", Delta::New);
        let out = format_report(&report, &plain(), false, &deltas, true);
        assert_eq!(out, concat!(
            "  warn logs         2 errors [NEW]\n",
            "\n",
            "logs:\n",
            "  • 2 errors (error_lines)\n",
            "\n",
            "Summary: warn  1 warnings, 0 failures\n",
            "Hint: --verbose for details on passing checks, --json for machine output\n",
        ));
    }

    // ── findings-block cap (issue #179) ───────────────────────────────────────

    fn make_pod_errors(n: usize) -> Vec<Check> {
        (0..n).map(|i| Check {
            name: "pod_error",
            status: Status::Warn,
            value: format!("pod-{i}: ERROR: boom"),
            next_command: Some(format!("kubectl logs pod-{i} -n nico")),
            kind: CheckKind::Detail,
        }).collect()
    }

    /// Returns the findings-block bullet lines for `layer_name` in `out`.
    /// A bullet line is `  • ...`; the elision line `  … +M more ...` is also
    /// included so tests can assert on it.
    fn findings_bullets(out: &str, layer_name: &str) -> Vec<String> {
        let header = format!("{layer_name}:");
        let mut lines = out.lines();
        // Skip until we hit the layer header.
        for line in lines.by_ref() {
            if line == header { break; }
        }
        let mut bullets = Vec::new();
        for line in lines {
            if line.starts_with("  • ") || line.starts_with("  … ") {
                bullets.push(line.to_string());
            } else if line.starts_with("    → ") {
                continue;
            } else {
                break;
            }
        }
        bullets
    }

    #[test]
    fn under_cap_default_mode_omits_elision_line() {
        // 3 detail bullets, cap is 5 → no elision line.
        let report = Report { layers: vec![layer("logs", make_pod_errors(3))] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        let bullets = findings_bullets(&out, "logs");

        assert_eq!(bullets.len(), 3, "expected 3 bullets, no elision:\n{out}");
        assert!(!out.contains("more · --verbose"), "no elision line expected:\n{out}");
    }

    #[test]
    fn at_cap_default_mode_omits_elision_line() {
        // Boundary: exactly N bullets → still no elision.
        let report = Report { layers: vec![layer("logs", make_pod_errors(FINDINGS_CAP))] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        let bullets = findings_bullets(&out, "logs");

        assert_eq!(bullets.len(), FINDINGS_CAP, "expected exactly {FINDINGS_CAP} bullets, no elision:\n{out}");
        assert!(!out.contains("more · --verbose"), "no elision line at the boundary:\n{out}");
    }

    #[test]
    fn over_cap_default_mode_emits_elision_line() {
        let report = Report { layers: vec![layer("logs", make_pod_errors(8))] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        let bullets = findings_bullets(&out, "logs");

        assert_eq!(bullets.len(), FINDINGS_CAP + 1,
            "expected {} bullets + 1 elision line, got {}:\n{out}", FINDINGS_CAP, bullets.len());
        for (i, bullet) in bullets.iter().take(FINDINGS_CAP).enumerate() {
            assert!(bullet.contains(&format!("pod-{i}: ERROR: boom")),
                "bullet {i} mismatch:\n{out}");
        }
        assert_eq!(bullets[FINDINGS_CAP], "  … +3 more · --verbose for full list");
    }

    #[test]
    fn over_cap_verbose_mode_renders_every_bullet() {
        // --verbose bypasses the cap: all 8 detail rows must render and no
        // elision line appears.
        let report = Report { layers: vec![layer("logs", make_pod_errors(8))] };
        let out = format_report(&report, &plain(), true, &no_deltas(), false);

        for i in 0..8 {
            assert!(out.contains(&format!("pod-{i}: ERROR: boom")),
                "expected pod-{i} in verbose output:\n{out}");
        }
        assert!(!out.contains("more · --verbose"), "no elision in verbose mode:\n{out}");
    }

    #[test]
    fn over_cap_json_includes_every_check() {
        // JSON contract: cap does not affect machine output.
        let report = Report { layers: vec![layer("logs", make_pod_errors(8))] };
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        let checks = json["layers"][0]["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 8, "JSON must include all 8 checks regardless of cap");
        for (i, check) in checks.iter().enumerate() {
            assert_eq!(check["value"], format!("pod-{i}: ERROR: boom"));
        }
    }

    #[test]
    fn json_byte_for_byte_unchanged_under_cap_vs_over_cap() {
        // Over-cap and under-cap inputs of equal length produce structurally
        // identical JSON shape (every field present); cap never short-circuits.
        let over = Report { layers: vec![layer("logs", make_pod_errors(20))] };
        let json_str = format_json(&over, "nico", serde_json::json!({"ok": true}), &no_deltas());
        // Re-parse and confirm every check is present.
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["layers"][0]["checks"].as_array().unwrap().len(), 20);
        // No elision marker may leak into JSON.
        assert!(!json_str.contains("--verbose"), "JSON must never contain elision text");
        assert!(!json_str.contains("…"), "JSON must never contain ellipsis");
    }

    // ── JSON delta field ──────────────────────────────────────────────────────

    #[test]
    fn json_skipped_layer_with_reason_includes_skipped_reason_field() {
        let report = Report { layers: vec![
            skipped_with_reason("dpu", "n/a in rest-only-mock: no forgedb"),
        ]};
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        let layer = &json["layers"][0];
        assert_eq!(layer["status"], "skipped");
        assert_eq!(layer["skipped_reason"], "n/a in rest-only-mock: no forgedb");
    }

    #[test]
    fn json_skipped_layer_without_reason_emits_null_skipped_reason() {
        let report = Report { layers: vec![skipped("logs")] };
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        assert!(
            json["layers"][0].get("skipped_reason").is_some(),
            "skipped_reason field present even when None: {}",
            json["layers"][0],
        );
        assert!(json["layers"][0]["skipped_reason"].is_null());
    }

    #[test]
    fn json_non_skipped_layer_emits_null_skipped_reason() {
        let report = Report { layers: vec![layer("cluster", vec![ok_check("pods_ready", "2/2")])] };
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        assert!(json["layers"][0]["skipped_reason"].is_null());
    }

    #[test]
    fn json_includes_delta_unchanged_by_default() {
        let report = Report { layers: vec![layer("cluster", vec![ok_check("pods_ready", "2/2")])] };
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        assert_eq!(json["layers"][0]["delta"], "unchanged");
    }

    #[test]
    fn json_includes_delta_new() {
        let report = Report { layers: vec![layer("logs", vec![warn_check("error_lines", "2 errors", None)])] };
        let deltas = single_delta("logs", Delta::New);
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &deltas)
        ).unwrap();
        assert_eq!(json["layers"][0]["delta"], "new");
    }

    #[test]
    fn json_includes_delta_fixed() {
        let report = Report { layers: vec![layer("cluster", vec![ok_check("pods_ready", "2/2")])] };
        let deltas = single_delta("cluster", Delta::Fixed);
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &deltas)
        ).unwrap();
        assert_eq!(json["layers"][0]["delta"], "fixed");
    }

    #[test]
    fn json_all_layers_have_delta_field() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
            layer("logs", vec![warn_check("error_lines", "1 error", None)]),
        ]};
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        for l in json["layers"].as_array().unwrap() {
            assert!(l.get("delta").is_some(), "layer {} missing delta field", l["name"]);
        }
    }

    // ── headline vs detail (issue #181) ───────────────────────────────────────

    /// Returns the layer summary line (the "  ICON name  values" line) for
    /// `layer_name` from formatted output. Strips status icon + name padding
    /// and returns the `value, value, ...` portion.
    fn layer_summary_values(out: &str, layer_name: &str) -> String {
        out.lines()
            .find(|l| l.contains(&format!(" {layer_name:<12} ")))
            .map(|l| l.split_once(&format!(" {layer_name:<12} ")).unwrap().1.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn summary_line_excludes_detail_check_values() {
        // Layer with 2 headlines + 100 details. Detail values must never
        // appear in the summary line, regardless of count.
        let mut checks = vec![
            warn_check("error_lines", "100 errors", None),
            ok_check("source", "loki"),
        ];
        for i in 0..100 {
            checks.push(detail_warn(
                "pod_error",
                &format!("pod-{i}: ERROR boom"),
                Some(&format!("kubectl logs pod-{i} -n nico")),
            ));
        }
        let report = Report { layers: vec![layer("logs", checks)] };
        let out = format_report(&report, &plain(), false, &no_deltas(), false);

        let summary = layer_summary_values(&out, "logs");
        assert_eq!(summary, "100 errors, loki",
            "summary line must contain headline values only, got: {summary:?}\nfull:\n{out}");
        assert!(!summary.contains("pod_error"));
        assert!(!summary.contains("pod-0:"));
    }

    #[test]
    fn summary_line_joins_only_headlines() {
        // Bounded summary: 3 headline checks → 3 comma-separated values.
        let report = Report { layers: vec![layer("cluster", vec![
            ok_check("pods_ready", "2/2"),
            ok_check("recent_restarts", "0"),
            ok_check("warning_events", "0"),
        ])]};
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        let summary = layer_summary_values(&out, "cluster");
        assert_eq!(summary, "2/2, 0, 0");
    }

    #[test]
    fn json_every_check_includes_kind_field() {
        // Acceptance: JSON exposes `kind` additively for every check.
        let report = Report { layers: vec![
            layer("logs", vec![
                warn_check("error_lines", "2 errors", None),
                ok_check("source", "loki"),
                detail_warn("pod_error", "core: boom", Some("kubectl logs core")),
            ]),
        ]};
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();

        let checks = json["layers"][0]["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0]["kind"], "headline");
        assert_eq!(checks[1]["kind"], "headline");
        assert_eq!(checks[2]["kind"], "detail");
    }

    #[test]
    fn json_version_unchanged_after_kind_field_added() {
        // ADR-0003: additive changes don't bump version. version stays at 1.
        let report = Report { layers: vec![layer("logs", vec![
            detail_warn("pod_error", "core: boom", Some("kubectl logs core")),
        ])]};
        let json: serde_json::Value = serde_json::from_str(
            &format_json(&report, "nico", serde_json::json!({"ok": true}), &no_deltas())
        ).unwrap();
        assert_eq!(json["version"], 1);
    }

    #[test]
    fn findings_block_iterates_all_non_ok_checks_including_details() {
        // Acceptance: Findings block (default and verbose) is unchanged —
        // still iterates all non-OK checks (subject to the per-layer cap).
        let report = Report { layers: vec![layer("logs", vec![
            warn_check("error_lines", "2 errors", None),
            detail_warn("pod_error", "core: boom", None),
            detail_warn("pod_error", "rest: oom", None),
        ])]};
        let out = format_report(&report, &plain(), false, &no_deltas(), false);
        // All three non-ok checks appear as bullets.
        assert!(out.contains("• 2 errors (error_lines)"));
        assert!(out.contains("• core: boom (pod_error)"));
        assert!(out.contains("• rest: oom (pod_error)"));
    }

    #[test]
    fn verbose_mode_unchanged_renders_every_check() {
        // Verbose mode iterates all checks, headline and detail alike.
        let report = Report { layers: vec![layer("logs", vec![
            warn_check("error_lines", "2 errors", None),
            detail_warn("pod_error", "core: boom", None),
        ])]};
        let out = format_report(&report, &plain(), true, &no_deltas(), false);
        assert!(out.contains("error_lines"));
        assert!(out.contains("pod_error"));
        assert!(out.contains("core: boom"));
    }
}
