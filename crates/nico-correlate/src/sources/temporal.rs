use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceUnavailable};

pub struct RawTemporalEvent {
    pub event_type: String,
    pub ts: DateTime<Utc>,
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
    let severity = if raw.event_type.contains("Failed") || raw.event_type.contains("TimedOut") {
        Severity::Error
    } else {
        Severity::Info
    };
    Event {
        ts: raw.ts,
        source: "temporal".into(),
        kind: raw.event_type.clone(),
        message: raw.event_type,
        severity,
    }
}

#[async_trait]
impl Source for TemporalSource {
    fn name(&self) -> &'static str {
        "temporal"
    }

    async fn collect(&self, id: &str, _id_type: &IdType) -> SourceResult {
        match self.client.get_history(id).await {
            Ok(raw_events) => SourceResult::Events(raw_events.into_iter().map(map_event).collect()),
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
                Ok(v) => Ok(v.iter().map(|e| RawTemporalEvent {
                    event_type: e.event_type.clone(),
                    ts: e.ts,
                }).collect()),
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
            RawTemporalEvent { event_type: "WorkflowExecutionStarted".into(), ts: ts(1000) },
        ]);
        let source = TemporalSource::new(Box::new(client));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let events = match result {
            SourceResult::Events(e) => e,
            _ => panic!("expected Events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "WorkflowExecutionStarted");
        assert_eq!(events[0].severity, Severity::Info);
        assert_eq!(events[0].source, "temporal");
        assert_eq!(events[0].ts, ts(1000));
    }

    #[tokio::test]
    async fn workflow_failed_maps_to_error_event() {
        let client = FakeTemporalClient::ok(vec![
            RawTemporalEvent { event_type: "WorkflowExecutionFailed".into(), ts: ts(2000) },
        ]);
        let source = TemporalSource::new(Box::new(client));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let events = match result {
            SourceResult::Events(e) => e,
            _ => panic!("expected Events"),
        };
        assert_eq!(events[0].severity, Severity::Error);
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
}
