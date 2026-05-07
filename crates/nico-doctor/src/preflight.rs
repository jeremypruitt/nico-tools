use async_trait::async_trait;
use anyhow::Result;
use kube::Client;
use kube::Api;
use kube::api::{ListParams, PostParams};
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::authorization::v1::{
    SelfSubjectAccessReview, SelfSubjectAccessReviewSpec, ResourceAttributes,
};

use nico_common::bootstrap::{run_with_budget, BootstrapStepError};
use nico_common::config::BootstrapTimeouts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Reachability,
    TokenExpiry,
    NamespaceExists,
    Rbac,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Reachability => "reachability",
            Step::TokenExpiry => "token_expiry",
            Step::NamespaceExists => "namespace_exists",
            Step::Rbac => "rbac",
        }
    }
}

pub struct Failure {
    pub step: Step,
    pub message: String,
    pub next_command: String,
    /// `true` when the step exceeded its budget (an underlying I/O hang).
    /// Boot probe renderer surfaces this as `timed out after Xs` rather
    /// than the inner error string. See ADR-0013.
    pub timed_out: bool,
}

pub enum Outcome {
    Ok,
    Failed(Failure),
}

#[async_trait]
pub trait PreflightChecks: Send + Sync {
    async fn check_reachability(&self) -> Result<()>;
    async fn check_token_valid(&self) -> Result<()>;
    async fn check_namespace_exists(&self, ns: &str) -> Result<()>;
    async fn check_rbac(&self, ns: &str) -> Result<()>;
}

pub struct KubePreflightClient {
    client: Client,
}

impl KubePreflightClient {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl PreflightChecks for KubePreflightClient {
    async fn check_reachability(&self) -> Result<()> {
        self.client
            .apiserver_version()
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("cannot reach API server: {e}"))
    }

    async fn check_token_valid(&self) -> Result<()> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        match api.list(&ListParams::default().limit(1)).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 401 => {
                Err(anyhow::anyhow!("credential is expired or invalid (HTTP 401)"))
            }
            // 403 = token valid but no list-namespaces permission; other errors are not token issues
            Err(_) => Ok(()),
        }
    }

    async fn check_namespace_exists(&self, ns: &str) -> Result<()> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        match api.get(ns).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => {
                Err(anyhow::anyhow!("namespace '{ns}' not found"))
            }
            Err(e) => Err(anyhow::anyhow!("failed to check namespace '{ns}': {e}")),
        }
    }

    async fn check_rbac(&self, ns: &str) -> Result<()> {
        let sar = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    namespace: Some(ns.to_string()),
                    verb: Some("list".to_string()),
                    resource: Some("pods".to_string()),
                    group: Some("".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let api: Api<SelfSubjectAccessReview> = Api::all(self.client.clone());
        let result = api
            .create(&PostParams::default(), &sar)
            .await
            .map_err(|e| anyhow::anyhow!("RBAC self-check failed: {e}"))?;

        let allowed = result.status.as_ref().map(|s| s.allowed).unwrap_or(false);
        if allowed {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "missing permission: cannot list pods in namespace '{ns}'"
            ))
        }
    }
}

pub async fn run(
    checks: &dyn PreflightChecks,
    namespace: &str,
    timeouts: &BootstrapTimeouts,
) -> Outcome {
    if let Err(e) = run_with_budget(timeouts.reach_api, checks.check_reachability()).await {
        return failed(Step::Reachability, e, "kubectl cluster-info".to_string());
    }

    if let Err(e) = run_with_budget(timeouts.preflight, checks.check_token_valid()).await {
        return failed(Step::TokenExpiry, e, "kubectl auth whoami".to_string());
    }

    if let Err(e) = run_with_budget(timeouts.preflight, checks.check_namespace_exists(namespace)).await {
        return failed(Step::NamespaceExists, e, format!("kubectl get ns {namespace}"));
    }

    if let Err(e) = run_with_budget(timeouts.preflight, checks.check_rbac(namespace)).await {
        return failed(
            Step::Rbac,
            e,
            format!("kubectl auth can-i list pods -n {namespace}"),
        );
    }

    Outcome::Ok
}

fn failed(step: Step, err: BootstrapStepError, next_command: String) -> Outcome {
    Outcome::Failed(Failure {
        step,
        message: err.to_string(),
        next_command,
        timed_out: err.is_timed_out(),
    })
}

pub fn format_failure_json(failure: &Failure, namespace: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "namespace": namespace,
        "preflight": {
            "ok": false,
            "failed_step": failure.step.as_str(),
            "message": failure.message,
            "next_command": failure.next_command,
            "timed_out": failure.timed_out,
        }
    }))
    .unwrap()
}

