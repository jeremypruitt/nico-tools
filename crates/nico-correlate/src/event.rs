use chrono::{DateTime, Utc};
use std::collections::HashMap;

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
    pub tags: HashMap<String, String>,
}
