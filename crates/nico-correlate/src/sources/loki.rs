use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::collections::HashMap;
use kube::{Client as KubeClient, Api};
use kube::api::{ListParams, LogParams};
use k8s_openapi::api::core::v1::Pod;
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceOutput, SourceUnavailable, StateEntry};

pub struct LokiLogLine {
    pub ts: DateTime<Utc>,
    pub message: String,
    #[allow(dead_code)]
    pub pod: Option<String>,
    pub is_serial_console: bool,
}

#[async_trait]
pub trait LokiClient: Send + Sync {
    async fn query_range(
        &self,
        id: &str,
        id_type: &IdType,
        since: Duration,
        pod_pattern: Option<&str>,
    ) -> Result<Vec<LokiLogLine>>;
}

pub struct K8sLogLine {
    pub ts: DateTime<Utc>,
    pub message: String,
    pub pod: String,
}

#[async_trait]
pub trait K8sLogStreamClient: Send + Sync {
    async fn stream_logs(
        &self,
        id: &str,
        id_type: &IdType,
        since: Duration,
        pod_pattern: Option<&str>,
    ) -> Result<Vec<K8sLogLine>>;
}

pub struct LokiSource {
    loki: Box<dyn LokiClient>,
    k8s_fallback: Option<Box<dyn K8sLogStreamClient>>,
    pub pod_pattern: Option<String>,
    pub since: Duration,
}

impl LokiSource {
    pub fn new(
        loki: Box<dyn LokiClient>,
        k8s_fallback: Option<Box<dyn K8sLogStreamClient>>,
        pod_pattern: Option<String>,
        since: Duration,
    ) -> Self {
        Self { loki, k8s_fallback, pod_pattern, since }
    }
}

fn loki_line_to_event(line: LokiLogLine) -> Event {
    let kind = if line.is_serial_console { "SerialConsoleLog" } else { "Log" };
    Event {
        ts: line.ts,
        source: "loki".into(),
        kind: kind.into(),
        message: line.message.clone(),
        severity: Severity::classify("loki", kind, &line.message),
        tags: Default::default(),
    }
}

fn k8s_line_to_event(line: K8sLogLine) -> Event {
    let message = format!("[{}] {}", line.pod, line.message);
    Event {
        ts: line.ts,
        source: "k8s-logs".into(),
        kind: "Log".into(),
        message: message.clone(),
        severity: Severity::classify("k8s-logs", "Log", &message),
        tags: Default::default(),
    }
}

