use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    /// Single canonical classifier for all sources.
    /// `source` is the source name (e.g. "temporal", "postgres", "redfish", "k8s", "loki").
    /// `kind`   is the event type / action / reason string from the raw event.
    /// `detail` is available for future pattern extensions; not currently used.
    pub fn classify(source: &str, kind: &str, _detail: &str) -> Severity {
        match source {
            "temporal" => {
                if kind.contains("Failed") || kind.contains("TimedOut") {
                    Severity::Error
                } else {
                    Severity::Info
                }
            }
            "postgres" => {
                if kind.contains("fail") || kind.contains("error") || kind.contains("delete") {
                    Severity::Warning
                } else {
                    Severity::Info
                }
            }
            "redfish" => {
                if kind.contains("Fault") || kind.contains("Critical") || kind.contains("Failed") {
                    Severity::Error
                } else if kind.contains("Warning") || kind.contains("Degraded") {
                    Severity::Warning
                } else {
                    Severity::Info
                }
            }
            "k8s" => Severity::Warning,
            _ => Severity::Info,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub ts: DateTime<Utc>,
    pub source: String,
    pub kind: String,
    pub message: String,
    pub severity: Severity,
    pub tags: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- temporal ---

    #[test]
    fn temporal_failed_is_error() {
        assert_eq!(Severity::classify("temporal", "WorkflowExecutionFailed", ""), Severity::Error);
    }

    #[test]
    fn temporal_timed_out_is_error() {
        assert_eq!(Severity::classify("temporal", "WorkflowExecutionTimedOut", ""), Severity::Error);
    }

    #[test]
    fn temporal_started_is_info() {
        assert_eq!(Severity::classify("temporal", "WorkflowExecutionStarted", ""), Severity::Info);
    }

    #[test]
    fn temporal_completed_is_info() {
        assert_eq!(Severity::classify("temporal", "WorkflowExecutionCompleted", ""), Severity::Info);
    }

    // --- postgres ---

    #[test]
    fn postgres_fail_action_is_warning() {
        assert_eq!(Severity::classify("postgres", "provision_fail", ""), Severity::Warning);
    }

    #[test]
    fn postgres_error_action_is_warning() {
        assert_eq!(Severity::classify("postgres", "auth_error", ""), Severity::Warning);
    }

    #[test]
    fn postgres_delete_action_is_warning() {
        assert_eq!(Severity::classify("postgres", "delete_host", ""), Severity::Warning);
    }

    #[test]
    fn postgres_create_action_is_info() {
        assert_eq!(Severity::classify("postgres", "create_host", ""), Severity::Info);
    }

    // --- redfish ---

    #[test]
    fn redfish_fault_is_error() {
        assert_eq!(Severity::classify("redfish", "DriveFault", ""), Severity::Error);
    }

    #[test]
    fn redfish_critical_is_error() {
        assert_eq!(Severity::classify("redfish", "CriticalTemperature", ""), Severity::Error);
    }

    #[test]
    fn redfish_failed_is_error() {
        assert_eq!(Severity::classify("redfish", "NetworkAdapterFailed", ""), Severity::Error);
    }

    #[test]
    fn redfish_warning_is_warning() {
        assert_eq!(Severity::classify("redfish", "SystemWarning", ""), Severity::Warning);
    }

    #[test]
    fn redfish_degraded_is_warning() {
        assert_eq!(Severity::classify("redfish", "StorageDegraded", ""), Severity::Warning);
    }

    #[test]
    fn redfish_power_on_is_info() {
        assert_eq!(Severity::classify("redfish", "SystemPowerOn", ""), Severity::Info);
    }

    // --- k8s ---

    #[test]
    fn k8s_any_kind_is_warning() {
        assert_eq!(Severity::classify("k8s", "OOMKilled", ""), Severity::Warning);
        assert_eq!(Severity::classify("k8s", "BackOff", ""), Severity::Warning);
    }

    // --- loki / unknown ---

    #[test]
    fn loki_log_is_info() {
        assert_eq!(Severity::classify("loki", "Log", ""), Severity::Info);
    }

    #[test]
    fn unknown_source_is_info() {
        assert_eq!(Severity::classify("k8s-logs", "Log", ""), Severity::Info);
    }
}
