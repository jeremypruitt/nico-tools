use std::sync::Arc;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::bootstrap::{build_log_source, LayerInputs};
use crate::log_source::LogSource;
use crate::layer::{self, Check, CheckKind, Layer, LayerOutcome, RunOpts};

const LOG_LINE_LIMIT: usize = 500;

pub const NAME: &str = "logs";

/// Factory consumed by `bootstrap::prepare_layers`.
pub fn register(inputs: &LayerInputs) -> Box<dyn Layer> {
    match build_log_source(inputs) {
        Some(chain) => Box::new(LogsLayer::new(chain)),
        None => layer::UnconfiguredLayer::new(
            NAME,
            "set LOKI_URL or ensure kubeconfig is accessible",
        ),
    }
}

pub struct LogsLayer {
    source: Arc<dyn LogSource>,
}

impl LogsLayer {
    pub fn new(source: Arc<dyn LogSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl Layer for LogsLayer {
    fn name(&self) -> &'static str { "logs" }

    async fn collect(&self, opts: &RunOpts) -> LayerOutcome {
        let (pod_errors, source_label, source_ok) =
            match self
                .source
                .collect(&opts.namespace, opts.since, LOG_LINE_LIMIT, &opts.pod_logs)
                .await
            {
                Ok(c) => (c.entries, c.label, c.primary_ok),
                Err(_) => (Vec::new(), "unavailable".to_string(), false),
            };

        LayerOutcome::Checks(checks_from(&pod_errors, &source_label, source_ok, &opts.namespace))
    }
}

fn checks_from(
    pod_errors: &[(String, String)],
    source_label: &str,
    source_ok: bool,
    namespace: &str,
) -> Vec<Check> {
    let error_count = pod_errors.len();
    let mut checks = vec![
        Check {
            name: "error_lines",
            status: if error_count == 0 { Status::Ok } else { Status::Warn },
            value: format!("{error_count} errors"),
            next_command: None,
            kind: CheckKind::Headline,
        },
        Check {
            name: "source",
            status: if source_ok { Status::Ok } else { Status::Warn },
            value: source_label.to_string(),
            next_command: None,
            kind: CheckKind::Headline,
        },
    ];

    for (pod, count, recent) in group_by_pod(pod_errors) {
        let excerpt = if recent.len() > 80 {
            format!("{}…", &recent[..79])
        } else {
            recent.to_string()
        };
        checks.push(Check {
            name: "pod_error",
            status: Status::Warn,
            value: format!("{pod}: {count} errors — {excerpt}"),
            next_command: Some(format!("kubectl logs {pod} -n {namespace}")),
            kind: CheckKind::Detail,
        });
    }

    checks
}

fn group_by_pod(pod_errors: &[(String, String)]) -> Vec<(&str, usize, &str)> {
    let mut grouped: Vec<(&str, usize, &str)> = Vec::new();
    for (pod, line) in pod_errors {
        if let Some(slot) = grouped.iter_mut().find(|(p, _, _)| *p == pod.as_str()) {
            slot.1 += 1;
            slot.2 = line.as_str();
        } else {
            grouped.push((pod.as_str(), 1, line.as_str()));
        }
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use async_trait::async_trait;
    use crate::layer::aggregate_status;
    use crate::log_source::{LogCollection, LogSource, PodLogsCache};

    fn opts() -> RunOpts {
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
            ..Default::default()
        }
    }

    struct FakeLogSource {
        label: String,
        primary_ok: bool,
        entries: Vec<(String, String)>,
    }

    impl FakeLogSource {
        fn new(label: &str, primary_ok: bool, entries: Vec<(&str, &str)>) -> Self {
            Self {
                label: label.to_string(),
                primary_ok,
                entries: entries.into_iter().map(|(p, t)| (p.to_string(), t.to_string())).collect(),
            }
        }
    }

    #[async_trait]
    impl LogSource for FakeLogSource {
        fn name(&self) -> &str { &self.label }

        async fn collect(
            &self,
            _: &str,
            _: Duration,
            _: usize,
            _: &PodLogsCache,
        ) -> Result<LogCollection> {
            Ok(LogCollection {
                label: self.label.clone(),
                primary_ok: self.primary_ok,
                entries: self.entries.clone(),
            })
        }
    }

    #[test]
    fn pod_error_checks_are_marked_detail() {
        let pod_errors = vec![
            ("core-abc".to_string(), "ERROR: disk full".to_string()),
            ("rest-xyz".to_string(), "FATAL: oom".to_string()),
        ];
        let checks = checks_from(&pod_errors, "loki", true, "nico");
        let pod_error_kinds: Vec<_> = checks.iter()
            .filter(|c| c.name == "pod_error")
            .map(|c| c.kind)
            .collect();
        assert_eq!(pod_error_kinds.len(), 2);
        assert!(pod_error_kinds.iter().all(|k| *k == CheckKind::Detail));
    }

    #[test]
    fn headline_checks_remain_headline_kind() {
        let pod_errors = vec![("core-abc".to_string(), "ERROR".to_string())];
        let checks = checks_from(&pod_errors, "loki", true, "nico");
        let err = checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err.kind, CheckKind::Headline);
        let src = checks.iter().find(|c| c.name == "source").unwrap();
        assert_eq!(src.kind, CheckKind::Headline);
    }

    #[test]
    fn checks_from_no_errors_reports_ok() {
        let checks = checks_from(&[], "loki", true, "nico");
        assert_eq!(aggregate_status(&checks), Status::Ok);
        assert_eq!(checks.iter().filter(|c| c.name == "pod_error").count(), 0);
    }

    #[test]
    fn checks_from_errors_present_reports_warn_with_one_pod_error_per_entry() {
        let pod_errors = vec![
            ("core-abc".to_string(), "ERROR: disk full".to_string()),
            ("rest-xyz".to_string(), "FATAL: oom".to_string()),
        ];
        let checks = checks_from(&pod_errors, "loki", true, "nico");

        assert_eq!(aggregate_status(&checks), Status::Warn);
        assert_eq!(checks.iter().filter(|c| c.name == "pod_error").count(), 2);
        let err_check = checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.status, Status::Warn);
    }

