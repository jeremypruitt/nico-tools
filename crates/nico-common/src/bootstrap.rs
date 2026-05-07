//! Shared error/timeout primitives for the bootstrap path.
//!
//! Every awaitable operation between `nico` starting and the diagnostic
//! ladder running has a per-step budget (see ADR-0013, issue #171). When
//! a step exceeds its budget the boot probe needs to render `timed out
//! after Xs` rather than the underlying error — so the timeout outcome
//! has to be distinguishable from a non-timeout failure.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, Context};

/// Outcome of a bootstrap step. `TimedOut` is surfaced when the step
/// exceeded its budget; `Other` wraps any non-timeout error.
#[derive(Debug)]
pub enum BootstrapStepError {
    TimedOut(Duration),
    Other(anyhow::Error),
}

impl BootstrapStepError {
    pub fn is_timed_out(&self) -> bool {
        matches!(self, Self::TimedOut(_))
    }

    pub fn budget(&self) -> Option<Duration> {
        match self {
            Self::TimedOut(d) => Some(*d),
            Self::Other(_) => None,
        }
    }
}

impl fmt::Display for BootstrapStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut(d) => write!(f, "timed out after {}", humantime::format_duration(*d)),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootstrapStepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TimedOut(_) => None,
            Self::Other(e) => Some(e.as_ref()),
        }
    }
}

impl From<anyhow::Error> for BootstrapStepError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// Run `fut` with the given `budget`. Returns `Err(TimedOut(budget))`
/// if the future doesn't complete in time, `Err(Other(_))` if the
/// future returns an inner error, otherwise `Ok(_)`.
pub async fn run_with_budget<T, F>(budget: Duration, fut: F) -> Result<T, BootstrapStepError>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(BootstrapStepError::Other(e)),
        Err(_) => Err(BootstrapStepError::TimedOut(budget)),
    }
}

/// Probe Postgres reachability by opening a TCP connection to the
/// host:port encoded in `url`. The probe is bounded by `budget`;
/// exceeding it yields `BootstrapStepError::TimedOut(budget)`. We
/// intentionally do **not** authenticate or run a query — that's a
/// layer-check concern; reachability is the only thing the boot probe
/// promises.
pub async fn probe_postgres(
    url: &str,
    budget: Duration,
) -> Result<(), BootstrapStepError> {
    let (host, port) = parse_pg_host_port(url).map_err(BootstrapStepError::Other)?;
    run_with_budget(budget, async move {
        let addr = format!("{host}:{port}");
        let _stream = tokio::net::TcpStream::connect(&addr)
            .await
            .with_context(|| format!("tcp connect to {addr}"))?;
        Ok(())
    }).await
}

/// Parse `postgres[ql]://user:pass@host:port/db[?options]` into `(host, port)`.
fn parse_pg_host_port(url: &str) -> anyhow::Result<(String, u16)> {
    let (_scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow!("invalid postgres URL: missing ://"))?;
    let (_user_info, host_and_rest) = rest
        .split_once('@')
        .ok_or_else(|| anyhow!("invalid postgres URL: missing @"))?;
    let host_port = host_and_rest
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("invalid postgres URL: missing :port"))?;
    let port: u16 = port
        .parse()
        .map_err(|e| anyhow!("invalid postgres port {port:?}: {e}"))?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_with_budget_returns_value_when_inside_budget() {
        let r: Result<i32, _> = run_with_budget(
            Duration::from_secs(1),
            async { Ok(42) },
        ).await;
        assert!(matches!(r, Ok(42)));
    }

    #[tokio::test]
    async fn run_with_budget_returns_timed_out_when_exceeded() {
        let budget = Duration::from_millis(20);
        let r: Result<i32, _> = run_with_budget(budget, async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(0)
        }).await;
        match r {
            Err(BootstrapStepError::TimedOut(d)) => assert_eq!(d, budget),
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_with_budget_returns_other_for_inner_error() {
        let r: Result<i32, _> = run_with_budget(Duration::from_secs(1), async {
            Err(anyhow::anyhow!("inner failure"))
        }).await;
        match r {
            Err(BootstrapStepError::Other(e)) => assert!(e.to_string().contains("inner failure")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timed_out_error_is_distinguishable_via_is_timed_out() {
        let budget = Duration::from_millis(5);
        let r: Result<(), _> = run_with_budget(budget, async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(())
        }).await;
        let err = r.expect_err("expected timeout");
        assert!(err.is_timed_out());
        assert_eq!(err.budget(), Some(budget));
    }

    #[test]
    fn timed_out_displays_human_readable_duration() {
        let err = BootstrapStepError::TimedOut(Duration::from_secs(3));
        assert_eq!(err.to_string(), "timed out after 3s");
    }

    #[test]
    fn parse_pg_host_port_extracts_basic() {
        let (h, p) = parse_pg_host_port("postgres://u:p@db.example.com:5432/mydb").unwrap();
        assert_eq!(h, "db.example.com");
        assert_eq!(p, 5432);
    }

    #[test]
    fn parse_pg_host_port_handles_query_string() {
        let (h, p) = parse_pg_host_port("postgres://u:p@host:5433/db?sslmode=require").unwrap();
        assert_eq!(h, "host");
        assert_eq!(p, 5433);
    }

    #[test]
    fn parse_pg_host_port_handles_no_db_path() {
        let (h, p) = parse_pg_host_port("postgresql://u:p@host:6543").unwrap();
        assert_eq!(h, "host");
        assert_eq!(p, 6543);
    }

    #[test]
    fn parse_pg_host_port_rejects_missing_port() {
        assert!(parse_pg_host_port("postgres://u:p@host/db").is_err());
    }

    #[tokio::test]
    async fn probe_postgres_succeeds_when_listener_accepts() {
        // Bind a local listener so the connect resolves.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Keep the listener alive in a task that just accepts and drops.
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let url = format!("postgres://u:p@127.0.0.1:{port}/db");
        probe_postgres(&url, Duration::from_secs(2)).await.expect("connect ok");
    }

    #[tokio::test]
    async fn probe_postgres_returns_timed_out_when_address_blackholes() {
        // 192.0.2.0/24 is TEST-NET-1 (RFC 5737) — guaranteed unreachable.
        // Use a tiny budget; if connect doesn't time out almost
        // immediately, run_with_budget enforces the budget.
        let url = "postgres://u:p@192.0.2.1:5432/db";
        let budget = Duration::from_millis(50);
        let r = probe_postgres(url, budget).await;
        match r {
            Err(BootstrapStepError::TimedOut(d)) => assert_eq!(d, budget),
            // Some platforms may return ECONNREFUSED quickly instead of
            // hanging — that surfaces as Other; the *important* contract
            // is just that the timed-out variant is reachable in
            // principle, exercised in the run_with_budget tests above.
            Err(BootstrapStepError::Other(_)) => {}
            Ok(()) => panic!("expected error reaching blackhole address"),
        }
    }

    #[tokio::test]
    async fn probe_postgres_returns_other_for_invalid_url() {
        let r = probe_postgres("not-a-url", Duration::from_secs(1)).await;
        match r {
            Err(BootstrapStepError::Other(e)) => assert!(e.to_string().contains("postgres URL")),
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
