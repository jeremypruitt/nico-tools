use std::sync::Arc;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::bootstrap::LayerInputs;
use crate::postgres::{LockWait, PoolStats, PostgresClient, SqlxPostgresClient};
use crate::layer::{self, Check, CheckKind, Layer, LayerOutcome, RunOpts};

pub const NAME: &str = "postgres";

/// Factory consumed by `bootstrap::prepare_layers`.
pub fn register(inputs: &LayerInputs) -> Box<dyn Layer> {
    match SqlxPostgresClient::new(&inputs.postgres_url) {
        Ok(pg) => Box::new(PostgresLayer::new(Arc::new(pg))),
        Err(e) => {
            eprintln!("warning: postgres URL invalid: {e}");
            eprintln!(
                "  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url"
            );
            layer::UnconfiguredLayer::new(NAME, "invalid postgres URL")
        }
    }
}

const POOL_WARN_RATIO: f64 = 0.90;
const LOCK_WARN_SECS: f64 = 5.0;

pub struct PostgresLayer {
    client: Arc<dyn PostgresClient>,
}

impl PostgresLayer {
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self { client }
    }
}

fn pool_check(stats: &PoolStats) -> Check {
    let ratio = if stats.max_conn > 0 {
        stats.active as f64 / stats.max_conn as f64
    } else {
        0.0
    };
    let status = if ratio >= POOL_WARN_RATIO { Status::Warn } else { Status::Ok };
    Check {
        name: "pool",
        status: status.clone(),
        value: format!("pool {}/{} in-use", stats.active, stats.max_conn),
        next_command: if status == Status::Warn {
            Some(
                "SELECT * FROM pg_stat_activity WHERE state != 'idle' ORDER BY query_start".into(),
            )
        } else {
            None
        },
        kind: CheckKind::Headline,
    }
}

fn lock_checks(waits: &[LockWait]) -> Vec<Check> {
    let long_waits: Vec<_> = waits.iter().filter(|w| w.wait_secs >= LOCK_WARN_SECS).collect();
    let mut checks = vec![Check {
        name: "locks",
        status: if long_waits.is_empty() { Status::Ok } else { Status::Warn },
        value: format!("{} lock waits", long_waits.len()),
        next_command: None,
        kind: CheckKind::Headline,
    }];
    for w in &long_waits {
        let pids: Vec<String> = std::iter::once(w.waiting_pid)
            .chain(w.blocking_pid)
            .map(|p| p.to_string())
            .collect();
        checks.push(Check {
            name: "lock_wait",
            status: Status::Warn,
            value: format!(
                "pid {} waiting {:.0}s on {} (blocked by pid {})",
                w.waiting_pid,
                w.wait_secs,
                w.relation.as_deref().unwrap_or("unknown"),
                w.blocking_pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".into()),
            ),
            next_command: Some(format!(
                "SELECT * FROM pg_stat_activity WHERE pid IN ({})",
                pids.join(", ")
            )),
            kind: CheckKind::Headline,
        });
    }
    checks
}

