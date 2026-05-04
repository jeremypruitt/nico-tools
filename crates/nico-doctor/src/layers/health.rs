use std::sync::Arc;
use std::time::Instant;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::http::{HttpClient, ServiceEndpoint};
use crate::layer::{Check, Layer, LayerResult, RunOpts};

pub struct HealthLayer {
    client: Arc<dyn HttpClient>,
    services: Vec<ServiceEndpoint>,
}

impl HealthLayer {
    pub fn new(client: Arc<dyn HttpClient>, services: Vec<ServiceEndpoint>) -> Self {
        Self { client, services }
    }
}

#[async_trait]
impl Layer for HealthLayer {
    fn name(&self) -> &'static str { "health" }

    async fn run(&self, _opts: &RunOpts) -> LayerResult {
        let start = Instant::now();
        let total = self.services.len();
        let mut healthy = 0usize;
        let mut degraded = 0usize;
        let mut failed = 0usize;
        let mut findings: Vec<Check> = Vec::new();

        for svc in &self.services {
            let healthz_ok = self.client
                .get_status(&format!("{}/healthz", svc.base_url))
                .await
                .map(|s| s < 400)
                .unwrap_or(false);

            if !healthz_ok {
                failed += 1;
                findings.push(Check {
                    name: "service",
                    status: Status::Fail,
                    value: format!("{} /healthz failed", svc.name),
                    next_command: Some(format!("curl -s {}/healthz", svc.base_url)),
                });
                continue;
            }

            let readyz_ok = self.client
                .get_status(&format!("{}/readyz", svc.base_url))
                .await
                .map(|s| s < 400)
                .unwrap_or(false);

            if !readyz_ok {
                degraded += 1;
                findings.push(Check {
                    name: "service",
                    status: Status::Warn,
                    value: format!("{} degraded (/readyz)", svc.name),
                    next_command: Some(format!("curl -s {}/readyz", svc.base_url)),
                });
            } else {
                healthy += 1;
            }
        }

        let summary_value = if degraded == 0 && failed == 0 {
            format!("{healthy}/{total} healthy")
        } else {
            format!("{healthy}/{total} healthy, {degraded} degraded, {failed} failed")
        };

        let summary_status = if failed > 0 {
            Status::Fail
        } else if degraded > 0 {
            Status::Warn
        } else {
            Status::Ok
        };

        let mut checks = vec![Check {
            name: "endpoints",
            status: summary_status,
            value: summary_value,
            next_command: None,
        }];
        checks.extend(findings);

        let overall = if checks.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else if checks.iter().any(|c| c.status == Status::Warn) {
            Status::Warn
        } else {
            Status::Ok
        };

        LayerResult {
            name: "health",
            status: overall,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use anyhow::Result;

    struct MockHttpClient {
        responses: HashMap<String, std::result::Result<u16, String>>,
        default_status: u16,
    }

    #[async_trait]
    impl HttpClient for MockHttpClient {
        async fn get_status(&self, url: &str) -> Result<u16> {
            match self.responses.get(url) {
                Some(Ok(s)) => Ok(*s),
                Some(Err(e)) => Err(anyhow::anyhow!("{}", e)),
                None => Ok(self.default_status),
            }
        }
    }

    fn opts() -> RunOpts {
        RunOpts { namespace: "nico".into(), since: Duration::from_secs(600), timeout: Duration::from_secs(5) }
    }

    fn client(overrides: Vec<(&str, std::result::Result<u16, &str>)>, default_status: u16) -> Arc<MockHttpClient> {
        Arc::new(MockHttpClient {
            responses: overrides.into_iter()
                .map(|(url, r)| (url.to_string(), r.map_err(|e| e.to_string())))
                .collect(),
            default_status,
        })
    }

    fn svc(name: &str, base_url: &str) -> ServiceEndpoint {
        ServiceEndpoint { name: name.into(), base_url: base_url.into() }
    }

    #[tokio::test]
    async fn all_healthy_reports_ok() {
        let result = HealthLayer::new(client(vec![], 200), vec![
            svc("core", "http://core:8080"),
            svc("rest", "http://rest:8080"),
        ]).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        let ep = result.checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert_eq!(ep.value, "2/2 healthy");
        assert_eq!(result.checks.iter().filter(|c| c.name == "service").count(), 0);
    }

    #[tokio::test]
    async fn readyz_fail_with_healthz_ok_is_warn_not_fail() {
        let result = HealthLayer::new(
            client(vec![("http://core:8080/readyz", Ok(503))], 200),
            vec![svc("core", "http://core:8080")],
        ).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let svc_check = result.checks.iter().find(|c| c.name == "service").unwrap();
        assert_eq!(svc_check.status, Status::Warn);
        assert!(svc_check.value.contains("degraded"), "value: {}", svc_check.value);
        assert!(svc_check.next_command.as_deref().unwrap().contains("/readyz"));
    }

    #[tokio::test]
    async fn healthz_fail_is_failure_with_curl_hint() {
        let result = HealthLayer::new(
            client(vec![("http://core:8080/healthz", Ok(500))], 200),
            vec![svc("core", "http://core:8080")],
        ).run(&opts()).await;

        assert_eq!(result.status, Status::Fail);
        let svc_check = result.checks.iter().find(|c| c.name == "service").unwrap();
        assert_eq!(svc_check.status, Status::Fail);
        assert!(svc_check.next_command.as_deref().unwrap().contains("/healthz"));
    }

    #[tokio::test]
    async fn unreachable_service_counts_as_failed() {
        let result = HealthLayer::new(
            client(vec![("http://core:8080/healthz", Err("connection refused"))], 200),
            vec![svc("core", "http://core:8080")],
        ).run(&opts()).await;

        assert_eq!(result.status, Status::Fail);
        let svc_check = result.checks.iter().find(|c| c.name == "service").unwrap();
        assert_eq!(svc_check.status, Status::Fail);
    }

    #[tokio::test]
    async fn mixed_services_shows_counts_and_correct_overall_status() {
        // svca: ok, svcb: degraded (readyz fails), svcc: failed (healthz fails)
        let result = HealthLayer::new(
            client(vec![
                ("http://svcb:8080/readyz", Ok(503)),
                ("http://svcc:8080/healthz", Ok(500)),
            ], 200),
            vec![
                svc("svca", "http://svca:8080"),
                svc("svcb", "http://svcb:8080"),
                svc("svcc", "http://svcc:8080"),
            ],
        ).run(&opts()).await;

        assert_eq!(result.status, Status::Fail);
        let ep = result.checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert!(ep.value.contains("1/3 healthy"), "value: {}", ep.value);
        assert!(ep.value.contains("1 degraded"), "value: {}", ep.value);
        assert!(ep.value.contains("1 failed"), "value: {}", ep.value);
        let svc_checks: Vec<_> = result.checks.iter().filter(|c| c.name == "service").collect();
        assert_eq!(svc_checks.len(), 2);
    }

    #[tokio::test]
    async fn empty_services_list_reports_ok() {
        let result = HealthLayer::new(client(vec![], 200), vec![]).run(&opts()).await;
        assert_eq!(result.status, Status::Ok);
        let ep = result.checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert_eq!(ep.value, "0/0 healthy");
    }
}
