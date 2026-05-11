use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use async_trait::async_trait;
use anyhow::{anyhow, Result};
use reqwest::Client as HttpClient;
use serde::Deserialize;

use crate::log_source::{LogCollection, LogSource, PodLogsCache};

pub struct LokiLine {
    pub pod: String,
    pub text: String,
}

pub enum LokiQueryResult {
    Lines(Vec<LokiLine>),
    Unreachable,
}

#[async_trait]
pub trait LokiClient: Send + Sync {
    /// Query Loki for lines matching error|panic|fatal in the given namespace.
    /// Returns Unreachable when Loki is not reachable; caller falls back to k8s streaming.
    async fn query_errors(&self, namespace: &str, since: Duration, limit: usize) -> Result<LokiQueryResult>;
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

pub struct RealLokiClient {
    url: String,
    client: HttpClient,
}

impl RealLokiClient {
    pub fn new(url: String) -> Self {
        let client = HttpClient::builder()
            .build()
            .expect("failed to build reqwest client");
        Self { url: url.trim_end_matches('/').to_string(), client }
    }
}

#[async_trait]
impl LokiClient for RealLokiClient {
    async fn query_errors(&self, namespace: &str, since: Duration, limit: usize) -> Result<LokiQueryResult> {
        let query = format!(r#"{{namespace="{namespace}"}} |~ "(?i)(error|panic|fatal)""#);

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        let start_ns = SystemTime::now()
            .checked_sub(since)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let resp = match self.client
            .get(format!("{}/loki/api/v1/query_range", self.url))
            .query(&[
                ("query", query.as_str()),
                ("start", &start_ns.to_string()),
                ("end", &now_ns.to_string()),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(LokiQueryResult::Unreachable),
        };

        let resp = match resp.error_for_status() {
            Ok(r) => r,
            Err(_) => return Ok(LokiQueryResult::Unreachable),
        };

        let loki_resp: LokiResponse = match resp.json().await {
            Ok(r) => r,
            Err(_) => return Ok(LokiQueryResult::Unreachable),
        };

        let mut lines = Vec::new();
        for stream in loki_resp.data.result {
            let pod = stream.stream.get("pod").cloned().unwrap_or_default();
            for (_ts, text) in stream.values {
                lines.push(LokiLine { pod: pod.clone(), text });
            }
        }

        Ok(LokiQueryResult::Lines(lines))
    }
}

/// Test fakes — parallel to `nico_common::k8s::testing::MockK8sClient`
/// and `nico_common::temporal::testing::MockTemporalClient`. Tests
/// configure the return value once at construction; calls return clones
/// (or report `Unreachable`) without any HTTP.
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// In-memory fake. Defaults to `Unreachable` so the
    /// `best_effort_chain` falls through to the next source unless a
    /// test pre-loads lines or an error explicitly.
    pub struct MockLokiClient {
        result: Mutex<MockState>,
    }

    enum MockState {
        Unreachable,
        Lines(Vec<(String, String)>),
        Err(String),
    }

    impl Default for MockLokiClient {
        fn default() -> Self {
            Self {
                result: Mutex::new(MockState::Unreachable),
            }
        }
    }

    impl MockLokiClient {
        pub fn new() -> Self {
            Self::default()
        }

        /// Pre-load `(pod, line)` rows the next `query_errors` call
        /// should return as `LokiQueryResult::Lines`.
        pub fn with_lines(self, lines: Vec<(String, String)>) -> Self {
            *self.result.lock().unwrap() = MockState::Lines(lines);
            self
        }

        /// Force `query_errors` to return an `Err` (network panic etc.),
        /// distinct from the soft `Unreachable` signal.
        pub fn with_err(self, msg: impl Into<String>) -> Self {
            *self.result.lock().unwrap() = MockState::Err(msg.into());
            self
        }
    }

    #[async_trait]
    impl LokiClient for MockLokiClient {
        async fn query_errors(
            &self,
            _namespace: &str,
            _since: Duration,
            _limit: usize,
        ) -> Result<LokiQueryResult> {
            let guard = self.result.lock().unwrap();
            match &*guard {
                MockState::Unreachable => Ok(LokiQueryResult::Unreachable),
                MockState::Lines(rows) => Ok(LokiQueryResult::Lines(
                    rows.iter()
                        .map(|(p, t)| LokiLine {
                            pod: p.clone(),
                            text: t.clone(),
                        })
                        .collect(),
                )),
                MockState::Err(msg) => Err(anyhow!("{msg}")),
            }
        }
    }
}

pub struct LokiLogSource {
    client: Arc<dyn LokiClient>,
}

impl LokiLogSource {
    pub fn new(client: Arc<dyn LokiClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl LogSource for LokiLogSource {
    fn name(&self) -> &str { "loki" }

    async fn collect(
        &self,
        namespace: &str,
        since: Duration,
        limit: usize,
        _prefetched: &PodLogsCache,
    ) -> Result<LogCollection> {
        match self.client.query_errors(namespace, since, limit).await? {
            LokiQueryResult::Lines(lines) => Ok(LogCollection {
                label: "loki".to_string(),
                primary_ok: true,
                entries: lines.into_iter().map(|l| (l.pod, l.text)).collect(),
            }),
            LokiQueryResult::Unreachable => Err(anyhow!("loki unreachable")),
        }
    }
}
