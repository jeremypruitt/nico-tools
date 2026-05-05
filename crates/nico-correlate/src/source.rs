use async_trait::async_trait;
use crate::event::Event;
use crate::id::IdType;

/// Canonical, ordered list of all sources. Add a new variant here (and update
/// `name` + `from_name`) to register a new source; the compiler will flag every
/// incomplete match arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Temporal,
    Postgres,
    K8s,
    Loki,
    Redfish,
}

impl SourceKind {
    pub const ALL: &'static [SourceKind] = &[
        SourceKind::Temporal,
        SourceKind::Postgres,
        SourceKind::K8s,
        SourceKind::Loki,
        SourceKind::Redfish,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SourceKind::Temporal => "temporal",
            SourceKind::Postgres => "postgres",
            SourceKind::K8s     => "k8s",
            SourceKind::Loki    => "loki",
            SourceKind::Redfish => "redfish",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "temporal" => Some(SourceKind::Temporal),
            "postgres" => Some(SourceKind::Postgres),
            "k8s"      => Some(SourceKind::K8s),
            "loki"     => Some(SourceKind::Loki),
            "redfish"  => Some(SourceKind::Redfish),
            _          => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateEntry {
    pub source: &'static str,
    pub key: String,
    pub value: String,
}

pub struct SourceOutput {
    pub events: Vec<Event>,
    pub state: Vec<StateEntry>,
}

pub struct SourceUnavailable {
    pub name: &'static str,
    #[allow(dead_code)]
    pub reason: String,
}

pub enum SourceResult {
    Output(SourceOutput),
    Unavailable(SourceUnavailable),
}

#[async_trait]
pub trait Source: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult;
}

pub struct UnavailableSource {
    name: &'static str,
    reason: String,
}

impl UnavailableSource {
    pub fn new(name: &'static str, reason: impl Into<String>) -> Self {
        Self { name, reason: reason.into() }
    }
}

#[async_trait]
impl Source for UnavailableSource {
    fn name(&self) -> &'static str { self.name }
    async fn collect(&self, _id: &str, _id_type: &IdType) -> SourceResult {
        SourceResult::Unavailable(SourceUnavailable { name: self.name, reason: self.reason.clone() })
    }
}
