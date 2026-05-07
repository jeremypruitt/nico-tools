use std::sync::Arc;

use nico_doctor::layer::{Layer, LayerResult, RunOpts};
use tokio::sync::mpsc;

use crate::model::{Finding, LayerSnapshot};

/// Run all layers concurrently and return the resulting snapshots in
/// `LAYER_ORDER` (whatever order `prepare_layers` produced).
pub async fn collect(layers: Arc<Vec<Box<dyn Layer>>>, opts: RunOpts) -> Vec<LayerSnapshot> {
    let names: Vec<&'static str> = layers.iter().map(|l| l.name()).collect();
    let (tx, mut rx) = mpsc::channel::<LayerResult>(layers.len().max(1));

    let layers_for_stream = layers.clone();
    let task_opts = opts.clone();
    let task = tokio::spawn(async move {
        nico_doctor::run_streaming(layers_for_stream, task_opts, tx).await;
    });

    let mut by_name = std::collections::HashMap::<&'static str, LayerResult>::new();
    while let Some(res) = rx.recv().await {
        by_name.insert(res.name, res);
    }
    let _ = task.await;

    names
        .into_iter()
        .filter_map(|n| by_name.remove(n).map(layer_result_to_snapshot))
        .collect()
}

fn layer_result_to_snapshot(r: LayerResult) -> LayerSnapshot {
    let evidence = summarize_evidence(&r);
    let duration_ms = r.duration_ms;
    let findings = r
        .checks
        .into_iter()
        .filter(|c| c.status != nico_common::output::Status::Ok)
        .map(|c| Finding {
            status: c.status,
            message: format!("{}: {}", c.name, c.value),
            next_command: c.next_command,
            link: None,
        })
        .collect();
    LayerSnapshot {
        name: r.name.to_string(),
        status: r.status,
        evidence,
        findings,
        duration_ms,
    }
}

fn summarize_evidence(r: &LayerResult) -> String {
    use nico_common::output::Status;
    let bad: Vec<_> = r
        .checks
        .iter()
        .filter(|c| matches!(c.status, Status::Warn | Status::Fail | Status::Unknown))
        .collect();

    if bad.is_empty() {
        match r.status {
            Status::Ok if !r.checks.is_empty() => format!("{} checks ok", r.checks.len()),
            Status::Ok => "ok".to_string(),
            Status::Skipped => "skipped".to_string(),
            Status::Unknown => "unknown".to_string(),
            _ => String::new(),
        }
    } else if bad.len() == 1 {
        let c = bad[0];
        if c.value.is_empty() {
            c.name.to_string()
        } else {
            c.value.clone()
        }
    } else {
        format!("{} findings", bad.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nico_common::output::Status;
    use nico_doctor::layer::{Check, CheckKind, LayerResult};

    fn check(name: &'static str, status: Status, value: &str) -> Check {
        Check {
            name,
            status,
            value: value.to_string(),
            next_command: None,
            kind: CheckKind::Headline,
        }
    }

    fn result(name: &'static str, status: Status, checks: Vec<Check>) -> LayerResult {
        LayerResult {
            name,
            status,
            checks,
            duration_ms: 0,
        }
    }

    #[test]
    fn ok_layer_with_checks_summarizes_count() {
        let r = result(
            "cluster",
            Status::Ok,
            vec![
                check("nodes", Status::Ok, "3 ready"),
                check("pods", Status::Ok, "all running"),
            ],
        );
        let s = layer_result_to_snapshot(r);
        assert_eq!(s.evidence, "2 checks ok");
        assert!(s.findings.is_empty());
    }

    #[test]
    fn single_warn_check_uses_its_value_as_evidence() {
        let r = result(
            "logs",
            Status::Warn,
            vec![check("errors", Status::Warn, "12 ERRORs")],
        );
        let s = layer_result_to_snapshot(r);
        assert_eq!(s.evidence, "12 ERRORs");
        assert_eq!(s.findings.len(), 1);
    }

    #[test]
    fn multiple_bad_checks_summarize_count() {
        let r = result(
            "logs",
            Status::Fail,
            vec![
                check("errors", Status::Warn, "12 ERRORs"),
                check("crashloops", Status::Fail, "2 pods"),
            ],
        );
        let s = layer_result_to_snapshot(r);
        assert_eq!(s.evidence, "2 findings");
        assert_eq!(s.findings.len(), 2);
    }

    #[test]
    fn skipped_layer_has_skipped_evidence_and_no_findings() {
        let r = result("postgres", Status::Skipped, vec![]);
        let s = layer_result_to_snapshot(r);
        assert_eq!(s.evidence, "skipped");
        assert!(s.findings.is_empty());
    }

    #[test]
    fn ok_check_is_not_a_finding() {
        let r = result(
            "cluster",
            Status::Warn,
            vec![
                check("nodes", Status::Ok, "3 ready"),
                check("pods", Status::Warn, "1 pending"),
            ],
        );
        let s = layer_result_to_snapshot(r);
        assert_eq!(s.findings.len(), 1);
        assert_eq!(s.findings[0].status, Status::Warn);
    }
}
