use chrono::{DateTime, Utc};

#[derive(Debug, PartialEq, Clone)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub ts: DateTime<Utc>,
    pub source: String,
    pub kind: String,
    pub message: String,
    pub severity: Severity,
}
