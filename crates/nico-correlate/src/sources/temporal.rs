use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceOutput, SourceUnavailable};

#[derive(Clone, Default)]
pub struct RawTemporalEvent {
    pub event_type: String,
    pub ts: DateTime<Utc>,
    pub activity_name: Option<String>,
    pub error_message: Option<String>,
    pub at_max_retries: bool,
}

#[async_trait]
pub trait TemporalClient: Send + Sync {
    async fn get_history(&self, workflow_id: &str) -> Result<Vec<RawTemporalEvent>>;
}

pub struct TemporalSource {
    client: Box<dyn TemporalClient>,
}

impl TemporalSource {
    pub fn new(client: Box<dyn TemporalClient>) -> Self {
        Self { client }
    }
}

fn map_event(raw: RawTemporalEvent) -> Event {
    let severity = Severity::classify("temporal", &raw.event_type, "");
    let mut tags = HashMap::new();
    if let Some(name) = raw.activity_name {
        tags.insert("activity_name".into(), name);
    }
    if let Some(err) = raw.error_message {
        tags.insert("error_signature".into(), err);
    }
    if raw.at_max_retries {
        tags.insert("at_max_retries".into(), "true".into());
    }
    Event {
        ts: raw.ts,
        source: "temporal".into(),
        kind: raw.event_type.clone(),
        message: raw.event_type,
        severity,
        tags,
    }
}

#[async_trait]
impl Source for TemporalSource {
    fn name(&self) -> &'static str {
        "temporal"
    }

    async fn collect(&self, id: &str, _id_type: &IdType) -> SourceResult {
        match self.client.get_history(id).await {
            Ok(raw_events) => SourceResult::Output(SourceOutput {
                events: raw_events.into_iter().map(map_event).collect(),
                state: vec![],
            }),
            Err(e) => SourceResult::Unavailable(SourceUnavailable {
                name: "temporal",
                reason: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct FakeTemporalClient {
        result: Result<Vec<RawTemporalEvent>>,
    }

    impl FakeTemporalClient {
        fn ok(events: Vec<RawTemporalEvent>) -> Self {
            Self { result: Ok(events) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl TemporalClient for FakeTemporalClient {
        async fn get_history(&self, _workflow_id: &str) -> Result<Vec<RawTemporalEvent>> {
            match &self.result {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[tokio::test]
    async fn workflow_started_maps_to_info_event() {
        let client = FakeTemporalClient::ok(vec![
            RawTemporalEvent { event_type: "WorkflowExecutionStarted".into(), ts: ts(1000), ..Default::default() },
        ]);
        let source = TemporalSource::new(Box::new(client));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].kind, "WorkflowExecutionStarted");
        assert_eq!(output.events[0].severity, Severity::Info);
        assert_eq!(output.events[0].source, "temporal");
        assert_eq!(output.events[0].ts, ts(1000));
    }

    #[tokio::test]
    async fn workflow_failed_maps_to_error_event() {
        let client = FakeTemporalClient::ok(vec![
            RawTemporalEvent { event_type: "WorkflowExecutionFailed".into(), ts: ts(2000), ..Default::default() },
        ]);
        let source = TemporalSource::new(Box::new(client));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Error);
    }

    #[tokio::test]
    async fn unavailable_client_returns_unavailable() {
        let client = FakeTemporalClient::err("connection refused");
        let source = TemporalSource::new(Box::new(client));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        match result {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "temporal");
                assert!(u.reason.contains("connection refused"));
            }
            _ => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn workflow_timed_out_maps_to_error_severity() {
        let client = FakeTemporalClient::ok(vec![
            RawTemporalEvent { event_type: "WorkflowExecutionTimedOut".into(), ts: ts(3000), ..Default::default() },
        ]);
        let source = TemporalSource::new(Box::new(client));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Error);
        assert_eq!(output.events[0].kind, "WorkflowExecutionTimedOut");
    }

    #[tokio::test]
    async fn activity_task_failed_maps_to_error_severity() {
        let client = FakeTemporalClient::ok(vec![
            RawTemporalEvent { event_type: "ActivityTaskFailed".into(), ts: ts(4000), ..Default::default() },
        ]);
        let source = TemporalSource::new(Box::new(client));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Error);
    }

    #[tokio::test]
    async fn empty_history_returns_empty_output() {
        let client = FakeTemporalClient::ok(vec![]);
        let source = TemporalSource::new(Box::new(client));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
        assert!(output.state.is_empty());
    }
}
