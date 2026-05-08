use std::sync::Arc;
use std::time::{Duration, SystemTime};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;
use nico_common::temporal::{GrpcTemporalClient, TemporalClient};
use temporal_sdk_core_protos::temporal::api::workflow::v1::WorkflowExecutionInfo;

use crate::bootstrap::LayerInputs;
use crate::layer::{Check, CheckKind, Layer, LayerOutcome, RunOpts};

pub const NAME: &str = "workflows";

/// Factory consumed by `bootstrap::prepare_layers`.
pub fn register(inputs: &LayerInputs) -> Box<dyn Layer> {
    Box::new(WorkflowsLayer::new(
        Arc::new(GrpcTemporalClient::new(inputs.temporal_address.clone())),
        inputs.temporal_namespace.clone(),
        inputs.stuck_threshold,
    ))
}

/// Doctor's view of a running workflow that has exceeded the stuck
/// threshold. Built from a `WorkflowExecutionInfo` returned by the
/// Temporal Visibility API.
#[derive(Clone)]
pub struct RunningWorkflow {
    pub workflow_id: String,
    pub workflow_type: String,
    pub start_time: SystemTime,
    pub last_event: String,
}

/// Doctor's view of a closed workflow that completed in the failed state.
#[derive(Clone)]
pub struct FailedWorkflow {
    pub workflow_id: String,
    pub workflow_type: String,
}

pub struct WorkflowsLayer {
    temporal: Arc<dyn TemporalClient>,
    namespace: String,
    stuck_threshold: Duration,
}

impl WorkflowsLayer {
    pub fn new(
        temporal: Arc<dyn TemporalClient>,
        namespace: String,
        stuck_threshold: Duration,
    ) -> Self {
        Self {
            temporal,
            namespace,
            stuck_threshold,
        }
    }
}

#[async_trait]
impl Layer for WorkflowsLayer {
    fn name(&self) -> &'static str {
        "workflows"
    }

    async fn collect(&self, opts: &RunOpts) -> LayerOutcome {
        let now = SystemTime::now();
        let stuck_before = now
            .checked_sub(self.stuck_threshold)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let failed_since = now.checked_sub(opts.since).unwrap_or(SystemTime::UNIX_EPOCH);

        let stuck = list_stuck(&*self.temporal, &self.namespace, stuck_before)
            .await
            .unwrap_or_default();
        let failed = list_failed(&*self.temporal, &self.namespace, failed_since)
            .await
            .unwrap_or_default();

        LayerOutcome::Checks(checks_from(&stuck, &failed, now))
    }
}

async fn list_stuck(
    client: &dyn TemporalClient,
    namespace: &str,
    stuck_before: SystemTime,
) -> anyhow::Result<Vec<RunningWorkflow>> {
    let stuck_before_dt: DateTime<Utc> = stuck_before.into();
    let query = format!(
        r#"ExecutionStatus = "Running" AND StartTime < "{}""#,
        stuck_before_dt.to_rfc3339()
    );
    let executions = client.list_workflow_executions(namespace, &query, 100).await?;
    Ok(executions.into_iter().map(running_from_proto).collect())
}

async fn list_failed(
    client: &dyn TemporalClient,
    namespace: &str,
    since: SystemTime,
) -> anyhow::Result<Vec<FailedWorkflow>> {
    let since_dt: DateTime<Utc> = since.into();
    let query = format!(
        r#"ExecutionStatus = "Failed" AND CloseTime > "{}""#,
        since_dt.to_rfc3339()
    );
    let executions = client.list_workflow_executions(namespace, &query, 100).await?;
    Ok(executions.into_iter().map(failed_from_proto).collect())
}

fn proto_ts_to_system_time(ts: prost_wkt_types::Timestamp) -> SystemTime {
    if ts.seconds >= 0 {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(ts.seconds as u64)
            + Duration::from_nanos(ts.nanos.max(0) as u64)
    } else {
        SystemTime::UNIX_EPOCH
    }
}

fn running_from_proto(info: WorkflowExecutionInfo) -> RunningWorkflow {
    let workflow_id = info
        .execution
        .as_ref()
        .map(|e| e.workflow_id.clone())
        .unwrap_or_default();
    let workflow_type = info.r#type.as_ref().map(|t| t.name.clone()).unwrap_or_default();
    let start_time = info
        .start_time
        .map(proto_ts_to_system_time)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let last_event = format!("{} events", info.history_length);
    RunningWorkflow {
        workflow_id,
        workflow_type,
        start_time,
        last_event,
    }
}

fn failed_from_proto(info: WorkflowExecutionInfo) -> FailedWorkflow {
    let workflow_id = info
        .execution
        .as_ref()
        .map(|e| e.workflow_id.clone())
        .unwrap_or_default();
    let workflow_type = info.r#type.as_ref().map(|t| t.name.clone()).unwrap_or_default();
    FailedWorkflow {
        workflow_id,
        workflow_type,
    }
}

