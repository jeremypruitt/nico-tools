use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;

use nico_common::k8s::{K8sClient, PodScope};

/// One round of log collection: the entries gathered, the human-readable
/// label of the source that produced them, and whether the primary
/// (preferred) source was used.
pub struct LogCollection {
    pub label: String,
    pub primary_ok: bool,
    pub entries: Vec<(String, String)>,
}

#[async_trait]
pub trait LogSource: Send + Sync {
    /// Human-readable name used when this source is the one that produced a
    /// `LogCollection`. The chain adapter uses this when annotating fallback
    /// labels (e.g. "k8s (loki unavailable)").
    fn name(&self) -> &str;

    async fn collect(
        &self,
        namespace: &str,
        since: Duration,
        limit: usize,
    ) -> Result<LogCollection>;
}

pub fn best_effort_chain(sources: Vec<Arc<dyn LogSource>>) -> Arc<dyn LogSource> {
    Arc::new(BestEffortChain { sources })
}

struct BestEffortChain {
    sources: Vec<Arc<dyn LogSource>>,
}

#[async_trait]
impl LogSource for BestEffortChain {
    fn name(&self) -> &str {
        self.sources.first().map(|s| s.name()).unwrap_or("none")
    }

    async fn collect(
        &self,
        namespace: &str,
        since: Duration,
        limit: usize,
    ) -> Result<LogCollection> {
        let mut failed: Vec<String> = Vec::new();
        for (idx, source) in self.sources.iter().enumerate() {
            match source.collect(namespace, since, limit).await {
                Ok(mut c) => {
                    if idx == 0 {
                        return Ok(c);
                    }
                    c.primary_ok = false;
                    c.label = format!("{} ({} unavailable)", c.label, failed.join(", "));
                    return Ok(c);
                }
                Err(_) => {
                    failed.push(source.name().to_string());
                }
            }
        }
        anyhow::bail!("no log source available: tried {}", failed.join(", "));
    }
}

pub(crate) fn is_error_line(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("error") || l.contains("panic") || l.contains("fatal")
}

/// Like [`is_error_line`] but also matches `warn`-severity lines. Mirrors the
/// classification done by `nico_ops::model::log_level_from_text` so the
/// cluster layer's `pod_log_tail` filtering aligns with operator-facing UI.
pub(crate) fn is_error_or_warn_line(s: &str) -> bool {
    if is_error_line(s) {
        return true;
    }
    s.to_lowercase().contains("warn")
}

pub struct K8sLogSource {
    k8s: Arc<dyn K8sClient>,
}

impl K8sLogSource {
    pub fn new(k8s: Arc<dyn K8sClient>) -> Self {
        Self { k8s }
    }
}

#[async_trait]
impl LogSource for K8sLogSource {
    fn name(&self) -> &str { "k8s" }

    async fn collect(
        &self,
        namespace: &str,
        since: Duration,
        limit: usize,
    ) -> Result<LogCollection> {
        let pods = self.k8s.list_pods(PodScope::Namespace(namespace)).await?;
        let mut entries = Vec::new();
        for pod in &pods {
            let lines = self.k8s.pod_logs(namespace, &pod.name, since)
                .await
                .unwrap_or_default();
            for line in lines.into_iter().take(limit) {
                if is_error_line(&line) {
                    entries.push((pod.name.clone(), line));
                }
            }
        }
        Ok(LogCollection {
            label: "k8s".to_string(),
            primary_ok: true,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    struct OkSource {
        name: &'static str,
        entries: Vec<(String, String)>,
    }

    #[async_trait]
    impl LogSource for OkSource {
        fn name(&self) -> &str { self.name }
        async fn collect(&self, _: &str, _: Duration, _: usize) -> Result<LogCollection> {
            Ok(LogCollection {
                label: self.name.to_string(),
                primary_ok: true,
                entries: self.entries.clone(),
            })
        }
    }

    struct FailSource { name: &'static str }

    #[async_trait]
    impl LogSource for FailSource {
        fn name(&self) -> &str { self.name }
        async fn collect(&self, _: &str, _: Duration, _: usize) -> Result<LogCollection> {
            Err(anyhow!("{} broken", self.name))
        }
    }

    fn d() -> Duration { Duration::from_secs(1) }

    #[tokio::test]
    async fn chain_returns_first_source_when_it_succeeds() {
        let chain = best_effort_chain(vec![
            Arc::new(OkSource { name: "loki", entries: vec![("p".into(), "x".into())] }),
            Arc::new(OkSource { name: "k8s", entries: vec![] }),
        ]);
        let c = chain.collect("ns", d(), 10).await.unwrap();
        assert_eq!(c.label, "loki");
        assert!(c.primary_ok);
        assert_eq!(c.entries.len(), 1);
    }

    #[tokio::test]
    async fn chain_falls_back_and_annotates_when_first_fails() {
        let chain = best_effort_chain(vec![
            Arc::new(FailSource { name: "loki" }),
            Arc::new(OkSource { name: "k8s", entries: vec![("p".into(), "x".into())] }),
        ]);
        let c = chain.collect("ns", d(), 10).await.unwrap();
        assert_eq!(c.label, "k8s (loki unavailable)");
        assert!(!c.primary_ok);
        assert_eq!(c.entries.len(), 1);
    }

    #[tokio::test]
    async fn chain_returns_error_when_all_sources_fail() {
        let chain = best_effort_chain(vec![
            Arc::new(FailSource { name: "loki" }),
            Arc::new(FailSource { name: "k8s" }),
        ]);
        assert!(chain.collect("ns", d(), 10).await.is_err());
    }
}