    #[test]
    fn error_lines_headline_counts_raw_entries_not_collapsed_pods() {
        let pod_errors: Vec<(String, String)> = (0..7)
            .map(|i| ("noisy-pod".to_string(), format!("ERROR {i}")))
            .collect();
        let checks = checks_from(&pod_errors, "loki", true, "nico");

        let err_check = checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.value, "7 errors");
        assert_eq!(checks.iter().filter(|c| c.name == "pod_error").count(), 1);
    }

    #[test]
    fn checks_from_truncates_sample_line_to_80_chars_with_ellipsis() {
        let long_line = format!("ERROR: {}", "x".repeat(200));
        let pod_errors = vec![("noisy-pod".to_string(), long_line)];
        let checks = checks_from(&pod_errors, "loki", true, "nico");

        let pe = checks.iter().find(|c| c.name == "pod_error").unwrap();
        let sample = pe.value.split(" — ").nth(1).expect("value has sample after em-dash");
        assert!(sample.ends_with('…'), "sample should be ellipsised: {sample}");
        assert_eq!(sample.chars().count(), 80, "sample should cap at 80 chars: {sample}");
    }

    #[test]
    fn checks_from_multiple_pods_each_get_their_own_pod_error() {
        let pod_errors = vec![
            ("carbide-api-abc".to_string(), "ERROR: vault expired".to_string()),
            ("carbide-api-abc".to_string(), "ERROR: vault expired".to_string()),
            ("rest-xyz".to_string(), "FATAL: oom".to_string()),
            ("workflow-svc".to_string(), "panic: nil pointer".to_string()),
            ("rest-xyz".to_string(), "FATAL: oom retry".to_string()),
        ];
        let checks = checks_from(&pod_errors, "loki", true, "nico");

        let pod_error_checks: Vec<_> = checks.iter().filter(|c| c.name == "pod_error").collect();
        assert_eq!(pod_error_checks.len(), 3);

        let next_cmds: Vec<_> = pod_error_checks
            .iter()
            .map(|c| c.next_command.clone().unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = next_cmds.iter().collect();
        assert_eq!(unique.len(), 3, "next_commands should be unique per pod: {next_cmds:?}");
    }

    #[test]
    fn checks_from_single_pod_multiple_errors_collapses_to_one_pod_error() {
        let pod_errors = vec![
            ("carbide-api-abc".to_string(), "ERROR: vault credential expired".to_string()),
            ("carbide-api-abc".to_string(), "ERROR: vault credential expired".to_string()),
            ("carbide-api-abc".to_string(), "ERROR: vault sealed".to_string()),
        ];
        let checks = checks_from(&pod_errors, "loki", true, "nico");

        let pod_error_checks: Vec<_> = checks.iter().filter(|c| c.name == "pod_error").collect();
        assert_eq!(pod_error_checks.len(), 1);
        let pe = pod_error_checks[0];
        assert!(pe.value.starts_with("carbide-api-abc: 3 errors"), "got value: {}", pe.value);
        assert!(pe.value.contains("vault sealed"), "expected most-recent line in value, got: {}", pe.value);
        assert_eq!(
            pe.next_command.as_deref(),
            Some("kubectl logs carbide-api-abc -n nico"),
        );
    }

    #[test]
    fn checks_from_source_unavailable_marks_source_warn() {
        let checks = checks_from(&[], "k8s (loki unavailable)", false, "nico");
        let src = checks.iter().find(|c| c.name == "source").unwrap();
        assert_eq!(src.status, Status::Warn);
        assert_eq!(checks.iter().filter(|c| c.name == "source").count(), 1);
    }

    #[tokio::test]
    async fn primary_source_with_errors_reports_warn_with_kubectl_hints() {
        let source: Arc<dyn LogSource> = Arc::new(FakeLogSource::new(
            "loki", true,
            vec![("core-abc", "ERROR: disk full"), ("rest-xyz", "FATAL: oom")],
        ));
        let result = LogsLayer::new(source).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let err_check = result.checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.value, "2 errors");
        let src = result.checks.iter().find(|c| c.name == "source").unwrap();
        assert_eq!(src.value, "loki");
        assert_eq!(src.status, Status::Ok);
        let pod_errors: Vec<_> = result.checks.iter().filter(|c| c.name == "pod_error").collect();
        assert_eq!(pod_errors.len(), 2);
        assert!(pod_errors[0].next_command.as_deref().unwrap().starts_with("kubectl logs"));
    }

    #[tokio::test]
    async fn fallback_source_marks_source_warn_and_keeps_label() {
        let source: Arc<dyn LogSource> = Arc::new(FakeLogSource::new(
            "k8s (loki unavailable)", false,
            vec![("core-abc", "ERROR: connection refused")],
        ));
        let result = LogsLayer::new(source).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let src = result.checks.iter().find(|c| c.name == "source").unwrap();
        assert!(src.value.contains("loki unavailable"));
        assert_eq!(src.status, Status::Warn);
    }

    #[tokio::test]
    async fn empty_source_reports_ok() {
        let source: Arc<dyn LogSource> = Arc::new(FakeLogSource::new("loki", true, vec![]));
        let result = LogsLayer::new(source).run(&opts()).await;
        assert_eq!(result.status, Status::Ok);
        let err_check = result.checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.value, "0 errors");
        assert!(result.checks.iter().filter(|c| c.name == "pod_error").count() == 0);
    }
}