#[async_trait]
impl Layer for PostgresLayer {
    fn name(&self) -> &'static str { "postgres" }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        let stats = match self.client.pool_stats().await {
            Ok(s) => s,
            Err(_) => {
                return LayerOutcome::Checks(vec![Check {
                    name: "pool",
                    status: Status::Unknown,
                    value: "postgres unreachable".into(),
                    next_command: Some("kubectl get svc -n nico | grep postgres".into()),
                    kind: CheckKind::Headline,
                }]);
            }
        };

        let mut checks = vec![pool_check(&stats)];

        match self.client.lock_waits().await {
            Ok(waits) => checks.extend(lock_checks(&waits)),
            Err(_) => {
                checks.push(Check {
                    name: "locks",
                    status: Status::Unknown,
                    value: "lock query failed".into(),
                    next_command: Some("kubectl get svc -n nico | grep postgres".into()),
                    kind: CheckKind::Headline,
                });
            }
        }

        LayerOutcome::Checks(checks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use crate::postgres::{LockWait, PoolStats};

    // --- sync unit tests for pure functions ---

    #[test]
    fn pool_check_healthy_pool_is_ok() {
        let check = pool_check(&PoolStats { active: 10, max_conn: 20 });
        assert_eq!(check.status, Status::Ok);
        assert!(check.next_command.is_none());
    }

    #[test]
    fn pool_check_at_90_pct_is_warn_with_hint() {
        let check = pool_check(&PoolStats { active: 18, max_conn: 20 });
        assert_eq!(check.status, Status::Warn);
        assert!(check.next_command.as_deref().unwrap().contains("pg_stat_activity"));
    }

    #[test]
    fn lock_checks_no_waits_is_ok() {
        let checks = lock_checks(&[]);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "locks");
        assert_eq!(checks[0].status, Status::Ok);
    }

    #[test]
    fn lock_checks_long_wait_is_warn_with_lock_wait_entry() {
        let waits = vec![LockWait {
            waiting_pid: 100,
            blocking_pid: Some(200),
            relation: Some("orders".into()),
            wait_secs: 10.0,
        }];
        let checks = lock_checks(&waits);
        assert_eq!(checks[0].status, Status::Warn);
        let lw = checks.iter().find(|c| c.name == "lock_wait").unwrap();
        assert_eq!(lw.status, Status::Warn);
    }

    struct MockPostgresClient {
        pool: std::result::Result<PoolStats, String>,
        waits: std::result::Result<Vec<LockWait>, String>,
    }

    #[async_trait]
    impl PostgresClient for MockPostgresClient {
        async fn pool_stats(&self) -> Result<PoolStats> {
            match &self.pool {
                Ok(s) => Ok(PoolStats { active: s.active, max_conn: s.max_conn }),
                Err(e) => Err(anyhow::anyhow!("{}", e)),
            }
        }
        async fn lock_waits(&self) -> Result<Vec<LockWait>> {
            match &self.waits {
                Ok(ws) => Ok(ws.iter().map(|w| LockWait {
                    waiting_pid: w.waiting_pid,
                    blocking_pid: w.blocking_pid,
                    relation: w.relation.clone(),
                    wait_secs: w.wait_secs,
                }).collect()),
                Err(e) => Err(anyhow::anyhow!("{}", e)),
            }
        }
    }

    fn opts() -> RunOpts {
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
            ..Default::default()
        }
    }

    fn layer(pool: std::result::Result<PoolStats, &str>, waits: std::result::Result<Vec<LockWait>, &str>) -> PostgresLayer {
        PostgresLayer::new(Arc::new(MockPostgresClient {
            pool: pool.map_err(|e| e.to_string()),
            waits: waits.map_err(|e| e.to_string()),
        }))
    }

    fn pool_ok(active: i64, max_conn: i64) -> std::result::Result<PoolStats, &'static str> {
        Ok(PoolStats { active, max_conn })
    }

    #[tokio::test]
    async fn healthy_pool_no_waits_reports_ok() {
        let result = layer(pool_ok(10, 20), Ok(vec![])).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        let pool_check = result.checks.iter().find(|c| c.name == "pool").unwrap();
        assert_eq!(pool_check.status, Status::Ok);
        assert_eq!(pool_check.value, "pool 10/20 in-use");
        assert!(pool_check.next_command.is_none());
        let lock_check = result.checks.iter().find(|c| c.name == "locks").unwrap();
        assert_eq!(lock_check.value, "0 lock waits");
    }

    #[tokio::test]
    async fn pool_at_90_pct_is_warn_with_pg_stat_activity_hint() {
        let result = layer(pool_ok(18, 20), Ok(vec![])).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let pool_check = result.checks.iter().find(|c| c.name == "pool").unwrap();
        assert_eq!(pool_check.status, Status::Warn);
        assert_eq!(pool_check.value, "pool 18/20 in-use");
        assert!(pool_check.next_command.as_deref().unwrap().contains("pg_stat_activity"));
    }

    #[tokio::test]
    async fn pool_below_90_pct_is_ok() {
        let result = layer(pool_ok(17, 20), Ok(vec![])).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        let pool_check = result.checks.iter().find(|c| c.name == "pool").unwrap();
        assert_eq!(pool_check.status, Status::Ok);
    }

    #[tokio::test]
    async fn lock_wait_over_5s_is_warn_with_pid_hint() {
        let waits = vec![LockWait {
            waiting_pid: 1234,
            blocking_pid: Some(5678),
            relation: Some("orders".into()),
            wait_secs: 12.5,
        }];
        let result = layer(pool_ok(5, 20), Ok(waits)).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let lock_check = result.checks.iter().find(|c| c.name == "locks").unwrap();
        assert_eq!(lock_check.status, Status::Warn);
        assert_eq!(lock_check.value, "1 lock waits");

        let lw = result.checks.iter().find(|c| c.name == "lock_wait").unwrap();
        assert_eq!(lw.status, Status::Warn);
        assert!(lw.value.contains("1234"), "value: {}", lw.value);
        assert!(lw.value.contains("12s"), "value: {}", lw.value);
        assert!(lw.value.contains("orders"), "value: {}", lw.value);
        assert!(lw.value.contains("5678"), "value: {}", lw.value);
        let cmd = lw.next_command.as_deref().unwrap();
        assert!(cmd.contains("pg_stat_activity"), "cmd: {cmd}");
        assert!(cmd.contains("1234"), "cmd: {cmd}");
        assert!(cmd.contains("5678"), "cmd: {cmd}");
    }

    #[tokio::test]
    async fn lock_wait_under_5s_is_ok() {
        let waits = vec![LockWait {
            waiting_pid: 100,
            blocking_pid: Some(200),
            relation: Some("users".into()),
            wait_secs: 4.9,
        }];
        let result = layer(pool_ok(5, 20), Ok(waits)).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        assert_eq!(result.checks.iter().filter(|c| c.name == "lock_wait").count(), 0);
    }

    #[tokio::test]
    async fn unreachable_postgres_reports_unknown_with_kubectl_hint() {
        let result = layer(Err("connection refused"), Ok(vec![])).run(&opts()).await;

        assert_eq!(result.status, Status::Unknown);
        let pool_check = result.checks.iter().find(|c| c.name == "pool").unwrap();
        assert_eq!(pool_check.status, Status::Unknown);
        assert!(pool_check.next_command.as_deref().unwrap().contains("kubectl get svc"));
    }

    #[tokio::test]
    async fn lock_wait_no_blocking_pid_shows_unknown() {
        let waits = vec![LockWait {
            waiting_pid: 999,
            blocking_pid: None,
            relation: Some("events".into()),
            wait_secs: 10.0,
        }];
        let result = layer(pool_ok(5, 20), Ok(waits)).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let lw = result.checks.iter().find(|c| c.name == "lock_wait").unwrap();
        assert!(lw.value.contains("blocked by pid unknown"), "value: {}", lw.value);
        let cmd = lw.next_command.as_deref().unwrap();
        assert!(cmd.contains("999"), "cmd: {cmd}");
    }

    #[tokio::test]
    async fn pool_warn_and_lock_waits_both_appear_in_json_checks() {
        let waits = vec![LockWait {
            waiting_pid: 42,
            blocking_pid: Some(43),
            relation: Some("jobs".into()),
            wait_secs: 7.0,
        }];
        let result = layer(pool_ok(18, 20), Ok(waits)).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        assert!(result.checks.iter().any(|c| c.name == "pool" && c.status == Status::Warn));
        assert!(result.checks.iter().any(|c| c.name == "locks" && c.status == Status::Warn));
        assert!(result.checks.iter().any(|c| c.name == "lock_wait"));
    }
}
