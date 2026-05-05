use serde::Serialize;
use crate::diagnosis::Diagnosis;
use crate::event::{Event, Severity};
use crate::source::StateEntry;

#[derive(Serialize)]
pub struct JsonDiagnosis {
    pub pattern: String,
    pub activity: String,
    pub error_signature: String,
    pub next_commands: Vec<String>,
}

#[derive(Serialize)]
pub struct JsonEvent {
    pub ts: String,
    pub source: String,
    pub kind: String,
    pub severity: String,
}

#[derive(Serialize)]
pub struct JsonStateEntry {
    pub source: String,
    pub key: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct JsonOutput {
    pub version: u32,
    pub id: String,
    pub id_type: String,
    pub events: Vec<JsonEvent>,
    pub sources_restricted: Vec<String>,
    pub sources_unavailable: Vec<String>,
    pub state: Vec<JsonStateEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<JsonDiagnosis>,
}

pub fn format_json(
    id: &str,
    id_type: &str,
    events: &[Event],
    sources_restricted: &[&str],
    sources_unavailable: &[&str],
    state: &[StateEntry],
    diagnosis: Option<&Diagnosis>,
) -> String {
    let out = JsonOutput {
        version: 1,
        id: id.to_string(),
        id_type: id_type.to_string(),
        events: events.iter().map(|e| JsonEvent {
            ts: e.ts.to_rfc3339(),
            source: e.source.clone(),
            kind: e.kind.clone(),
            severity: match e.severity {
                Severity::Info => "info".to_string(),
                Severity::Warning => "warning".to_string(),
                Severity::Error => "error".to_string(),
            },
        }).collect(),
        sources_restricted: sources_restricted.iter().map(|s| s.to_string()).collect(),
        sources_unavailable: sources_unavailable.iter().map(|s| s.to_string()).collect(),
        state: state.iter().map(|s| JsonStateEntry {
            source: s.source.to_string(),
            key: s.key.clone(),
            value: s.value.clone(),
        }).collect(),
        diagnosis: diagnosis.map(|d| JsonDiagnosis {
            pattern: d.pattern.clone(),
            activity: d.activity.clone(),
            error_signature: d.error_signature.clone(),
            next_commands: d.next_commands.clone(),
        }),
    };
    serde_json::to_string_pretty(&out).unwrap()
}
