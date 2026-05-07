use std::sync::Arc;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::grpc::{GrpcInspectResult, GrpcInspector};
use crate::layer::{Check, CheckKind, Layer, LayerOutcome, RunOpts};

pub struct GrpcLayer {
    inspector: Arc<dyn GrpcInspector>,
    addr: String,
}

impl GrpcLayer {
    pub fn new(inspector: Arc<dyn GrpcInspector>, addr: String) -> Self {
        Self { inspector, addr }
    }
}

#[async_trait]
impl Layer for GrpcLayer {
    fn name(&self) -> &'static str { "grpc" }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        let result = self.inspector.inspect(&self.addr).await
            .unwrap_or(GrpcInspectResult::Unreachable);
        LayerOutcome::Checks(checks_from(&result, &self.addr))
    }
}

fn checks_from(result: &GrpcInspectResult, addr: &str) -> Vec<Check> {
    match result {
        GrpcInspectResult::Reachable { services } => {
            let svc_count = services.len();
            let method_count: usize = services.iter().map(|s| s.method_count).sum();
            vec![
                Check {
                    name: "reachable",
                    status: Status::Ok,
                    value: "reachable".to_string(),
                    next_command: None,
                    kind: CheckKind::Headline,
                },
                Check {
                    name: "services",
                    status: Status::Ok,
                    value: format!("{svc_count} services"),
                    next_command: None,
                    kind: CheckKind::Headline,
                },
                Check {
                    name: "methods",
                    status: Status::Ok,
                    value: format!("{method_count} methods"),
                    next_command: None,
                    kind: CheckKind::Headline,
                },
            ]
        }
        GrpcInspectResult::Unreachable => {
            vec![Check {
                name: "reachable",
                status: Status::Fail,
                value: "unreachable".to_string(),
                next_command: Some(format!("grpcurl -plaintext {addr} list")),
                kind: CheckKind::Headline,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use crate::grpc::GrpcServiceInfo;

    struct MockReachable {
        services: Vec<(String, usize)>,
    }

    #[async_trait]
    impl GrpcInspector for MockReachable {
        async fn inspect(&self, _addr: &str) -> Result<GrpcInspectResult> {
            Ok(GrpcInspectResult::Reachable {
                services: self.services.iter().map(|(name, count)| GrpcServiceInfo {
                    name: name.clone(),
                    method_count: *count,
                }).collect(),
            })
        }
    }

    struct MockUnreachable;

    #[async_trait]
    impl GrpcInspector for MockUnreachable {
        async fn inspect(&self, _addr: &str) -> Result<GrpcInspectResult> {
            Ok(GrpcInspectResult::Unreachable)
        }
    }

    struct MockError;

    #[async_trait]
    impl GrpcInspector for MockError {
        async fn inspect(&self, _addr: &str) -> Result<GrpcInspectResult> {
            Err(anyhow::anyhow!("connection refused"))
        }
    }

    fn opts() -> RunOpts {
        RunOpts { namespace: "nico".into(), since: Duration::from_secs(600), timeout: Duration::from_secs(5) }
    }

    #[tokio::test]
    async fn reachable_shows_service_and_method_counts() {
        let inspector = Arc::new(MockReachable {
            services: vec![
                ("nico.v1.HostService".into(), 10),
                ("nico.v1.DpuService".into(), 5),
            ],
        });
        let result = GrpcLayer::new(inspector, "localhost:50051".into()).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        let reachable = result.checks.iter().find(|c| c.name == "reachable").unwrap();
        assert_eq!(reachable.status, Status::Ok);
        let services = result.checks.iter().find(|c| c.name == "services").unwrap();
        assert_eq!(services.value, "2 services");
        let methods = result.checks.iter().find(|c| c.name == "methods").unwrap();
        assert_eq!(methods.value, "15 methods");
    }

    #[tokio::test]
    async fn unreachable_reports_fail_with_grpcurl_hint() {
        let result = GrpcLayer::new(Arc::new(MockUnreachable), "localhost:50051".into()).run(&opts()).await;

        assert_eq!(result.status, Status::Fail);
        let reachable = result.checks.iter().find(|c| c.name == "reachable").unwrap();
        assert_eq!(reachable.status, Status::Fail);
        let cmd = reachable.next_command.as_deref().unwrap();
        assert!(cmd.contains("grpcurl"), "cmd: {cmd}");
        assert!(cmd.contains("localhost:50051"), "cmd: {cmd}");
    }

    #[tokio::test]
    async fn inspector_error_reports_fail_with_grpcurl_hint() {
        let result = GrpcLayer::new(Arc::new(MockError), "core:50051".into()).run(&opts()).await;

        assert_eq!(result.status, Status::Fail);
        let reachable = result.checks.iter().find(|c| c.name == "reachable").unwrap();
        assert_eq!(reachable.status, Status::Fail);
        assert!(reachable.next_command.as_deref().unwrap().contains("grpcurl"));
    }

    #[tokio::test]
    async fn zero_services_reachable_still_reports_ok() {
        let inspector = Arc::new(MockReachable { services: vec![] });
        let result = GrpcLayer::new(inspector, "localhost:50051".into()).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        let services = result.checks.iter().find(|c| c.name == "services").unwrap();
        assert_eq!(services.value, "0 services");
        let methods = result.checks.iter().find(|c| c.name == "methods").unwrap();
        assert_eq!(methods.value, "0 methods");
    }

    #[test]
    fn checks_from_reachable_produces_ok_status() {
        let result = GrpcInspectResult::Reachable {
            services: vec![
                GrpcServiceInfo { name: "svc.v1.Foo".into(), method_count: 3 },
                GrpcServiceInfo { name: "svc.v1.Bar".into(), method_count: 7 },
            ],
        };
        let checks = checks_from(&result, "host:50051");

        let reachable = checks.iter().find(|c| c.name == "reachable").unwrap();
        assert_eq!(reachable.status, Status::Ok);
        let services = checks.iter().find(|c| c.name == "services").unwrap();
        assert_eq!(services.status, Status::Ok);
        let methods = checks.iter().find(|c| c.name == "methods").unwrap();
        assert_eq!(methods.status, Status::Ok);
    }

    #[test]
    fn checks_from_unreachable_produces_fail_status() {
        let result = GrpcInspectResult::Unreachable;
        let checks = checks_from(&result, "host:50051");

        let reachable = checks.iter().find(|c| c.name == "reachable").unwrap();
        assert_eq!(reachable.status, Status::Fail);
        assert!(reachable.next_command.as_deref().unwrap().contains("host:50051"));
    }
}
