use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceOutput, SourceUnavailable, StateEntry};

#[derive(Clone)]
pub struct PgRow {
    pub table: String,
    pub columns: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct PgAuditEvent {
    pub ts: DateTime<Utc>,
    pub action: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct PgEntityData {
    pub rows: Vec<PgRow>,
    pub audit_events: Vec<PgAuditEvent>,
}

#[async_trait]
pub trait PostgresClient: Send + Sync {
    async fn query_entity(&self, id: &str, id_type: &IdType) -> Result<PgEntityData>;
}

pub struct PostgresSource {
    client: Box<dyn PostgresClient>,
}

impl PostgresSource {
    pub fn new(client: Box<dyn PostgresClient>) -> Self {
        Self { client }
    }
}

fn audit_event_to_event(e: PgAuditEvent) -> Event {
    let severity = Severity::classify("postgres", &e.action, &e.detail);
    let message = if e.detail.is_empty() { e.action.clone() } else { e.detail };
    Event {
        ts: e.ts,
        source: "postgres".into(),
        kind: e.action,
        message,
        severity,
        tags: Default::default(),
    }
}

#[async_trait]
impl Source for PostgresSource {
    fn name(&self) -> &'static str {
        "postgres"
    }

    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult {
        match self.client.query_entity(id, id_type).await {
            Ok(data) => {
                let events = data.audit_events.into_iter().map(audit_event_to_event).collect();
                let state = data.rows.into_iter().flat_map(|row| {
                    let table = row.table;
                    row.columns.into_iter().map(move |(k, v)| StateEntry {
                        source: "postgres",
                        key: format!("{}.{}", table, k),
                        value: v,
                    })
                }).collect();
                SourceResult::Output(SourceOutput { events, state })
            }
            Err(e) => SourceResult::Unavailable(SourceUnavailable {
                name: "postgres",
                reason: e.to_string(),
            }),
        }
    }
}

pub struct SqlxPostgresClient {
    pool: sqlx::PgPool,
}

impl SqlxPostgresClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }
}

fn col_as_string(row: &sqlx::postgres::PgRow, i: usize) -> String {
    use sqlx::Row;
    macro_rules! try_as {
        ($t:ty) => {
            if let Ok(v) = row.try_get::<Option<$t>, _>(i) {
                return v.map(|x| x.to_string()).unwrap_or_default();
            }
        };
    }
    try_as!(String);
    try_as!(i64);
    try_as!(i32);
    try_as!(i16);
    try_as!(bool);
    try_as!(f64);
    try_as!(f32);
    if let Ok(v) = row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(i) {
        return v.map(|t| t.to_rfc3339()).unwrap_or_default();
    }
    String::new()
}

fn sqlx_row_to_pg_row(row: &sqlx::postgres::PgRow, table: &str) -> PgRow {
    use sqlx::{Column, Row};
    let columns = row
        .columns()
        .iter()
        .map(|col| (col.name().to_string(), col_as_string(row, col.ordinal())))
        .collect();
    PgRow { table: table.to_string(), columns }
}

fn is_undefined_table(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = e {
        db.code().as_deref() == Some("42P01")
    } else {
        false
    }
}

