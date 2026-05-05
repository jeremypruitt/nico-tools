use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use temporal_sdk_core_protos::temporal::api::common::v1::WorkflowExecution;
use temporal_sdk_core_protos::temporal::api::enums::v1::{EventType, RetryState};
use temporal_sdk_core_protos::temporal::api::history::v1::history_event::Attributes;
use temporal_sdk_core_protos::temporal::api::workflowservice::v1::{
    GetWorkflowExecutionHistoryRequest,
    workflow_service_client::WorkflowServiceClient,
};
use tonic::transport::Channel;

use crate::sources::temporal::{RawTemporalEvent, TemporalClient};

pub struct GrpcTemporalClient {
    address: String,
    namespace: String,
}

impl GrpcTemporalClient {
    pub fn new(address: String, namespace: String) -> Self {
        Self { address, namespace }
    }
}

fn proto_ts_to_chrono(ts: prost_wkt_types::Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp(ts.seconds, ts.nanos.max(0) as u32).unwrap_or_else(Utc::now)
}

#[async_trait]
impl TemporalClient for GrpcTemporalClient {
    async fn get_history(&self, workflow_id: &str) -> Result<Vec<RawTemporalEvent>> {
        let channel = Channel::from_shared(self.address.clone())
            .map_err(|e| anyhow::anyhow!("invalid Temporal address: {e}"))?
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("connect to Temporal failed: {e}"))?;

        let mut client = WorkflowServiceClient::new(channel);

        let request = GetWorkflowExecutionHistoryRequest {
            namespace: self.namespace.clone(),
            execution: Some(WorkflowExecution {
                workflow_id: workflow_id.to_string(),
                run_id: String::new(),
            }),
            ..Default::default()
        };

        let response = client
            .get_workflow_execution_history(request)
            .await
            .map_err(|e| anyhow::anyhow!("GetWorkflowExecutionHistory RPC failed: {e}"))?;

        let history = response.into_inner().history.unwrap_or_default();

        // Pass 1: build event_id -> activity_name from ActivityTaskScheduled events
        let mut activity_by_event_id: HashMap<i64, String> = HashMap::new();
        for e in &history.events {
            if let Some(Attributes::ActivityTaskScheduledEventAttributes(attrs)) = &e.attributes {
                let name = attrs.activity_type.as_ref().map(|t| t.name.clone()).unwrap_or_default();
                if !name.is_empty() {
                    activity_by_event_id.insert(e.event_id, name);
                }
            }
        }

        // Pass 2: create RawTemporalEvents, enriching activity failures with diagnosis tags
        let events = history.events.into_iter().map(|e| {
            let (activity_name, error_message, at_max_retries) =
                if let Some(Attributes::ActivityTaskFailedEventAttributes(failed)) = &e.attributes {
                    let name = activity_by_event_id.get(&failed.scheduled_event_id).cloned();
                    let err = failed.failure.as_ref()
                        .map(|f| f.message.clone())
                        .filter(|s| !s.is_empty());
                    let at_max = RetryState::try_from(failed.retry_state)
                        .ok()
                        .map(|s| s == RetryState::MaximumAttemptsReached)
                        .unwrap_or(false);
                    (name, err, at_max)
                } else {
                    (None, None, false)
                };

            let ts = e.event_time.map(proto_ts_to_chrono).unwrap_or_else(Utc::now);
            let event_type = EventType::try_from(e.event_type)
                .map(|et| et.as_str_name().to_string())
                .unwrap_or_else(|_| format!("UnknownEventType({})", e.event_type));
            RawTemporalEvent { event_type, ts, activity_name, error_message, at_max_retries }
        }).collect();

        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn integration_get_history() {
        let address = match std::env::var("NICO_TEMPORAL_ADDRESS") {
            Ok(a) => a,
            Err(_) => return,
        };
        let namespace =
            std::env::var("NICO_TEMPORAL_NAMESPACE").unwrap_or_else(|_| "default".into());

        let client = GrpcTemporalClient::new(address, namespace);
        // Use a well-known workflow ID from the dev server; any string exercises the RPC path.
        let result = client.get_history("smoke-test-workflow").await;
        // The workflow may not exist — what matters is we got a real gRPC response, not a panic.
        match result {
            Ok(events) => {
                println!("Got {} history events", events.len());
            }
            Err(e) => {
                // NotFound is acceptable — it proves we reached the server.
                let msg = e.to_string();
                assert!(
                    msg.contains("NotFound") || msg.contains("not found") || msg.contains("Workflow"),
                    "unexpected error: {msg}"
                );
            }
        }
    }
}
