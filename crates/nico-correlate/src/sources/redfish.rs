use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceOutput, SourceUnavailable, StateEntry};

/// Current system state read from a BMC Redfish endpoint (GET-only, ADR-002).
#[derive(Clone)]
pub struct RedfishSystemState {
    /// Resolved host ID — differs from the entity ID when the entity is a DPU.
    pub host_id: String,
    pub power_state: String,
    pub boot_source: String,
    pub health: String,
}

#[derive(Clone)]
pub struct RedfishRawEvent {
    pub ts: DateTime<Utc>,
    pub event_type: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct RedfishData {
    pub system_state: RedfishSystemState,
    pub events: Vec<RedfishRawEvent>,
}

/// All Redfish calls are read-only GETs (ADR-002).
/// For DPU entities the client resolves the associated host via Postgres
/// `hosts.dpu_id` and queries that host's BMC address.
#[async_trait]
pub trait RedfishClient: Send + Sync {
    async fn query(&self, id: &str, id_type: &IdType) -> Result<RedfishData>;
}

pub struct RedfishSource {
    client: Box<dyn RedfishClient>,
}

impl RedfishSource {
    pub fn new(client: Box<dyn RedfishClient>) -> Self {
        Self { client }
    }
}

fn map_event(raw: RedfishRawEvent) -> Event {
    let severity = if raw.event_type.contains("Fault")
        || raw.event_type.contains("Critical")
        || raw.event_type.contains("Failed")
    {
        Severity::Error
    } else if raw.event_type.contains("Warning") || raw.event_type.contains("Degraded") {
        Severity::Warning
    } else {
        Severity::Info
    };
    Event {
        ts: raw.ts,
        source: "redfish".into(),
        kind: raw.event_type.clone(),
        message: if raw.detail.is_empty() { raw.event_type } else { raw.detail },
        severity,
        tags: Default::default(),
    }
}

#[async_trait]
impl Source for RedfishSource {
    fn name(&self) -> &'static str {
        "redfish"
    }

    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult {
        if !matches!(id_type, IdType::Host | IdType::Dpu) {
            return SourceResult::Output(SourceOutput { events: vec![], state: vec![] });
        }

        match self.client.query(id, id_type).await {
            Ok(data) => {
                let events = data.events.into_iter().map(map_event).collect();
                let host = &data.system_state.host_id;
                let state = vec![
                    StateEntry { source: "redfish", key: format!("{host}.power_state"), value: data.system_state.power_state },
                    StateEntry { source: "redfish", key: format!("{host}.boot_source"), value: data.system_state.boot_source },
                    StateEntry { source: "redfish", key: format!("{host}.health"),      value: data.system_state.health },
                ];
                SourceResult::Output(SourceOutput { events, state })
            }
            Err(e) => SourceResult::Unavailable(SourceUnavailable {
                name: "redfish",
                reason: e.to_string(),
            }),
        }
    }
}

// Private serde structs for Redfish JSON responses.

#[derive(Deserialize)]
struct RedfishSystemResp {
    #[serde(rename = "PowerState")]
    power_state: Option<String>,
    #[serde(rename = "Boot")]
    boot: Option<BootResp>,
    #[serde(rename = "Status")]
    status: Option<StatusResp>,
}

#[derive(Deserialize)]
struct BootResp {
    #[serde(rename = "BootSourceOverrideTarget")]
    boot_source_override_target: Option<String>,
}

#[derive(Deserialize)]
struct StatusResp {
    #[serde(rename = "Health")]
    health: Option<String>,
}

#[derive(Deserialize)]
struct LogEntriesResp {
    #[serde(rename = "Members")]
    members: Option<Vec<LogEntryResp>>,
}

#[derive(Deserialize)]
struct LogEntryResp {
    #[serde(rename = "Created")]
    created: Option<String>,
    /// Structured ID like "Drive.1.0.DriveFault" — last segment used as event type name.
    #[serde(rename = "MessageId")]
    message_id: Option<String>,
    #[serde(rename = "Severity")]
    severity: Option<String>,
    #[serde(rename = "Message")]
    message: Option<String>,
}

fn parse_log_entry(entry: LogEntryResp) -> Option<RedfishRawEvent> {
    let event_type = entry.message_id
        .as_deref()
        .and_then(|mid| mid.rsplit('.').next())
        .map(|s| s.to_string())
        .or(entry.severity)
        .unwrap_or_else(|| "Event".into());
    let detail = entry.message.unwrap_or_default();
    let ts = entry.created
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);
    Some(RedfishRawEvent { ts, event_type, detail })
}

/// Real Redfish client. Queries the BMC via read-only GETs (ADR-002).
/// `bmc_base_url` may contain `{host}` which is replaced with the resolved host ID.
/// For DPU entities, the host ID is resolved via `SELECT id FROM hosts WHERE dpu_id = $1`.
/// Set `REDFISH_SKIP_TLS_VERIFY=1` for BMCs with self-signed certificates.
pub struct RealRedfishClient {
    http: HttpClient,
    bmc_base_url: String,
    pg_pool: Option<sqlx::PgPool>,
}

impl RealRedfishClient {
    pub fn new(bmc_base_url: String, pg_pool: Option<sqlx::PgPool>) -> Self {
        let skip_tls = std::env::var("REDFISH_SKIP_TLS_VERIFY").as_deref() == Ok("1");
        let http = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .danger_accept_invalid_certs(skip_tls)
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            bmc_base_url: bmc_base_url.trim_end_matches('/').to_string(),
            pg_pool,
        }
    }

    fn bmc_url_for_host(&self, host_id: &str) -> String {
        self.bmc_base_url.replace("{host}", host_id)
    }

    async fn resolve_host_id(&self, id: &str, id_type: &IdType) -> Result<String> {
        if !matches!(id_type, IdType::Dpu) {
            return Ok(id.to_string());
        }
        let pool = self.pg_pool.as_ref()
            .ok_or_else(|| anyhow::anyhow!("DPU resolution requires Postgres — set NICO_POSTGRES_URL"))?;
        let host_id: String = sqlx::query_scalar("SELECT id FROM hosts WHERE dpu_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| anyhow::anyhow!("DPU host lookup failed for {id}: {e}"))?;
        Ok(host_id)
    }
}