async fn fetch_table_rows(pool: &sqlx::PgPool, table: &str, id_col: &str, id: &str) -> Result<Vec<PgRow>> {
    // table and id_col are always internal constants, not user input.
    let sql = format!("SELECT * FROM {table} WHERE {id_col} = $1 LIMIT 20");
    match sqlx::query(&sql).bind(id).fetch_all(pool).await {
        Ok(rows) => Ok(rows.iter().map(|r| sqlx_row_to_pg_row(r, table)).collect()),
        Err(ref e) if is_undefined_table(e) => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

async fn fetch_audit_events(pool: &sqlx::PgPool, entity_id: &str) -> Result<Vec<PgAuditEvent>> {
    let sql = "SELECT ts, action, detail FROM audit_log \
               WHERE entity_id = $1 ORDER BY ts DESC LIMIT 100";
    match sqlx::query(sql).bind(entity_id).fetch_all(pool).await {
        Ok(rows) => {
            use sqlx::Row;
            Ok(rows
                .iter()
                .map(|r| PgAuditEvent {
                    ts: r.try_get("ts").unwrap_or_else(|_| chrono::Utc::now()),
                    action: r.try_get("action").unwrap_or_default(),
                    detail: r.try_get("detail").unwrap_or_default(),
                })
                .collect())
        }
        Err(ref e) if is_undefined_table(e) => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

#[async_trait]
impl PostgresClient for SqlxPostgresClient {
    async fn query_entity(&self, id: &str, id_type: &IdType) -> Result<PgEntityData> {
        let mut rows = Vec::new();
        match id_type {
            IdType::Host => rows.extend(fetch_table_rows(&self.pool, "hosts", "id", id).await?),
            IdType::Dpu => rows.extend(fetch_table_rows(&self.pool, "hosts", "dpu_id", id).await?),
            IdType::Workflow => rows.extend(fetch_table_rows(&self.pool, "workflows", "id", id).await?),
            IdType::Request => {}
        }
        let audit_events = fetch_audit_events(&self.pool, id).await?;
        Ok(PgEntityData { rows, audit_events })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct FakePostgresClient {
        result: Result<PgEntityData>,
    }

    impl FakePostgresClient {
        fn ok(data: PgEntityData) -> Self {
            Self { result: Ok(data) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl PostgresClient for FakePostgresClient {
        async fn query_entity(&self, _id: &str, _id_type: &IdType) -> Result<PgEntityData> {
            match &self.result {
                Ok(data) => Ok(data.clone()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[tokio::test]
    async fn host_row_becomes_state_entries() {
        let data = PgEntityData {
            rows: vec![PgRow {
                table: "hosts".into(),
                columns: vec![
                    ("id".into(), "host-r12u5".into()),
                    ("status".into(), "ready".into()),
                ],
            }],
            audit_events: vec![],
        };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let result = source.collect("host-r12u5", &IdType::Host).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state.len(), 2);
        assert_eq!(output.state[0].key, "hosts.id");
        assert_eq!(output.state[0].value, "host-r12u5");
        assert_eq!(output.state[1].key, "hosts.status");
        assert_eq!(output.state[1].value, "ready");
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn audit_log_failure_maps_to_warning_event() {
        let data = PgEntityData {
            rows: vec![],
            audit_events: vec![PgAuditEvent {
                ts: ts(1000),
                action: "provision_fail".into(),
                detail: "timeout".into(),
            }],
        };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].severity, Severity::Warning);
        assert_eq!(output.events[0].source, "postgres");
        assert_eq!(output.events[0].kind, "provision_fail");
        assert_eq!(output.events[0].message, "timeout");
    }

    #[tokio::test]
    async fn audit_log_success_maps_to_info_event() {
        let data = PgEntityData {
            rows: vec![],
            audit_events: vec![PgAuditEvent {
                ts: ts(1000),
                action: "create_host".into(),
                detail: "".into(),
            }],
        };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events[0].severity, Severity::Info);
        assert_eq!(output.events[0].message, "create_host");
    }

    #[tokio::test]
    async fn unavailable_client_returns_unavailable() {
        let source = PostgresSource::new(Box::new(FakePostgresClient::err("connection refused")));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        match result {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "postgres");
                assert!(u.reason.contains("connection refused"));
            }
            _ => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn workflow_rows_become_workflow_state_entries() {
        let data = PgEntityData {
            rows: vec![PgRow {
                table: "workflows".into(),
                columns: vec![
                    ("id".into(), "hp-abc".into()),
                    ("status".into(), "running".into()),
                ],
            }],
            audit_events: vec![],
        };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state.len(), 2);
        assert_eq!(output.state[0].key, "workflows.id");
        assert_eq!(output.state[0].value, "hp-abc");
        assert_eq!(output.state[1].key, "workflows.status");
        assert_eq!(output.state[1].value, "running");
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn empty_entity_data_produces_empty_output() {
        let data = PgEntityData { rows: vec![], audit_events: vec![] };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let output = match source.collect("hp-abc", &IdType::Workflow).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert!(output.events.is_empty());
        assert!(output.state.is_empty());
    }

    #[tokio::test]
    async fn multiple_rows_produce_all_state_entries() {
        let data = PgEntityData {
            rows: vec![
                PgRow {
                    table: "hosts".into(),
                    columns: vec![("id".into(), "host-r12u5".into()), ("rack".into(), "r12".into())],
                },
                PgRow {
                    table: "hosts".into(),
                    columns: vec![("id".into(), "host-r13u5".into()), ("rack".into(), "r13".into())],
                },
            ],
            audit_events: vec![],
        };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let output = match source.collect("host-r12u5", &IdType::Host).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state.len(), 4);
        assert!(output.state.iter().all(|s| s.key.starts_with("hosts.")));
    }

    #[tokio::test]
    async fn multiple_audit_events_become_multiple_events() {
        let data = PgEntityData {
            rows: vec![],
            audit_events: vec![
                PgAuditEvent { ts: ts(1000), action: "create_host".into(), detail: "".into() },
                PgAuditEvent { ts: ts(2000), action: "provision_fail".into(), detail: "timeout".into() },
                PgAuditEvent { ts: ts(3000), action: "delete_host".into(), detail: "decommissioned".into() },
            ],
        };
        let source = PostgresSource::new(Box::new(FakePostgresClient::ok(data)));
        let output = match source.collect("host-r12u5", &IdType::Host).await {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 3);
        assert_eq!(output.events[0].severity, Severity::Info);
        assert_eq!(output.events[1].severity, Severity::Warning);
        assert_eq!(output.events[2].severity, Severity::Warning);
        assert!(output.state.is_empty());
    }

    #[tokio::test]
    async fn smoke_real_postgres_skips_when_url_unset() {
        let url = match std::env::var("NICO_POSTGRES_URL") {
            Ok(u) => u,
            Err(_) => return,
        };
        let client = SqlxPostgresClient::connect(&url)
            .await
            .expect("connect with NICO_POSTGRES_URL");
        // Does not panic; Ok or Err both accepted (schema may differ per environment).
        let _ = client.query_entity("host-r12u5", &IdType::Host).await;
        let _ = client.query_entity("dpu-bf3-r12u5", &IdType::Dpu).await;
    }
}
