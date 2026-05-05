use std::time::Duration;
use async_trait::async_trait;
use anyhow::Result;
use reqwest::redirect::Policy;

pub struct ServiceEndpoint {
    pub name: String,
    pub base_url: String,
}

#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn get_status(&self, url: &str) -> Result<u16>;
}

pub struct ReqwestHttpClient {
    inner: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(Policy::none())
            .build()
            .expect("failed to build reqwest client");
        Self { inner }
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get_status(&self, url: &str) -> Result<u16> {
        let resp = self.inner.get(url).send().await?;
        Ok(resp.status().as_u16())
    }
}
