use std::sync::Arc;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::http::{HttpClient, ServiceEndpoint};
use crate::layer::{Check, CheckKind, Layer, LayerOutcome, RunOpts};

enum ProbeOutcome { Healthy, Degraded, Failed }

struct ServiceProbe {
    name: String,
    base_url: String,
    outcome: ProbeOutcome,
}

fn checks_from(probes: &[ServiceProbe]) -> Vec<Check> {
    let total = probes.len();
    let healthy = probes.iter().filter(|p| matches!(p.outcome, ProbeOutcome::Healthy)).count();
    let degraded = probes.iter().filter(|p| matches!(p.outcome, ProbeOutcome::Degraded)).count();
    let failed = probes.iter().filter(|p| matches!(p.outcome, ProbeOutcome::Failed)).count();

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
        kind: CheckKind::Headline,
    }];

    for probe in probes {
        match probe.outcome {
            ProbeOutcome::Failed => checks.push(Check {
                name: "service",
                status: Status::Fail,
                value: format!("{} /healthz failed", probe.name),
                next_command: Some(format!("curl -s {}/healthz", probe.base_url)),
                kind: CheckKind::Headline,
            }),
            ProbeOutcome::Degraded => checks.push(Check {
                name: "service",
                status: Status::Warn,
                value: format!("{} degraded (/readyz)", probe.name),
                next_command: Some(format!("curl -s {}/readyz", probe.base_url)),
                kind: CheckKind::Headline,
            }),
            ProbeOutcome::Healthy => {}
        }
    }

    checks
}

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

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        let mut probes = Vec::new();

        for svc in &self.services {
            let healthz_ok = self.client
                .get_status(&format!("{}/healthz", svc.base_url))
                .await
                .map(|s| s < 400)
                .unwrap_or(false);

            if !healthz_ok {
                probes.push(ServiceProbe {
                    name: svc.name.clone(),
                    base_url: svc.base_url.clone(),
                    outcome: ProbeOutcome::Failed,
                });
                continue;
            }

            let readyz_ok = self.client
                .get_status(&format!("{}/readyz", svc.base_url))
                .await
                .map(|s| s < 400)
                .unwrap_or(false);

            if !readyz_ok {
                probes.push(ServiceProbe {
                    name: svc.name.clone(),
                    base_url: svc.base_url.clone(),
                    outcome: ProbeOutcome::Degraded,
                });
            } else {
                probes.push(ServiceProbe {
                    name: svc.name.clone(),
                    base_url: svc.base_url.clone(),
                    outcome: ProbeOutcome::Healthy,
                });
            }
        }

        LayerOutcome::Checks(checks_from(&probes))
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

    fn probe(name: &str, base_url: &str, outcome: ProbeOutcome) -> ServiceProbe {
        ServiceProbe { name: name.into(), base_url: base_url.into(), outcome }
    }

    #[test]
    fn checks_from_all_healthy_is_ok() {
        let probes = vec![
            probe("core", "http://core:8080", ProbeOutcome::Healthy),
            probe("rest", "http://rest:8080", ProbeOutcome::Healthy),
        ];
        let checks = checks_from(&probes);
        let ep = checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert_eq!(ep.status, Status::Ok);
        assert_eq!(checks.iter().filter(|c| c.name == "service").count(), 0);
    }

    #[test]
    fn checks_from_degraded_probe_is_warn() {
        let probes = vec![probe("core", "http://core:8080", ProbeOutcome::Degraded)];
        let checks = checks_from(&probes);
        let ep = checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert_eq!(ep.status, Status::Warn);
        let svc = checks.iter().find(|c| c.name == "service").unwrap();
        assert_eq!(svc.status, Status::Warn);
    }

    #[test]
    fn checks_from_failed_probe_is_fail() {
        let probes = vec![probe("core", "http://core:8080", ProbeOutcome::Failed)];
        let checks = checks_from(&probes);
        let ep = checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert_eq!(ep.status, Status::Fail);
        let svc = checks.iter().find(|c| c.name == "service").unwrap();
        assert_eq!(svc.status, Status::Fail);
    }

    #[test]
    fn checks_from_mixed_produces_correct_service_check_count() {
        let probes = vec![
            probe("a", "http://a:8080", ProbeOutcome::Healthy),
            probe("b", "http://b:8080", ProbeOutcome::Degraded),
            probe("c", "http://c:8080", ProbeOutcome::Failed),
        ];
        let checks = checks_from(&probes);
        let ep = checks.iter().find(|c| c.name == "endpoints").unwrap();
        assert_eq!(ep.status, Status::Fail);
        let svc_checks: Vec<_> = checks.iter().filter(|c| c.name == "service").collect();
        assert_eq!(svc_checks.len(), 2);
        assert!(svc_checks.iter().any(|c| c.status == Status::Warn));
        assert!(svc_checks.iter().any(|c| c.status == Status::Fail));
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