#[async_trait]
impl RedfishClient for RealRedfishClient {
    async fn query(&self, id: &str, id_type: &IdType) -> Result<RedfishData> {
        let host_id = self.resolve_host_id(id, id_type).await?;
        let bmc_url = self.bmc_url_for_host(&host_id);

        let system: RedfishSystemResp = self.http
            .get(format!("{bmc_url}/redfish/v1/Systems/1"))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("BMC unreachable at {bmc_url}: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("BMC returned error: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("BMC response parse error: {e}"))?;

        // Event log is optional — degrade to empty events rather than failing the whole query.
        let events: Vec<RedfishRawEvent> = match self.http
            .get(format!("{bmc_url}/redfish/v1/Systems/1/LogServices/Log1/Entries"))
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => resp
                .json::<LogEntriesResp>()
                .await
                .unwrap_or(LogEntriesResp { members: None })
                .members
                .unwrap_or_default()
                .into_iter()
                .filter_map(parse_log_entry)
                .collect(),
            Err(_) => vec![],
        };

        Ok(RedfishData {
            system_state: RedfishSystemState {
                host_id,
                power_state: system.power_state.unwrap_or_else(|| "Unknown".into()),
                boot_source: system.boot
                    .and_then(|b| b.boot_source_override_target)
                    .unwrap_or_else(|| "Unknown".into()),
                health: system.status
                    .and_then(|s| s.health)
                    .unwrap_or_else(|| "Unknown".into()),
            },
            events,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct FakeRedfishClient {
        result: Result<RedfishData>,
    }

    impl FakeRedfishClient {
        fn ok(data: RedfishData) -> Self {
            Self { result: Ok(data) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl RedfishClient for FakeRedfishClient {
        async fn query(&self, _id: &str, _id_type: &IdType) -> Result<RedfishData> {
            match &self.result {
                Ok(d) => Ok(d.clone()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn host_data(host_id: &str) -> RedfishData {
        RedfishData {
            system_state: RedfishSystemState {
                host_id: host_id.into(),
                power_state: "On".into(),
                boot_source: "Hdd".into(),
                health: "OK".into(),
            },
            events: vec![],
        }
    }

    #[tokio::test]
    async fn host_power_state_becomes_state_entries() {
        let source = RedfishSource::new(Box::new(FakeRedfishClient::ok(host_data("host-r12u5"))));
        let result = source.collect("host-r12u5", &IdType::Host).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state.len(), 3);
        assert_eq!(output.state[0].key, "host-r12u5.power_state");
        assert_eq!(output.state[0].value, "On");
        assert_eq!(output.state[1].key, "host-r12u5.boot_source");
        assert_eq!(output.state[2].key, "host-r12u5.health");
        assert_eq!(output.state[2].value, "OK");
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn dpu_resolves_to_host_state_entries() {
        // When entity is a DPU the client resolves the host; state keys carry the host ID.
        let source = RedfishSource::new(Box::new(FakeRedfishClient::ok(host_data("host-r12u5"))));
        let result = source.collect("dpu-bf3-r12u5", &IdType::Dpu).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state[0].key, "host-r12u5.power_state");
    }

    #[tokio::test]
    async fn fault_event_maps_to_error_severity() {
        let data = RedfishData {
            system_state: RedfishSystemState {
                host_id: "host-r12u5".into(),
                power_state: "On".into(),
                boot_source: "Hdd".into(),
                health: "Critical".into(),
            },
            events: vec![RedfishRawEvent {
                ts: ts(1000),
                event_type: "DriveFault".into(),
                detail: "NVMe slot 2".into(),
            }],
        };
        let source = RedfishSource::new(Box::new(FakeRedfishClient::ok(data)));
        let result = source.collect("host-r12u5", &IdType::Host).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Error);
        assert_eq!(output.events[0].source, "redfish");
        assert_eq!(output.events[0].kind, "DriveFault");
        assert_eq!(output.events[0].message, "NVMe slot 2");
    }

    #[tokio::test]
    async fn power_on_event_maps_to_info_severity() {
        let data = RedfishData {
            system_state: RedfishSystemState {
                host_id: "host-r12u5".into(),
                power_state: "On".into(),
                boot_source: "Hdd".into(),
                health: "OK".into(),
            },
            events: vec![RedfishRawEvent {
                ts: ts(2000),
                event_type: "SystemPowerOn".into(),
                detail: "".into(),
            }],
        };
        let source = RedfishSource::new(Box::new(FakeRedfishClient::ok(data)));
        let result = source.collect("host-r12u5", &IdType::Host).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Info);
        assert_eq!(output.events[0].message, "SystemPowerOn");
    }

    #[tokio::test]
    async fn non_host_dpu_type_returns_empty_output() {
        let source = RedfishSource::new(Box::new(FakeRedfishClient::ok(host_data("host-r12u5"))));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
        assert!(output.state.is_empty());
    }

    #[tokio::test]
    async fn unreachable_bmc_returns_unavailable() {
        let source = RedfishSource::new(Box::new(FakeRedfishClient::err("connection refused")));
        let result = source.collect("host-r12u5", &IdType::Host).await;
        match result {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "redfish");
                assert!(u.reason.contains("connection refused"));
            }
            _ => panic!("expected Unavailable"),
        }
    }
}
