use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use async_trait::async_trait;
use anyhow::Result;
use reqwest::Client as HttpClient;
use serde::Deserialize;

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
