use async_trait::async_trait;
use anyhow::Result;
use kube::Client;
use kube::Api;
use kube::api::{ListParams, PostParams};
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::authorization::v1::{
    SelfSubjectAccessReview, SelfSubjectAccessReviewSpec, ResourceAttributes,
};

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

pub async fn run(checks: &dyn PreflightChecks, namespace: &str) -> Outcome {
    if let Err(e) = checks.check_reachability().await {
        return Outcome::Failed(Failure {
            step: Step::Reachability,
            message: e.to_string(),
            next_command: "kubectl cluster-info".to_string(),
        });
    }

    if let Err(e) = checks.check_token_valid().await {
        return Outcome::Failed(Failure {
            step: Step::TokenExpiry,
            message: e.to_string(),
            next_command: "kubectl auth whoami".to_string(),
        });
    }

    if let Err(e) = checks.check_namespace_exists(namespace).await {
        return Outcome::Failed(Failure {
            step: Step::NamespaceExists,
            message: e.to_string(),
            next_command: format!("kubectl get ns {namespace}"),
        });
    }

    if let Err(e) = checks.check_rbac(namespace).await {
        return Outcome::Failed(Failure {
            step: Step::Rbac,
            message: e.to_string(),
            next_command: format!("kubectl auth can-i list pods -n {namespace}"),
        });
    }

    Outcome::Ok
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

    struct MockChecks {
        reachability: Option<&'static str>,
        token: Option<&'static str>,
        namespace: Option<&'static str>,
        rbac: Option<&'static str>,
    }

    impl MockChecks {
        fn all_ok() -> Self {
            Self { reachability: None, token: None, namespace: None, rbac: None }
        }
    }

    #[async_trait]
    impl PreflightChecks for MockChecks {
        async fn check_reachability(&self) -> Result<()> {
            self.reachability.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
        async fn check_token_valid(&self) -> Result<()> {
            self.token.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
        async fn check_namespace_exists(&self, _ns: &str) -> Result<()> {
            self.namespace.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
        async fn check_rbac(&self, _ns: &str) -> Result<()> {
            self.rbac.map(|e| Err(anyhow::anyhow!("{e}"))).unwrap_or(Ok(()))
        }
    }

    #[tokio::test]
    async fn reachability_failure_returns_step_and_hint() {
        let mock = MockChecks { reachability: Some("connection refused"), ..MockChecks::all_ok() };
        match run(&mock, "nico").await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::Reachability);
                assert_eq!(f.next_command, "kubectl cluster-info");
                assert!(f.message.contains("connection refused"));
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn token_failure_returns_step_and_hint() {
        let mock = MockChecks { token: Some("401 Unauthorized"), ..MockChecks::all_ok() };
        match run(&mock, "nico").await {
            Outcome::Failed(f) => {
                assert_eq!(f.step, Step::TokenExpiry);
                assert_eq!(f.next_command, "kubectl auth whoami");
            }
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn namespace_failure_returns_step_and_hint() {
        let mock = MockChecks { namespace: Some("namespace 'nico' not found"), ..MockChecks::all_ok() };
        match run(&mock, "nico").await {
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
        match run(&mock, "nico").await {
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
        assert!(matches!(run(&mock, "nico").await, Outcome::Ok));
    }

    #[tokio::test]
    async fn reachability_failure_short_circuits_remaining_steps() {
        // All later steps would fail too, but we should only see Reachability
        let mock = MockChecks {
            reachability: Some("no route to host"),
            token: Some("should not be called"),
            namespace: Some("should not be called"),
            rbac: Some("should not be called"),
        };
        match run(&mock, "nico").await {
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
        };
        match run(&mock, "nico").await {
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
        };
        match run(&mock, "nico").await {
            Outcome::Failed(f) => assert_eq!(f.step, Step::NamespaceExists),
            Outcome::Ok => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn failure_json_contains_preflight_section() {
        let failure = Failure {
            step: Step::NamespaceExists,
            message: "namespace 'nico' not found".to_string(),
            next_command: "kubectl get ns nico".to_string(),
        };
        let json_str = format_failure_json(&failure, "nico");
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["namespace"], "nico");
        assert_eq!(json["preflight"]["ok"], false);
        assert_eq!(json["preflight"]["failed_step"], "namespace_exists");
        assert_eq!(json["preflight"]["next_command"], "kubectl get ns nico");
        assert_eq!(json["preflight"]["message"], "namespace 'nico' not found");
    }
}
