use async_trait::async_trait;
use anyhow::Result;

pub struct ServiceEndpoint {
    pub name: String,
    pub base_url: String,
}

#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn get_status(&self, url: &str) -> Result<u16>;
}
