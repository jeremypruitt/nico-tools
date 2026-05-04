use async_trait::async_trait;
use crate::event::Event;
use crate::id::IdType;

pub struct SourceUnavailable {
    pub name: &'static str,
    pub reason: String,
}

pub enum SourceResult {
    Events(Vec<Event>),
    Unavailable(SourceUnavailable),
}

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult;
}
