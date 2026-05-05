use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use async_trait::async_trait;
use nico_common::output::Status;
use crate::temporal::TemporalClient;
use crate::layer::{Check, Layer, LayerResult, RunOpts};

pub struct WorkflowsLayer {
    temporal: Arc<dyn TemporalClient>,
    stuck_threshold: Duration,
}

impl WorkflowsLayer {
    pub fn new(temporal: Arc<dyn TemporalClient>, stuck_threshold: Duration) -> Self {
        Self { temporal, stuck_threshold }
    }
}

#[async_trait]
impl Layer for WorkflowsLayer {
    fn name(&self) -> &'static str { "workflows" }

    async fn run(&self, opts: &RunOpts) -> LayerResult {
        let start = Instant::now();
        let now = SystemTime::now();
        let stuck_before = now.checked_sub(self.stuck_threshold).unwrap_or(SystemTime::UNIX_EPOCH);
        let failed_since = now.checked_sub(opts.since).unwrap_or(SystemTime::UNIX_EPOCH);

        let stuck = self.temporal.list_stuck(&opts.namespace, stuck_before).await.unwrap_or_default();
        let failed = self.temporal.list_failed(&opts.namespace, failed_since).await.unwrap_or_default();

        let mut checks = vec![
            Check {
                name: "stuck",
                status: if stuck.is_empty() { Status::Ok } else { Status::Warn },
                value: format!("{} stuck", stuck.len()),
                next_command: if stuck.is_empty() {
                    None
                } else {
                    Some(r#"temporal workflow list --query "ExecutionStatus=Running""#.to_string())
                },
            },
            Check {
                name: "failed",
                status: if failed.is_empty() { Status::Ok } else { Status::Warn },
                value: format!("{} failed", failed.len()),
                next_command: None,
            },
        ];

        for wf in &stuck {
            let running_mins = now.duration_since(wf.start_time)
                .unwrap_or_default()
                .as_secs() / 60;
            checks.push(Check {
                name: "stuck_workflow",
                status: Status::Warn,
                value: format!(
                    "{} ({}): {}m running, last: {}",
                    wf.workflow_id, wf.workflow_type, running_mins, wf.last_event
                ),
                next_command: Some(format!("temporal workflow show -w {}", wf.workflow_id)),
            });
        }

        for wf in &failed {
            checks.push(Check {
                name: "failed_workflow",
                status: Status::Warn,
                value: format!("{} ({}): failed", wf.workflow_id, wf.workflow_type),
                next_command: Some(format!("temporal workflow show -w {}", wf.workflow_id)),
            });
        }

        let overall = if checks.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else if checks.iter().any(|c| c.status == Status::Warn) {
            Status::Warn
        } else {
            Status::Ok
        };

        LayerResult {
            name: "workflows",
            status: overall,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use async_trait::async_trait;
    use crate::temporal::{FailedWorkflow, RunningWorkflow};

    struct MockTemporal {
        stuck: Vec<RunningWorkflow>,
        failed: Vec<FailedWorkflow>,
    }

    #[async_trait]
    impl TemporalClient for MockTemporal {
        async fn list_stuck(&self, _: &str, _: SystemTime) -> Result<Vec<RunningWorkflow>> {
            Ok(self.stuck.iter().map(|w| RunningWorkflow {
                workflow_id: w.workflow_id.clone(),
                workflow_type: w.workflow_type.clone(),
                start_time: w.start_time,
                last_event: w.last_event.clone(),
            }).collect())
        }
        async fn list_failed(&self, _: &str, _: SystemTime) -> Result<Vec<FailedWorkflow>> {
            Ok(self.failed.iter().map(|w| FailedWorkflow {
                workflow_id: w.workflow_id.clone(),
                workflow_type: w.workflow_type.clone(),
                close_time: w.close_time,
            }).collect())
        }
    }

    fn opts() -> RunOpts {
        RunOpts { namespace: "nico".into(), since: Duration::from_secs(600), timeout: Duration::from_secs(5) }
    }

    fn stuck_threshold() -> Duration { Duration::from_secs(30 * 60) }

    #[tokio::test]
    async fn stuck_workflow_shows_running_time_and_temporal_hint() {
        let started = SystemTime::now() - Duration::from_secs(47 * 60);
        let temporal = Arc::new(MockTemporal {
            stuck: vec![RunningWorkflow {
                workflow_id: "wf-001".into(),
                workflow_type: "HostProvisioning".into(),
                start_time: started,
                last_event: "ActivityScheduled".into(),
            }],
            failed: vec![],
        });
        let result = WorkflowsLayer::new(temporal, stuck_threshold()).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let stuck_check = result.checks.iter().find(|c| c.name == "stuck").unwrap();
        assert_eq!(stuck_check.value, "1 stuck");
        let wf_check = result.checks.iter().find(|c| c.name == "stuck_workflow").unwrap();
        assert!(wf_check.value.contains("wf-001"));
        assert!(wf_check.value.contains("HostProvisioning"));
        assert!(wf_check.value.contains("ActivityScheduled"));
        assert!(wf_check.value.contains("47m"), "value: {}", wf_check.value);
        assert_eq!(
            wf_check.next_command.as_deref(),
            Some("temporal workflow show -w wf-001")
        );
    }

    #[tokio::test]
    async fn failed_workflow_shows_with_temporal_hint() {
        let temporal = Arc::new(MockTemporal {
            stuck: vec![],
            failed: vec![FailedWorkflow {
                workflow_id: "wf-002".into(),
                workflow_type: "HostDecommission".into(),
                close_time: SystemTime::now() - Duration::from_secs(300),
            }],
        });
        let result = WorkflowsLayer::new(temporal, stuck_threshold()).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let failed_check = result.checks.iter().find(|c| c.name == "failed").unwrap();
        assert_eq!(failed_check.value, "1 failed");
        let wf_check = result.checks.iter().find(|c| c.name == "failed_workflow").unwrap();
        assert!(wf_check.value.contains("wf-002"));
        assert!(wf_check.value.contains("HostDecommission"));
        assert_eq!(
            wf_check.next_command.as_deref(),
            Some("temporal workflow show -w wf-002")
        );
    }

    #[tokio::test]
    async fn no_stuck_no_failed_reports_ok() {
        let temporal = Arc::new(MockTemporal { stuck: vec![], failed: vec![] });
        let result = WorkflowsLayer::new(temporal, stuck_threshold()).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        assert_eq!(result.checks.iter().find(|c| c.name == "stuck").unwrap().value, "0 stuck");
        assert_eq!(result.checks.iter().find(|c| c.name == "failed").unwrap().value, "0 failed");
    }

    #[tokio::test]
    async fn multiple_stuck_all_appear_as_findings() {
        let now = SystemTime::now();
        let temporal = Arc::new(MockTemporal {
            stuck: vec![
                RunningWorkflow {
                    workflow_id: "wf-a".into(), workflow_type: "HostProvisioning".into(),
                    start_time: now - Duration::from_secs(90 * 60), last_event: "TimerFired".into(),
                },
                RunningWorkflow {
                    workflow_id: "wf-b".into(), workflow_type: "HostProvisioning".into(),
                    start_time: now - Duration::from_secs(35 * 60), last_event: "ActivityStarted".into(),
                },
            ],
            failed: vec![],
        });
        let result = WorkflowsLayer::new(temporal, stuck_threshold()).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let wf_checks: Vec<_> = result.checks.iter().filter(|c| c.name == "stuck_workflow").collect();
        assert_eq!(wf_checks.len(), 2);
    }
}
