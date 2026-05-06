use nico_common::output::{OutputMode, Status};
use crate::runner::Report;

pub fn format_report(report: &Report, mode: &OutputMode, verbose: bool) -> String {
    let mut out = String::new();

    for layer in &report.layers {
        let icon = layer.status.icon(mode);
        let styled_icon = layer.status.style(icon, mode);
        let summary = if layer.status == Status::Skipped {
            "(skipped)".to_string()
        } else {
            layer.checks.iter()
                .map(|c| c.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!("  {} {:<12} {}\n", styled_icon, layer.name, summary));

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
                for check in bad {
                    out.push_str(&format!("  • {} ({})\n", check.value, check.name));
                    if let Some(cmd) = &check.next_command {
                        out.push_str(&format!("    → {}\n", cmd));
                    }
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

pub fn format_json(report: &Report, namespace: &str, preflight: serde_json::Value) -> String {
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
        "layers": report.layers.iter().map(|l| serde_json::json!({
            "name": l.name,
            "status": format!("{:?}", l.status).to_lowercase(),
            "duration_ms": l.duration_ms,
            "checks": l.checks.iter().map(|c| serde_json::json!({
                "name": c.name,
                "status": format!("{:?}", c.status).to_lowercase(),
                "value": c.value,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{Check, LayerResult};
    use crate::runner::Report;

    fn plain() -> OutputMode {
        OutputMode { color: false, ascii: true }
    }

    fn ok_check(name: &'static str, value: &str) -> Check {
        Check { name, status: Status::Ok, value: value.to_string(), next_command: None }
    }

    fn warn_check(name: &'static str, value: &str, cmd: Option<&str>) -> Check {
        Check { name, status: Status::Warn, value: value.to_string(), next_command: cmd.map(str::to_string) }
    }

    fn fail_check(name: &'static str, value: &str, cmd: Option<&str>) -> Check {
        Check { name, status: Status::Fail, value: value.to_string(), next_command: cmd.map(str::to_string) }
    }

    fn layer(name: &'static str, checks: Vec<Check>) -> LayerResult {
        let status = if checks.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else if checks.iter().any(|c| c.status == Status::Warn) {
            Status::Warn
        } else {
            Status::Ok
        };
        LayerResult { name, status, checks, duration_ms: 0 }
    }

    fn skipped(name: &'static str) -> LayerResult {
        LayerResult { name, status: Status::Skipped, checks: vec![], duration_ms: 0 }
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
        let out = format_report(&report, &plain(), false);
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
        let out = format_report(&report, &plain(), false);
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
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
            warn_check("pod_error", "core-abc: ERROR: disk full", Some("kubectl logs core-abc -n nico")),
            warn_check("pod_error", "rest-xyz: FATAL: oom", Some("kubectl logs rest-xyz -n nico")),
        ])] };
        let out = format_report(&report, &plain(), false);
        assert_eq!(out, concat!(
            "  warn logs         2 errors, loki, core-abc: ERROR: disk full, rest-xyz: FATAL: oom\n",
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
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
            warn_check("stuck_workflow", "wf-001 (HostProvisioning): 47m running, last: ActivityScheduled",
                Some("temporal workflow show -w wf-001")),
        ])] };
        let out = format_report(&report, &plain(), false);
        assert_eq!(out, concat!(
            "  warn workflows    1 stuck, 0 failed, wf-001 (HostProvisioning): 47m running, last: ActivityScheduled\n",
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
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
        let out = format_report(&report, &plain(), false);
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
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
        let out = format_report(&report, &plain(), false);
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
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
        let out = format_report(&report, &plain(), false);
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
        let lines = out.lines().count();
        assert!(lines <= 20, "default output has {lines} lines, expected <= 20:\n{out}");
    }

    #[test]
    fn all_ok_human_snapshot() {
        let report = all_ok_report();
        let out = format_report(&report, &plain(), false);
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
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), false);
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
    fn skip_layers_json_snapshot() {
        let report = Report { layers: vec![
            layer("cluster", vec![ok_check("pods_ready", "2/2")]),
            skipped("logs"),
            skipped("grpc"),
        ]};
        let json: serde_json::Value = serde_json::from_str(&format_json(&report, "nico", serde_json::json!({"ok": true}))).unwrap();
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
        let out = format_report(&report, &plain(), true);
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
        let out = format_report(&report, &plain(), true);
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
        let out = format_report(&report, &plain(), true);
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
        let out = format_report(&report, &plain(), false);
        assert!(out.contains("Hint: --verbose for details on passing checks, --json for machine output"));
    }

    #[test]
    fn footer_hint_appears_with_failures() {
        let report = Report { layers: vec![layer("grpc", vec![
            fail_check("reachable", "unreachable", None),
        ])] };
        let out = format_report(&report, &plain(), false);
        assert!(out.contains("Hint: --verbose for details on passing checks, --json for machine output"));
    }
}