pub fn ok_section() -> serde_json::Value {
    serde_json::json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct MockChecks {
        reachability: Option<&'static str>,
        token: Option<&'static str>,
        namespace: Option<&'static str>,
        rbac: Option<&'static str>,
        delay: Option<Duration>,
    }

    impl MockChecks {
        fn all_ok() -> Self {
            Self { reachability: None, token: None, namespace: None, rbac: None, delay: None }
        }

        async fn maybe_delay(&self) {
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
        }
    }

    fn fast() -> BootstrapTimeouts {
        // Generous budget — for tests that don't want to time out.
        BootstrapTimeouts {
            reach_api: Duration::from_secs(5),
            preflight: Duration::from_secs(5),
            ..Default::default()
        }
    }

    #[async_trait]
    impl PreflightChecks for MockChecks {
        async fn check_reachability(&self) -> Result<()> {
            self.maybe_delay().await;
            self.reachability.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
        async fn check_token_valid(&self) -> Result<()> {
            self.maybe_delay().await;
            self.token.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
        async fn check_namespace_exists(&self, _ns: &str) -> Result<()> {
            self.maybe_delay().await;
            self.namespace.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
        async fn check_rbac(&self, _ns: &str) -> Result<()> {
            self.maybe_delay().await;
            self.rbac.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
    }

    #[tokio::test]
    async fn reachability_failure_returns_step_and_hint() {
        let mock = MockChecks { reachability: Some("connection refused"), ..MockChecks::all_ok() };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::Reachability);
                assert_eq!(f.next_command, "kubectl cluster-info");
                assert!(f.message.contains("connection refused"));
                assert!(!f.timed_out);
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn token_failure_returns_step_and_hint() {
        let mock = MockChecks { token: Some("401 Unauthorized"), ..MockChecks::all_ok() };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::TokenExpiry);
                assert_eq!(f.next_command, "kubectl auth whoami");
                assert!(!f.timed_out);
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn namespace_failure_returns_step_and_hint() {
        let mock = MockChecks { namespace: Some("namespace 'nico' not found"), ..MockChecks::all_ok() };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::NamespaceExists);
                assert!(f.next_command.contains("kubectl get ns nico"));
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn rbac_failure_returns_step_and_hint() {
        let mock = MockChecks { rbac: Some("cannot list pods"), ..MockChecks::all_ok() };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::Rbac);
                assert!(f.next_command.contains("kubectl auth can-i list pods -n nico"));
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn all_ok_returns_outcome_ok() {
        let mock = MockChecks::all_ok();
        assert!(matches!(run(&mock, "nico", &fast()).await, Outcome::Ok));
    }

    #[tokio::test]
    async fn reachability_failure_short_circuits_remaining_steps() {
        // All later steps would fail too, but we should only see Reachability
        let mock = MockChecks {
            reachability: Some("no route to host"),
            token: Some("should not be called"),
            namespace: Some("should not be called"),
            rbac: Some("should not be called"),
            delay: None,
        };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => assert_eq!(f.step, Step::Reachability),
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn token_failure_short_circuits_namespace_and_rbac() {
        let mock = MockChecks {
            reachability: None,
            token: Some("401 Unauthorized"),
            namespace: Some("should not be called"),
            rbac: Some("should not be called"),
            delay: None,
        };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => assert_eq!(f.step, Step::TokenExpiry),
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn namespace_failure_short_circuits_rbac() {
        let mock = MockChecks {
            reachability: None,
            token: None,
            namespace: Some("namespace 'nico' not found"),
            rbac: Some("should not be called"),
            delay: None,
        };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => assert_eq!(f.step, Step::NamespaceExists),
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn reachability_timeout_surfaces_timed_out_failure() {
        // Inner check would block for 1s; budget is 20ms.
        let mock = MockChecks {
            delay: Some(Duration::from_secs(1)),
            ..MockChecks::all_ok()
        };
        let mut t = fast();
        t.reach_api = Duration::from_millis(20);
        match run(&mock, "nico", &t).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::Reachability);
                assert!(f.timed_out, "expected timed_out=true, got message {:?}", f.message);
                assert!(f.message.contains("timed out"));
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn rbac_timeout_surfaces_timed_out_failure_at_rbac_step() {
        // Reachability/token/namespace pass instantly; rbac hangs.
        struct SlowRbac;
        #[async_trait]
        impl PreflightChecks for SlowRbac {
            async fn check_reachability(&self) -> Result<()> { Ok(()) }
            async fn check_token_valid(&self) -> Result<()> { Ok(()) }
            async fn check_namespace_exists(&self, _ns: &str) -> Result<()> { Ok(()) }
            async fn check_rbac(&self, _ns: &str) -> Result<()> {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            }
        }
        let mut t = fast();
        t.preflight = Duration::from_millis(30);
        match run(&SlowRbac, "nico", &t).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::Rbac);
                assert!(f.timed_out);
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn non_timeout_failure_has_timed_out_false() {
        let mock = MockChecks { rbac: Some("nope"), ..MockChecks::all_ok() };
        match run(&mock, "nico", &fast()).await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::Rbac);
                assert!(!f.timed_out);
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn failure_json_contains_preflight_section_with_timed_out_flag() {
        let failure = Failure {
            step: Step::NamespaceExists,
            message: "namespace 'nico' not found".to_string(),
            next_command: "kubectl get ns nico".to_string(),
            timed_out: false,
        };
        let json_str = format_failure_json(&failure, "nico");
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["namespace"], "nico");
        assert_eq!(json["preflight"]["ok"], false);
        assert_eq!(json["preflight"]["failed_step"], "namespace_exists");
        assert_eq!(json["preflight"]["next_command"], "kubectl get ns nico");
        assert_eq!(json["preflight"]["message"], "namespace 'nico' not found");
        assert_eq!(json["preflight"]["timed_out"], false);
    }
}