#[async_trait]
impl Source for LokiSource {
    fn name(&self) -> &'static str {
        "loki"
    }

    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult {
        match self.loki.query_range(id, id_type, self.since, self.pod_pattern.as_deref()).await {
            Ok(lines) => {
                let events = lines.into_iter().map(loki_line_to_event).collect();
                SourceResult::Output(SourceOutput { events, state: vec![] })
            }
            Err(loki_err) => {
                if let Some(ref k8s) = self.k8s_fallback {
                    match k8s.stream_logs(id, id_type, self.since, self.pod_pattern.as_deref()).await {
                        Ok(lines) => {
                            let events = lines.into_iter().map(k8s_line_to_event).collect();
                            let state = vec![StateEntry {
                                source: "loki",
                                key: "fallback".into(),
                                value: "[loki unavailable, using k8s streaming]".into(),
                            }];
                            SourceResult::Output(SourceOutput { events, state })
                        }
                        Err(_) => SourceResult::Unavailable(SourceUnavailable {
                            name: "loki",
                            reason: loki_err.to_string(),
                        }),
                    }
                } else {
                    SourceResult::Unavailable(SourceUnavailable {
                        name: "loki",
                        reason: loki_err.to_string(),
                    })
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct LokiResponse {
    data: LokiData,
}

#[derive(Deserialize)]
struct LokiData {
    result: Vec<LokiStream>,
}

#[derive(Deserialize)]
struct LokiStream {
    stream: HashMap<String, String>,
    values: Vec<(String, String)>,
}

fn loki_label_query(id: &str, id_type: &IdType, pod_pattern: Option<&str>) -> String {
    let label_key = id_type.label_key();
    let mut q = format!("{{{label_key}=\"{id}\"");
    if let Some(pattern) = pod_pattern {
        let rx = pattern.replace('*', ".*");
        q.push_str(&format!(", pod=~\"{rx}\""));
    }
    q.push('}');
    q
}

fn parse_k8s_timestamp_log(line: &str) -> (DateTime<Utc>, &str) {
    if let Some((ts_part, rest)) = line.split_once(' ')
        && let Ok(ts) = ts_part.parse::<DateTime<Utc>>()
    {
        return (ts, rest);
    }
    (Utc::now(), line)
}

pub struct RealLokiClient {
    loki_url: String,
    client: HttpClient,
}

impl RealLokiClient {
    pub fn new(loki_url: String) -> Self {
        let client = HttpClient::builder()
            .build()
            .expect("failed to build reqwest client");
        Self { loki_url: loki_url.trim_end_matches('/').to_string(), client }
    }
}

#[async_trait]
impl LokiClient for RealLokiClient {
    async fn query_range(
        &self,
        id: &str,
        id_type: &IdType,
        since: Duration,
        pod_pattern: Option<&str>,
    ) -> Result<Vec<LokiLogLine>> {
        let query = loki_label_query(id, id_type, pod_pattern);
        let now = Utc::now();
        let start_ns = (now - since)
            .timestamp_nanos_opt()
            .ok_or_else(|| anyhow::anyhow!("start timestamp out of i64 nanosecond range"))?;
        let end_ns = now
            .timestamp_nanos_opt()
            .ok_or_else(|| anyhow::anyhow!("end timestamp out of i64 nanosecond range"))?;

        let resp = self.client
            .get(format!("{}/loki/api/v1/query_range", self.loki_url))
            .query(&[
                ("query", query.as_str()),
                ("start", &start_ns.to_string()),
                ("end", &end_ns.to_string()),
                ("limit", "5000"),
            ])
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("loki request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("loki HTTP error: {e}"))?
            .json::<LokiResponse>()
            .await
            .map_err(|e| anyhow::anyhow!("loki response parse error: {e}"))?;

        let mut lines = Vec::new();
        for stream in resp.data.result {
            let pod = stream.stream.get("pod").cloned();
            let is_serial_console = pod.as_deref().map(|p| p.contains("serial")).unwrap_or(false);
            for (ts_str, message) in stream.values {
                let ts_ns: i64 = ts_str
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid loki timestamp: {ts_str}"))?;
                let secs = ts_ns / 1_000_000_000;
                let nanos = (ts_ns % 1_000_000_000).unsigned_abs() as u32;
                let ts = Utc
                    .timestamp_opt(secs, nanos)
                    .single()
                    .ok_or_else(|| anyhow::anyhow!("loki timestamp out of range: {ts_ns}"))?;
                lines.push(LokiLogLine { ts, message, pod: pod.clone(), is_serial_console });
            }
        }
        Ok(lines)
    }
}

pub struct RealK8sLogStreamClient {
    client: KubeClient,
}

impl RealK8sLogStreamClient {
    pub fn new(client: KubeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl K8sLogStreamClient for RealK8sLogStreamClient {
    async fn stream_logs(
        &self,
        id: &str,
        id_type: &IdType,
        since: Duration,
        pod_pattern: Option<&str>,
    ) -> Result<Vec<K8sLogLine>> {
        let label_selector = format!("{}={id}", id_type.label_key());

        let pods: Api<Pod> = Api::all(self.client.clone());
        let pod_list = pods
            .list(&ListParams::default().labels(&label_selector))
            .await
            .map_err(|e| anyhow::anyhow!("k8s pod list failed: {e}"))?;

        let mut lines = Vec::new();
        for pod in pod_list.items {
            let pod_name = pod.metadata.name.unwrap_or_default();
            let namespace = pod.metadata.namespace.unwrap_or_else(|| "default".into());

            if let Some(pattern) = pod_pattern {
                let clean = pattern.trim_matches('*');
                if !pod_name.contains(clean) {
                    continue;
                }
            }

            let pods_ns: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
            let lp = LogParams {
                since_seconds: Some(since.num_seconds()),
                timestamps: true,
                ..Default::default()
            };
            let log_data = pods_ns.logs(&pod_name, &lp).await.unwrap_or_default();

            for line in log_data.lines() {
                let (ts, message) = parse_k8s_timestamp_log(line);
                lines.push(K8sLogLine { ts, message: message.to_string(), pod: pod_name.clone() });
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};

    struct FakeLokiClient {
        result: Result<Vec<LokiLogLine>>,
    }

    impl FakeLokiClient {
        fn ok(lines: Vec<LokiLogLine>) -> Self {
            Self { result: Ok(lines) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl LokiClient for FakeLokiClient {
        async fn query_range(
            &self,
            _id: &str,
            _id_type: &IdType,
            _since: Duration,
            _pod_pattern: Option<&str>,
        ) -> Result<Vec<LokiLogLine>> {
            match &self.result {
                Ok(lines) => Ok(lines.iter().map(|l| LokiLogLine {
                    ts: l.ts,
                    message: l.message.clone(),
                    pod: l.pod.clone(),
                    is_serial_console: l.is_serial_console,
                }).collect()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    struct FakeK8sLogStreamClient {
        result: Result<Vec<K8sLogLine>>,
    }

    impl FakeK8sLogStreamClient {
        fn ok(lines: Vec<K8sLogLine>) -> Self {
            Self { result: Ok(lines) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl K8sLogStreamClient for FakeK8sLogStreamClient {
        async fn stream_logs(
            &self,
            _id: &str,
            _id_type: &IdType,
            _since: Duration,
            _pod_pattern: Option<&str>,
        ) -> Result<Vec<K8sLogLine>> {
            match &self.result {
                Ok(lines) => Ok(lines.iter().map(|l| K8sLogLine {
                    ts: l.ts,
                    message: l.message.clone(),
                    pod: l.pod.clone(),
                }).collect()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn make_source(loki: impl LokiClient + 'static, k8s: Option<impl K8sLogStreamClient + 'static>) -> LokiSource {
        LokiSource::new(Box::new(loki), k8s.map(|c| Box::new(c) as Box<dyn K8sLogStreamClient>), None, Duration::hours(1))
    }

    #[tokio::test]
    async fn loki_lines_appear_with_loki_source_and_correct_ts() {
        let loki = FakeLokiClient::ok(vec![LokiLogLine {
            ts: ts(1000),
            message: "provisioning started".into(),
            pod: Some("hp-worker-xyz".into()),
            is_serial_console: false,
        }]);
        let source = make_source(loki, None::<FakeK8sLogStreamClient>);
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].source, "loki");
        assert_eq!(output.events[0].ts, ts(1000));
        assert_eq!(output.events[0].kind, "Log");
    }

    #[tokio::test]
    async fn serial_console_lines_appear_in_timeline() {
        let loki = FakeLokiClient::ok(vec![LokiLogLine {
            ts: ts(2000),
            message: "BIOS POST complete".into(),
            pod: Some("serial-console".into()),
            is_serial_console: true,
        }]);
        let source = make_source(loki, None::<FakeK8sLogStreamClient>);
        let output = match source.collect("host-r12u5", &IdType::Host).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].kind, "SerialConsoleLog");
        assert_eq!(output.events[0].source, "loki");
    }

    #[tokio::test]
    async fn loki_unavailable_falls_back_to_k8s_with_annotation() {
        let k8s = FakeK8sLogStreamClient::ok(vec![K8sLogLine {
            ts: ts(3000),
            message: "starting container".into(),
            pod: "hp-worker-xyz".into(),
        }]);
        let source = make_source(FakeLokiClient::err("connection refused"), Some(k8s));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].source, "k8s-logs");
        assert_eq!(output.state.len(), 1);
        assert!(output.state[0].value.contains("[loki unavailable, using k8s streaming]"));
    }

    #[tokio::test]
    async fn both_unavailable_returns_unavailable() {
        let source = make_source(
            FakeLokiClient::err("loki down"),
            Some(FakeK8sLogStreamClient::err("k8s down")),
        );
        match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "loki");
                assert!(u.reason.contains("loki down"));
            }
            _ => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn pod_pattern_forwarded_to_loki_client() {
        struct CaptureLoki {
            captured: Arc<Mutex<Option<String>>>,
        }

        #[async_trait]
        impl LokiClient for CaptureLoki {
            async fn query_range(
                &self,
                _id: &str,
                _id_type: &IdType,
                _since: Duration,
                pod_pattern: Option<&str>,
            ) -> Result<Vec<LokiLogLine>> {
                *self.captured.lock().unwrap() = pod_pattern.map(str::to_string);
                Ok(vec![])
            }
        }

        let captured = Arc::new(Mutex::new(None));
        let source = LokiSource::new(
            Box::new(CaptureLoki { captured: captured.clone() }),
            Some(Box::new(FakeK8sLogStreamClient::ok(vec![]))),
            Some("hp-worker-*".into()),
            Duration::hours(1),
        );
        source.collect("hp-abc", &IdType::Workflow).await;
        assert_eq!(*captured.lock().unwrap(), Some("hp-worker-*".to_string()));
    }

    #[tokio::test]
    async fn source_is_independently_optional() {
        let source = make_source(
            FakeLokiClient::err("loki unavailable"),
            Some(FakeK8sLogStreamClient::err("k8s unavailable")),
        );
        match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Unavailable(u) => assert_eq!(u.name, "loki"),
            SourceResult::Output(_) => panic!("expected Unavailable, not Output"),
        }
    }

    #[tokio::test]
    async fn loki_error_message_severity_is_info() {
        let loki = FakeLokiClient::ok(vec![LokiLogLine {
            ts: ts(5000),
            message: "error: connection refused".into(),
            pod: Some("hp-worker".into()),
            is_serial_console: false,
        }]);
        let source = make_source(loki, None::<FakeK8sLogStreamClient>);
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Info);
        assert_eq!(output.events[0].kind, "Log");
    }

    #[tokio::test]
    async fn k8s_log_message_has_pod_name_prefix() {
        let k8s = FakeK8sLogStreamClient::ok(vec![K8sLogLine {
            ts: ts(6000),
            message: "init container finished".into(),
            pod: "hp-worker-xyz".into(),
        }]);
        let source = make_source(FakeLokiClient::err("loki down"), Some(k8s));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].source, "k8s-logs");
        assert_eq!(output.events[0].kind, "Log");
        assert_eq!(output.events[0].message, "[hp-worker-xyz] init container finished");
    }

    #[tokio::test]
    async fn loki_empty_ok_produces_empty_output_no_k8s_fallback() {
        struct CheckK8sNotCalled;

        #[async_trait]
        impl K8sLogStreamClient for CheckK8sNotCalled {
            async fn stream_logs(
                &self,
                _id: &str,
                _id_type: &IdType,
                _since: Duration,
                _pod_pattern: Option<&str>,
            ) -> Result<Vec<K8sLogLine>> {
                panic!("k8s should not be called when loki returns Ok");
            }
        }

        let source = LokiSource::new(
            Box::new(FakeLokiClient::ok(vec![])),
            Some(Box::new(CheckK8sNotCalled)),
            None,
            Duration::hours(1),
        );
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
        assert!(output.state.is_empty());
    }

    #[tokio::test]
    async fn loki_unavailable_with_no_k8s_configured_returns_unavailable() {
        let source = LokiSource::new(
            Box::new(FakeLokiClient::err("loki down")),
            None::<Box<dyn K8sLogStreamClient>>,
            None,
            Duration::hours(1),
        );
        match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "loki");
                assert!(u.reason.contains("loki down"));
            }
            SourceResult::Output(_) => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn pod_pattern_forwarded_to_k8s_stream_logs() {
        struct CaptureK8s {
            captured: Arc<Mutex<Option<String>>>,
        }

        #[async_trait]
        impl K8sLogStreamClient for CaptureK8s {
            async fn stream_logs(
                &self,
                _id: &str,
                _id_type: &IdType,
                _since: Duration,
                pod_pattern: Option<&str>,
            ) -> Result<Vec<K8sLogLine>> {
                *self.captured.lock().unwrap() = pod_pattern.map(str::to_string);
                Ok(vec![])
            }
        }

        let captured = Arc::new(Mutex::new(None));
        let source = LokiSource::new(
            Box::new(FakeLokiClient::err("loki down")),
            Some(Box::new(CaptureK8s { captured: captured.clone() })),
            Some("hp-worker-*".into()),
            Duration::hours(1),
        );
        source.collect("hp-abc", &IdType::Workflow).await;
        assert_eq!(*captured.lock().unwrap(), Some("hp-worker-*".to_string()));
    }
}
