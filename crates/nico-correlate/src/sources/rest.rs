use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use kube::{Client as KubeClient, Api};
use kube::api::{ListParams, LogParams};
use k8s_openapi::api::core::v1::Pod;
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceOutput, SourceUnavailable};

#[derive(Deserialize)]
struct RestAccessLog {
    ts: Option<DateTime<Utc>>,
    method: Option<String>,
    path: Option<String>,
    status: Option<u16>,
    request_id: Option<String>,
    workflow_id: Option<String>,
}

/// Returns raw log lines from infra-controller-rest pods (labeled app=rest).
#[async_trait]
pub trait RestLogClient: Send + Sync {
    async fn get_access_logs(&self, since: Duration) -> Result<Vec<String>>;
}

pub struct RestSource {
    client: Box<dyn RestLogClient>,
    since: Duration,
}

impl RestSource {
    pub fn new(client: Box<dyn RestLogClient>, since: Duration) -> Self {
        Self { client, since }
    }
}

fn matches_id(log: &RestAccessLog, id: &str, id_type: &IdType) -> bool {
    match id_type {
        IdType::Request  => log.request_id.as_deref() == Some(id),
        IdType::Workflow => log.workflow_id.as_deref() == Some(id),
        _ => false,
    }
}

fn log_to_event(log: RestAccessLog) -> Event {
    let ts = log.ts.unwrap_or_else(Utc::now);
    let method = log.method.as_deref().unwrap_or("?");
    let path   = log.path.as_deref().unwrap_or("?");
    let status = log.status.unwrap_or(0);
    let message = format!("{method} {path} → {status}");

    let severity = match status {
        500..=599 => Severity::Error,
        400..=499 => Severity::Warning,
        _ => Severity::Info,
    };

    let mut tags = HashMap::new();
    if let Some(req_id) = log.request_id {
        tags.insert("request_id".to_string(), req_id);
    }
    if let Some(wf_id) = log.workflow_id {
        tags.insert("workflow_id".to_string(), wf_id);
    }

    Event {
        ts,
        source: "rest".into(),
        kind: "AccessRequest".into(),
        message,
        severity,
        tags,
    }
}

#[async_trait]
impl Source for RestSource {
    fn name(&self) -> &'static str {
        "rest"
    }

    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult {
        match self.client.get_access_logs(self.since).await {
            Ok(lines) => {
                let events = lines
                    .into_iter()
                    .filter_map(|line| serde_json::from_str::<RestAccessLog>(&line).ok())
                    .filter(|log| matches_id(log, id, id_type))
                    .map(log_to_event)
                    .collect();
                SourceResult::Output(SourceOutput { events, state: vec![] })
            }
            Err(e) => SourceResult::Unavailable(SourceUnavailable {
                name: "rest",
                reason: e.to_string(),
            }),
        }
    }
}

pub struct RealRestLogClient {
    client: KubeClient,
    namespace: String,
}

impl RealRestLogClient {
    pub fn new(client: KubeClient, namespace: impl Into<String>) -> Self {
        Self { client, namespace: namespace.into() }
    }
}

#[async_trait]
impl RestLogClient for RealRestLogClient {
    async fn get_access_logs(&self, since: Duration) -> Result<Vec<String>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_list = pods
            .list(&ListParams::default().labels("app=rest"))
            .await
            .map_err(|e| anyhow::anyhow!("k8s pod list failed: {e}"))?;

