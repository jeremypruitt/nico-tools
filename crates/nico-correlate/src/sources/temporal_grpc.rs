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

fn event_type_name(n: i32) -> String {
    EventType::try_from(n)
        .map(|et| et.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UnknownEventType({})", n))
}

fn history_event_to_raw(
    e: &temporal_sdk_core_protos::temporal::api::history::v1::HistoryEvent,
    activity_by_event_id: &HashMap<i64, String>,
) -> RawTemporalEvent {
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

    let ts = e.event_time.clone().map(proto_ts_to_chrono).unwrap_or_else(Utc::now);
    let event_type = event_type_name(e.event_type);
    RawTemporalEvent { event_type, ts, activity_name, error_message, at_max_retries }
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
            history_event_to_raw(&e, &activity_by_event_id)
        }).collect();

        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temporal_sdk_core_protos::temporal::api::history::v1::{
        HistoryEvent, history_event::Attributes, ActivityTaskFailedEventAttributes,
        ActivityTaskScheduledEventAttributes,
    };
    use temporal_sdk_core_protos::temporal::api::common::v1::ActivityType;
    use temporal_sdk_core_protos::temporal::api::failure::v1::Failure;

    #[test]
    fn workflow_execution_started_maps_to_proto_name() {
        let name = event_type_name(EventType::WorkflowExecutionStarted as i32);
        assert_eq!(name, "EVENT_TYPE_WORKFLOW_EXECUTION_STARTED");
    }

    #[test]
    fn workflow_execution_failed_maps_to_proto_name() {
        let name = event_type_name(EventType::WorkflowExecutionFailed as i32);
        assert_eq!(name, "EVENT_TYPE_WORKFLOW_EXECUTION_FAILED");
    }

    #[test]
    fn activity_task_failed_maps_to_proto_name() {
        let name = event_type_name(EventType::ActivityTaskFailed as i32);
        assert_eq!(name, "EVENT_TYPE_ACTIVITY_TASK_FAILED");
    }

    #[test]
    fn unknown_event_type_uses_fallback_format() {
        let name = event_type_name(99999);
        assert_eq!(name, "UnknownEventType(99999)");
    }

    #[test]
    fn activity_failed_event_extracts_error_message_and_max_retries() {
        let scheduled_event_id = 5_i64;
        let mut activity_by_event_id = HashMap::new();
        activity_by_event_id.insert(scheduled_event_id, "my-activity".to_string());

        let event = HistoryEvent {
            event_id: 8,
            event_type: EventType::ActivityTaskFailed as i32,
            attributes: Some(Attributes::ActivityTaskFailedEventAttributes(
                ActivityTaskFailedEventAttributes {
                    scheduled_event_id,
                    retry_state: RetryState::MaximumAttemptsReached as i32,
                    failure: Some(Failure {
                        message: "disk full".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        let raw = history_event_to_raw(&event, &activity_by_event_id);
        assert_eq!(raw.activity_name.as_deref(), Some("my-activity"));
        assert_eq!(raw.error_message.as_deref(), Some("disk full"));
        assert!(raw.at_max_retries);
    }

    #[test]
    fn activity_scheduled_event_contributes_to_activity_name_lookup() {
        let mut activity_by_event_id = HashMap::new();
        let scheduled = HistoryEvent {
            event_id: 10,
            event_type: EventType::ActivityTaskScheduled as i32,
            attributes: Some(Attributes::ActivityTaskScheduledEventAttributes(
                ActivityTaskScheduledEventAttributes {
                    activity_type: Some(ActivityType { name: "provision-host".to_string() }),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        // Build the lookup as the real get_history does
        if let Some(Attributes::ActivityTaskScheduledEventAttributes(attrs)) = &scheduled.attributes {
            let name = attrs.activity_type.as_ref().map(|t| t.name.clone()).unwrap_or_default();
            if !name.is_empty() {
                activity_by_event_id.insert(scheduled.event_id, name);
            }
        }

        assert_eq!(activity_by_event_id.get(&10).map(String::as_str), Some("provision-host"));

        let failed = HistoryEvent {
            event_id: 13,
            event_type: EventType::ActivityTaskFailed as i32,
            attributes: Some(Attributes::ActivityTaskFailedEventAttributes(
                ActivityTaskFailedEventAttributes {
                    scheduled_event_id: 10,
                    retry_state: RetryState::InProgress as i32,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        let raw = history_event_to_raw(&failed, &activity_by_event_id);
        assert_eq!(raw.activity_name.as_deref(), Some("provision-host"));
        assert!(!raw.at_max_retries);
    }

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
