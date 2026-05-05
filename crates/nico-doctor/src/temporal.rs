use std::time::{Duration, SystemTime};
use async_trait::async_trait;
use anyhow::Result;
use chrono::{DateTime, Utc};
use temporal_sdk_core_protos::temporal::api::workflowservice::v1::{
    ListWorkflowExecutionsRequest,
    workflow_service_client::WorkflowServiceClient,
};
use tonic::transport::Channel;

pub struct RunningWorkflow {
    pub workflow_id: String,
    pub workflow_type: String,
    pub start_time: SystemTime,
    pub last_event: String,
}

pub struct FailedWorkflow {
    pub workflow_id: String,
    pub workflow_type: String,
    #[allow(dead_code)]
    pub close_time: SystemTime,
}

#[async_trait]
pub trait TemporalClient: Send + Sync {
    /// Running workflows whose start_time is earlier than stuck_before.
    async fn list_stuck(&self, namespace: &str, stuck_before: SystemTime) -> Result<Vec<RunningWorkflow>>;
    /// Workflows that failed after the given since time.
    async fn list_failed(&self, namespace: &str, since: SystemTime) -> Result<Vec<FailedWorkflow>>;
}

pub struct RealTemporalClient {
    address: String,
    namespace: String,
}

impl RealTemporalClient {
    pub fn new(address: String, namespace: String) -> Self {
        Self { address, namespace }
    }

    async fn connect(&self) -> Result<WorkflowServiceClient<Channel>> {
        let channel = Channel::from_shared(self.address.clone())
            .map_err(|e| anyhow::anyhow!("invalid Temporal address: {e}"))?
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("connect to Temporal failed: {e}"))?;
        Ok(WorkflowServiceClient::new(channel))
    }
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

#[async_trait]
impl TemporalClient for RealTemporalClient {
    async fn list_stuck(&self, _namespace: &str, stuck_before: SystemTime) -> Result<Vec<RunningWorkflow>> {
        let stuck_before_dt: DateTime<Utc> = stuck_before.into();
        let query = format!(
            r#"ExecutionStatus = "Running" AND StartTime < "{}""#,
            stuck_before_dt.to_rfc3339()
        );

        let mut client = self.connect().await?;
        let response = client
            .list_workflow_executions(ListWorkflowExecutionsRequest {
                namespace: self.namespace.clone(),
                query,
                page_size: 100,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("ListWorkflowExecutions RPC failed: {e}"))?;

        let workflows = response
            .into_inner()
            .executions
            .into_iter()
            .filter_map(|info| {
                let execution = info.execution?;
                let wf_type = info.r#type.map(|t| t.name).unwrap_or_default();
                let start_time = info
                    .start_time
                    .map(proto_ts_to_system_time)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some(RunningWorkflow {
                    workflow_id: execution.workflow_id,
                    workflow_type: wf_type,
                    start_time,
                    last_event: format!("{} events", info.history_length),
                })
            })
            .collect();

        Ok(workflows)
    }

    async fn list_failed(&self, _namespace: &str, since: SystemTime) -> Result<Vec<FailedWorkflow>> {
        let since_dt: DateTime<Utc> = since.into();
        let query = format!(
            r#"ExecutionStatus = "Failed" AND CloseTime > "{}""#,
            since_dt.to_rfc3339()
        );

        let mut client = self.connect().await?;
        let response = client
            .list_workflow_executions(ListWorkflowExecutionsRequest {
                namespace: self.namespace.clone(),
                query,
                page_size: 100,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("ListWorkflowExecutions RPC failed: {e}"))?;

        let workflows = response
            .into_inner()
            .executions
            .into_iter()
            .filter_map(|info| {
                let execution = info.execution?;
                let wf_type = info.r#type.map(|t| t.name).unwrap_or_default();
                let close_time = info
                    .close_time
                    .map(proto_ts_to_system_time)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some(FailedWorkflow {
                    workflow_id: execution.workflow_id,
                    workflow_type: wf_type,
                    close_time,
                })
            })
            .collect();

        Ok(workflows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn integration_list_stuck() {
        let address = match std::env::var("NICO_TEMPORAL_ADDRESS") {
            Ok(a) => a,
            Err(_) => return,
        };
        let namespace =
            std::env::var("NICO_TEMPORAL_NAMESPACE").unwrap_or_else(|_| "default".into());

        let client = RealTemporalClient::new(address, namespace);
        let stuck_before = SystemTime::now();
        let result = client.list_stuck("", stuck_before).await;
        match result {
            Ok(workflows) => println!("Got {} stuck workflows", workflows.len()),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("NotFound") || msg.contains("not found") || msg.contains("connect"),
                    "unexpected error: {msg}"
                );
            }
        }
    }

    #[tokio::test]
    async fn integration_list_failed() {
        let address = match std::env::var("NICO_TEMPORAL_ADDRESS") {
            Ok(a) => a,
            Err(_) => return,
        };
        let namespace =
            std::env::var("NICO_TEMPORAL_NAMESPACE").unwrap_or_else(|_| "default".into());

        let client = RealTemporalClient::new(address, namespace);
        let since = SystemTime::now() - Duration::from_secs(3600);
        let result = client.list_failed("", since).await;
        match result {
            Ok(workflows) => println!("Got {} failed workflows", workflows.len()),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("NotFound") || msg.contains("not found") || msg.contains("connect"),
                    "unexpected error: {msg}"
                );
            }
        }
    }
}