        let mut lines = Vec::new();
        for pod in pod_list.items {
            let pod_name = pod.metadata.name.unwrap_or_default();
            let lp = LogParams {
                since_seconds: Some(since.num_seconds()),
                ..Default::default()
            };
            let log_data = pods.logs(&pod_name, &lp).await.unwrap_or_default();
            for line in log_data.lines() {
                lines.push(line.to_string());
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct FakeRestLogClient {
        result: Result<Vec<String>>,
    }

    impl FakeRestLogClient {
        fn ok(lines: Vec<&str>) -> Self {
            Self { result: Ok(lines.into_iter().map(str::to_string).collect()) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl RestLogClient for FakeRestLogClient {
        async fn get_access_logs(&self, _since: Duration) -> Result<Vec<String>> {
            match &self.result {
                Ok(lines) => Ok(lines.clone()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn make_source(client: impl RestLogClient + 'static) -> RestSource {
        RestSource::new(Box::new(client), Duration::hours(1))
    }

    // --- tracer bullet ---

    #[tokio::test]
    async fn json_access_log_with_matching_request_id_produces_event() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"POST","path":"/v1/hosts","status":200,"request_id":"req-a83b","workflow_id":"hp-7f3a2c"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].source, "rest");
        assert_eq!(output.events[0].kind, "AccessRequest");
        assert_eq!(output.events[0].message, "POST /v1/hosts → 200");
    }

    // --- tags link workflow_id ---

    #[tokio::test]
    async fn matched_event_tags_include_workflow_id() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"POST","path":"/v1/hosts","status":200,"request_id":"req-a83b","workflow_id":"hp-7f3a2c"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].tags.get("workflow_id"), Some(&"hp-7f3a2c".to_string()));
        assert_eq!(output.events[0].tags.get("request_id"), Some(&"req-a83b".to_string()));
    }

    // --- workflow ID reverse lookup ---

    #[tokio::test]
    async fn workflow_id_lookup_returns_log_that_triggered_it() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"POST","path":"/v1/hosts","status":200,"request_id":"req-a83b","workflow_id":"hp-7f3a2c"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("hp-7f3a2c", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].tags.get("request_id"), Some(&"req-a83b".to_string()));
    }

    // --- non-JSON lines skipped ---

    #[tokio::test]
    async fn non_json_lines_are_skipped() {
        let source = make_source(FakeRestLogClient::ok(vec![
            "plain text line not json",
            r#"{"ts":"2024-01-15T10:30:00Z","method":"GET","path":"/healthz","status":200,"request_id":"req-0001"}"#,
        ]));
        let output = match source.collect("req-0001", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
    }

    // --- non-matching lines filtered ---

    #[tokio::test]
    async fn lines_for_other_request_ids_are_excluded() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"POST","path":"/v1/hosts","status":200,"request_id":"req-other","workflow_id":"hp-other"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
    }

    // --- client error → Unavailable ---

    #[tokio::test]
    async fn client_error_returns_unavailable() {
        let source = make_source(FakeRestLogClient::err("pods unreachable"));
        match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "rest");
                assert!(u.reason.contains("pods unreachable"));
            }
            _ => panic!("expected Unavailable"),
        }
    }

    // --- HTTP status severity ---

    #[tokio::test]
    async fn status_5xx_produces_error_severity() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"POST","path":"/v1/hosts","status":503,"request_id":"req-a83b"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Error);
    }

    #[tokio::test]
    async fn status_4xx_produces_warning_severity() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"GET","path":"/v1/hosts/bad","status":404,"request_id":"req-a83b"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Warning);
    }

    #[tokio::test]
    async fn status_2xx_produces_info_severity() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"GET","path":"/v1/hosts","status":200,"request_id":"req-a83b"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Info);
    }

    // --- empty OK returns empty Output ---

    #[tokio::test]
    async fn no_matching_lines_returns_empty_output() {
        let source = make_source(FakeRestLogClient::ok(vec![]));
        let output = match source.collect("req-a83b", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
        assert!(output.state.is_empty());
    }

    // --- timestamp from JSON field ---

    #[tokio::test]
    async fn event_timestamp_comes_from_json_ts_field() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"GET","path":"/healthz","status":200,"request_id":"req-ts"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("req-ts", &IdType::Request).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        let expected_ts = "2024-01-15T10:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(output.events[0].ts, expected_ts);
    }

    // --- host/DPU entities produce no matches (rest is request/workflow scoped) ---

    #[tokio::test]
    async fn host_entity_type_returns_empty_output() {
        let line = r#"{"ts":"2024-01-15T10:30:00Z","method":"POST","path":"/v1/hosts","status":200,"request_id":"req-a83b","workflow_id":"hp-7f3a2c"}"#;
        let source = make_source(FakeRestLogClient::ok(vec![line]));
        let output = match source.collect("host-r12u5", &IdType::Host).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
    }
}
