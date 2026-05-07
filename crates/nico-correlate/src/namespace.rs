//! Namespace-scoped event fan-out: collect recent events from the
//! temporal + k8s Sources for a namespace, with no entity-scoping. Used
//! by the `nico ops` Mission Control (Layout B) Activity quadrant.
//!
//! Unlike the `Source::collect(id, id_type)` path used elsewhere, this
//! function fans out at the namespace level and returns the most recent
//! events merged across both sources.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use nico_common::k8s::{K8sClient, RawEvent};
use nico_common::temporal::TemporalClient;

use crate::event::{Event, Severity};

/// Maximum number of events returned by [`recent_namespace_events`].
pub const RECENT_EVENT_CAP: usize = 20;

/// Fan-out collect over the temporal + k8s Sources for a namespace, no
/// entity-scoping. Returns up to [`RECENT_EVENT_CAP`] events sorted
/// newest-first. Sources that fail are silently skipped — the Activity
/// quadrant degrades gracefully when one feed is unreachable.
pub async fn recent_namespace_events(
    temporal: Arc<dyn TemporalClient>,
    k8s: Arc<dyn K8sClient>,
    namespace: &str,
    since: Duration,
) -> Vec<Event> {
    let cutoff = Utc::now() - since;

    let temporal_fut = collect_temporal(temporal.as_ref(), namespace, cutoff);
    let k8s_fut = collect_k8s(k8s.as_ref(), namespace, cutoff);
    let (mut events, mut k8s_events) = tokio::join!(temporal_fut, k8s_fut);
    events.append(&mut k8s_events);

    events.sort_by_key(|e| std::cmp::Reverse(e.ts));
    events.truncate(RECENT_EVENT_CAP);
    events
}