fn checks_from(stuck: &[RunningWorkflow], failed: &[FailedWorkflow], now: SystemTime) -> Vec<Check> {
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
            kind: CheckKind::Headline,
        },
        Check {
            name: "failed",
            status: if failed.is_empty() { Status::Ok } else { Status::Warn },
            value: format!("{} failed", failed.len()),
            next_command: None,
            kind: CheckKind::Headline,
        },
    ];

    for wf in stuck {
        let running_mins = now
            .duration_since(wf.start_time)
            .unwrap_or_default()
            .as_secs()
            / 60;
        checks.push(Check {
            name: "stuck_workflow",
            status: Status::Warn,
            value: format!(
                "{} ({}): {}m running, last: {}",
                wf.workflow_id, wf.workflow_type, running_mins, wf.last_event
            ),
            next_command: Some(format!("temporal workflow show -w {}", wf.workflow_id)),
            kind: CheckKind::Detail,
        });
    }

    for wf in failed {
        checks.push(Check {
            name: "failed_workflow",
            status: Status::Warn,
            value: format!("{} ({}): failed", wf.workflow_id, wf.workflow_type),
            next_command: Some(format!("temporal workflow show -w {}", wf.workflow_id)),
            kind: CheckKind::Headline,
        });
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use nico_common::output::Status;
    use std::sync::Mutex;
    use std::time::Duration;
    use temporal_sdk_core_protos::temporal::api::common::v1::{
        WorkflowExecution, WorkflowType,
    };

    // --- Sync tests for checks_from (deterministic, fixed `now`). ---

    #[test]
    fn stuck_workflow_checks_are_marked_detail() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let stuck = vec![RunningWorkflow {
            workflow_id: "wf-001".into(),
            workflow_type: "HostProvisioning".into(),
            start_time: now - Duration::from_secs(47 * 60),
            last_event: "47 events".into(),
        }];
        let checks = checks_from(&stuck, &[], now);
        let kinds: Vec<_> = checks.iter()
            .filter(|c| c.name == "stuck_workflow")
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, vec![CheckKind::Detail]);
    }

    #[test]
    fn headline_workflow_checks_remain_headline_kind() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let checks = checks_from(&[], &[], now);
        let stuck = checks.iter().find(|c| c.name == "stuck").unwrap();
        let failed = checks.iter().find(|c| c.name == "failed").unwrap();
        assert_eq!(stuck.kind, CheckKind::Headline);
        assert_eq!(failed.kind, CheckKind::Headline);
    }

    #[test]
    fn checks_from_empty_slices_produces_ok_checks() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let checks = checks_from(&[], &[], now);
        assert_eq!(checks.iter().find(|c| c.name == "stuck").unwrap().status, Status::Ok);
        assert_eq!(checks.iter().find(|c| c.name == "failed").unwrap().status, Status::Ok);
    }

    #[test]
    fn checks_from_one_stuck_workflow_is_warn_with_one_finding() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let stuck = vec![RunningWorkflow {
            workflow_id: "wf-001".into(),
            workflow_type: "HostProvisioning".into(),
            start_time: now - Duration::from_secs(47 * 60),
            last_event: "47 events".into(),
        }];
        let checks = checks_from(&stuck, &[], now);
        assert_eq!(checks.iter().find(|c| c.name == "stuck").unwrap().status, Status::Warn);
        assert_eq!(checks.iter().filter(|c| c.name == "stuck_workflow").count(), 1);
    }

    #[test]
    fn checks_from_one_failed_workflow_is_warn_with_one_finding() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let failed = vec![FailedWorkflow {
            workflow_id: "wf-002".into(),
            workflow_type: "HostDecommission".into(),
        }];
        let checks = checks_from(&[], &failed, now);
        assert_eq!(checks.iter().find(|c| c.name == "failed").unwrap().status, Status::Warn);
        assert_eq!(checks.iter().filter(|c| c.name == "failed_workflow").count(), 1);
    }

    #[test]
    fn checks_from_multiple_stuck_produces_matching_finding_count() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 3600);
        let stuck = vec![
            RunningWorkflow {
                workflow_id: "wf-a".into(),
                workflow_type: "HostProvisioning".into(),
                start_time: now - Duration::from_secs(90 * 60),
                last_event: "TimerFired".into(),
            },
            RunningWorkflow {
                workflow_id: "wf-b".into(),
                workflow_type: "HostProvisioning".into(),
                start_time: now - Duration::from_secs(35 * 60),
                last_event: "ActivityStarted".into(),
            },
        ];
        let checks = checks_from(&stuck, &[], now);
        assert_eq!(checks.iter().filter(|c| c.name == "stuck_workflow").count(), 2);
    }

    // --- Layer integration tests using a query-aware mock. ---

    fn execution_info(id: &str, wf_type: &str, start: SystemTime, history: i64) -> WorkflowExecutionInfo {
        let dt: DateTime<Utc> = start.into();
        WorkflowExecutionInfo {
            execution: Some(WorkflowExecution {
                workflow_id: id.into(),
                run_id: String::new(),
            }),
            r#type: Some(WorkflowType { name: wf_type.into() }),
            start_time: Some(prost_wkt_types::Timestamp {
                seconds: dt.timestamp(),
                nanos: dt.timestamp_subsec_nanos() as i32,
            }),
            history_length: history,
            ..Default::default()
        }
    }

    /// Test mock that dispatches on the visibility query: queries containing
    /// "Running" return the stuck list, queries containing "Failed" return
    /// the failed list. This mirrors how the production code uses the
    /// `list_workflow_executions` primitive for both calls.
    struct QueryDispatchTemporal {
        stuck: Mutex<Vec<WorkflowExecutionInfo>>,
        failed: Mutex<Vec<WorkflowExecutionInfo>>,
    }

    #[async_trait]
    impl TemporalClient for QueryDispatchTemporal {
        async fn list_workflow_executions(
            &self,
            _namespace: &str,
            query: &str,
            _page_size: i32,
        ) -> Result<Vec<WorkflowExecutionInfo>> {
            if query.contains("Running") {
                Ok(self.stuck.lock().unwrap().clone())
            } else if query.contains("Failed") {
                Ok(self.failed.lock().unwrap().clone())
            } else {
                Ok(vec![])
            }
        }

        async fn get_workflow_history(
            &self,
            _namespace: &str,
            _workflow_id: &str,
        ) -> Result<temporal_sdk_core_protos::temporal::api::history::v1::History> {
            Ok(Default::default())
        }
    }

    fn opts() -> RunOpts {
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
            ..Default::default()
        }
    }

    fn stuck_threshold() -> Duration {
        Duration::from_secs(30 * 60)
    }

    #[tokio::test]
    async fn stuck_workflow_shows_running_time_and_temporal_hint() {
        let started = SystemTime::now() - Duration::from_secs(47 * 60);
        let temporal = Arc::new(QueryDispatchTemporal {
            stuck: Mutex::new(vec![execution_info("wf-001", "HostProvisioning", started, 47)]),
            failed: Mutex::new(vec![]),
        });
        let result = WorkflowsLayer::new(temporal, "default".into(), stuck_threshold())
            .run(&opts())
            .await;

        assert_eq!(result.status, Status::Warn);
        let stuck_check = result.checks.iter().find(|c| c.name == "stuck").unwrap();
        assert_eq!(stuck_check.value, "1 stuck");
        let wf_check = result.checks.iter().find(|c| c.name == "stuck_workflow").unwrap();
        assert!(wf_check.value.contains("wf-001"));
        assert!(wf_check.value.contains("HostProvisioning"));
        assert!(wf_check.value.contains("47 events"));
        assert!(wf_check.value.contains("47m"), "value: {}", wf_check.value);
        assert_eq!(
            wf_check.next_command.as_deref(),
            Some("temporal workflow show -w wf-001")
        );
    }

    #[tokio::test]
    async fn failed_workflow_shows_with_temporal_hint() {
        let temporal = Arc::new(QueryDispatchTemporal {
            stuck: Mutex::new(vec![]),
            failed: Mutex::new(vec![execution_info(
                "wf-002",
                "HostDecommission",
                SystemTime::now() - Duration::from_secs(300),
                10,
            )]),
        });
        let result = WorkflowsLayer::new(temporal, "default".into(), stuck_threshold())
            .run(&opts())
            .await;

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
        let temporal = Arc::new(QueryDispatchTemporal {
            stuck: Mutex::new(vec![]),
            failed: Mutex::new(vec![]),
        });
        let result = WorkflowsLayer::new(temporal, "default".into(), stuck_threshold())
            .run(&opts())
            .await;

        assert_eq!(result.status, Status::Ok);
        assert_eq!(result.checks.iter().find(|c| c.name == "stuck").unwrap().value, "0 stuck");
        assert_eq!(result.checks.iter().find(|c| c.name == "failed").unwrap().value, "0 failed");
    }

    #[tokio::test]
    async fn multiple_stuck_all_appear_as_findings() {
        let now = SystemTime::now();
        let temporal = Arc::new(QueryDispatchTemporal {
            stuck: Mutex::new(vec![
                execution_info("wf-a", "HostProvisioning", now - Duration::from_secs(90 * 60), 5),
                execution_info("wf-b", "HostProvisioning", now - Duration::from_secs(35 * 60), 3),
            ]),
            failed: Mutex::new(vec![]),
        });
        let result = WorkflowsLayer::new(temporal, "default".into(), stuck_threshold())
            .run(&opts())
            .await;

        assert_eq!(result.status, Status::Warn);
        let wf_checks: Vec<_> = result
            .checks
            .iter()
            .filter(|c| c.name == "stuck_workflow")
            .collect();
        assert_eq!(wf_checks.len(), 2);
    }
}