async fn collect_temporal(
    client: &dyn TemporalClient,
    namespace: &str,
    cutoff: DateTime<Utc>,
) -> Vec<Event> {
    // Visibility query for executions started within the look-back window.
    // ORDER BY puts most recent first; page size is capped to RECENT_EVENT_CAP
    // since we only ever return that many.
    let query = format!(
        "StartTime > '{}' ORDER BY StartTime DESC",
        cutoff.to_rfc3339()
    );
    let executions = match client
        .list_workflow_executions(namespace, &query, RECENT_EVENT_CAP as i32)
        .await
    {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    executions
        .into_iter()
        .filter_map(workflow_execution_to_event)
        .filter(|e| e.ts >= cutoff)
        .collect()
}

fn workflow_execution_to_event(
    info: temporal_sdk_core_protos::temporal::api::workflow::v1::WorkflowExecutionInfo,
) -> Option<Event> {
    let exec = info.execution.as_ref()?;
    let workflow_id = exec.workflow_id.clone();
    let workflow_type = info.r#type.as_ref().map(|t| t.name.clone()).unwrap_or_default();
    let ts = info
        .start_time
        .as_ref()
        .and_then(|t| DateTime::from_timestamp(t.seconds, t.nanos.max(0) as u32))?;

    let kind = if workflow_type.is_empty() {
        "WorkflowExecution".to_string()
    } else {
        workflow_type.clone()
    };
    let mut tags = HashMap::new();
    tags.insert("workflow_id".to_string(), workflow_id.clone());
    if !workflow_type.is_empty() {
        tags.insert("workflow_type".to_string(), workflow_type);
    }
    Some(Event {
        ts,
        source: "temporal".into(),
        kind,
        message: workflow_id,
        severity: Severity::Info,
        tags,
    })
}

async fn collect_k8s(
    client: &dyn K8sClient,
    namespace: &str,
    cutoff: DateTime<Utc>,
) -> Vec<Event> {
    let raw = match client.list_events(namespace, Some("type=Warning")).await {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    raw.into_iter()
        .filter_map(raw_event_to_event)
        .filter(|e| e.ts >= cutoff)
        .collect()
}

fn raw_event_to_event(raw: RawEvent) -> Option<Event> {
    let ts = raw.ts?;
    let reason = raw.reason.unwrap_or_default();
    let message = raw.message.unwrap_or_default();
    let severity = Severity::classify("k8s", &reason, &message);
    Some(Event {
        ts,
        source: "k8s".into(),
        kind: reason,
        message,
        severity,
        tags: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nico_common::k8s::testing::MockK8sClient;
    use nico_common::temporal::testing::MockTemporalClient;
    use prost_wkt_types::Timestamp;
    use temporal_sdk_core_protos::temporal::api::common::v1::{WorkflowExecution, WorkflowType};
    use temporal_sdk_core_protos::temporal::api::workflow::v1::WorkflowExecutionInfo;

    fn proto_ts(ts: DateTime<Utc>) -> Timestamp {
        Timestamp {
            seconds: ts.timestamp(),
            nanos: 0,
        }
    }

    fn workflow_info(id: &str, ty: &str, ts: DateTime<Utc>) -> WorkflowExecutionInfo {
        WorkflowExecutionInfo {
            execution: Some(WorkflowExecution {
                workflow_id: id.into(),
                run_id: "run-1".into(),
            }),
            r#type: Some(WorkflowType { name: ty.into() }),
            start_time: Some(proto_ts(ts)),
            ..Default::default()
        }
    }

    fn warning_event(reason: &str, message: &str, ts: DateTime<Utc>) -> RawEvent {
        RawEvent {
            ts: Some(ts),
            event_type: Some("Warning".into()),
            reason: Some(reason.into()),
            message: Some(message.into()),
        }
    }

    #[tokio::test]
    async fn fans_out_temporal_and_k8s_into_one_merged_list() {
        let now = Utc::now();
        let temporal = Arc::new(
            MockTemporalClient::new()
                .with_executions(vec![workflow_info("hp-1", "HostProvisioning", now)]),
        );
        let k8s = Arc::new(
            MockK8sClient::new().with_events(vec![warning_event("OOMKilled", "boom", now)]),
        );

        let events =
            recent_namespace_events(temporal, k8s, "nico", Duration::minutes(10)).await;

        let sources: Vec<&str> = events.iter().map(|e| e.source.as_str()).collect();
        assert!(sources.contains(&"temporal"), "missing temporal: {sources:?}");
        assert!(sources.contains(&"k8s"), "missing k8s: {sources:?}");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn merged_events_are_sorted_newest_first() {
        let now = Utc::now();
        let temporal = Arc::new(MockTemporalClient::new().with_executions(vec![
            workflow_info("hp-old", "HostProvisioning", now - Duration::minutes(5)),
        ]));
        let k8s = Arc::new(MockK8sClient::new().with_events(vec![warning_event(
            "Crash",
            "x",
            now - Duration::minutes(1),
        )]));

        let events =
            recent_namespace_events(temporal, k8s, "nico", Duration::minutes(10)).await;

        assert_eq!(events[0].source, "k8s");
        assert_eq!(events[1].source, "temporal");
    }

    #[tokio::test]
    async fn truncates_to_recent_event_cap() {
        let now = Utc::now();
        // Build CAP+5 distinct events all from k8s, each at a unique recent ts.
        let raw_events: Vec<RawEvent> = (0..(RECENT_EVENT_CAP + 5))
            .map(|i| warning_event("Reason", "msg", now - Duration::seconds(i as i64)))
            .collect();
        let temporal = Arc::new(MockTemporalClient::new());
        let k8s = Arc::new(MockK8sClient::new().with_events(raw_events));

        let events =
            recent_namespace_events(temporal, k8s, "nico", Duration::minutes(10)).await;
        assert_eq!(events.len(), RECENT_EVENT_CAP);
    }

    #[tokio::test]
    async fn temporal_failure_yields_only_k8s_events() {
        let temporal = Arc::new(MockTemporalClient::new().with_executions_err("boom"));
        let k8s = Arc::new(
            MockK8sClient::new().with_events(vec![warning_event("Crash", "x", Utc::now())]),
        );

        let events =
            recent_namespace_events(temporal, k8s, "nico", Duration::minutes(10)).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "k8s");
    }

    #[tokio::test]
    async fn k8s_failure_yields_only_temporal_events() {
        let temporal = Arc::new(
            MockTemporalClient::new()
                .with_executions(vec![workflow_info("hp-1", "HostProvisioning", Utc::now())]),
        );
        let k8s = Arc::new(MockK8sClient::new().with_events_err("nope"));

        let events =
            recent_namespace_events(temporal, k8s, "nico", Duration::minutes(10)).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "temporal");
    }

    #[tokio::test]
    async fn drops_temporal_executions_older_than_since_cutoff() {
        let now = Utc::now();
        let stale = now - Duration::hours(2);
        let temporal = Arc::new(
            MockTemporalClient::new()
                .with_executions(vec![workflow_info("hp-stale", "Old", stale)]),
        );
        let k8s = Arc::new(MockK8sClient::new());

        let events =
            recent_namespace_events(temporal, k8s, "nico", Duration::minutes(10)).await;
        assert!(events.is_empty(), "stale execution should be dropped");
    }

    #[tokio::test]
    async fn workflow_event_carries_workflow_id_and_type_tags() {
        let temporal = Arc::new(
            MockTemporalClient::new()
                .with_executions(vec![workflow_info("hp-7", "HostProvisioning", Utc::now())]),
        );
        let k8s = Arc::new(MockK8sClient::new());

        let events = recent_namespace_events(temporal, k8s, "nico", Duration::hours(1)).await;
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind, "HostProvisioning");
        assert_eq!(e.tags.get("workflow_id").map(String::as_str), Some("hp-7"));
        assert_eq!(
            e.tags.get("workflow_type").map(String::as_str),
            Some("HostProvisioning")
        );
    }
}
