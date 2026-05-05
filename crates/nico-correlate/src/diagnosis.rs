use chrono::Utc;
use crate::event::Event;
use crate::source::StateEntry;

#[derive(Debug, PartialEq)]
pub struct Diagnosis {
    pub pattern: String,
    pub activity: String,
    pub error_signature: String,
    pub next_commands: Vec<String>,
}

pub fn diagnose(events: &[Event], state: &[StateEntry]) -> Option<Diagnosis> {
    match_k8s_crash_loop(state)
        .or_else(|| match_provisioning_timeout(events))
        .or_else(|| activity_retry_exhaustion(events))
        .or_else(|| match_stuck_workflow(events))
}

fn match_k8s_crash_loop(state: &[StateEntry]) -> Option<Diagnosis> {
    let crash_pod = state.iter()
        .filter(|s| s.source == "k8s")
        .find(|s| {
            s.value.starts_with("CrashLoopBackOff") || restart_count_from_value(&s.value) > 5
        })?;

    let restart_count = restart_count_from_value(&crash_pod.value);
    let pod_name = &crash_pod.key;

    Some(Diagnosis {
        pattern: "k8s_crash_loop".to_string(),
        activity: "(n/a — pod-level failure)".to_string(),
        error_signature: format!("pod {pod_name} in CrashLoopBackOff ({restart_count} restarts)"),
        next_commands: vec![
            format!("kubectl describe pod {pod_name} -n <namespace>"),
            format!("kubectl logs {pod_name} -n <namespace> --previous"),
        ],
    })
}

fn restart_count_from_value(value: &str) -> u32 {
    // Parse N from "status (N restart[s])"
    value.find('(')
        .and_then(|start| {
            let rest = &value[start + 1..];
            rest.find(' ').map(|end| &rest[..end])
        })
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0)
}

fn match_provisioning_timeout(events: &[Event]) -> Option<Diagnosis> {
    let matching: Vec<&Event> = events
        .iter()
        .filter(|e| {
            let name = e.tags.get("activity_name").map(|s| s.as_str()).unwrap_or("");
            let err = e.tags.get("error_signature").map(|s| s.as_str()).unwrap_or("");
            let is_provision = name.contains("Provision") || name.contains("provision");
            let err_lower = err.to_lowercase();
            let is_timeout = err_lower.contains("timeout") || err_lower.contains("deadline");
            is_provision && is_timeout
        })
        .collect();

    if matching.len() < 2 {
        return None;
    }

    let activity = matching[0].tags.get("activity_name")?;
    let error = matching[0].tags.get("error_signature")?;

    Some(Diagnosis {
        pattern: "provisioning_timeout".to_string(),
        activity: activity.clone(),
        error_signature: error.clone(),
        next_commands: vec![
            "kubectl get pods -n <namespace> -l workflow-id=<id>".to_string(),
            "curl -k https://<bmc-ip>/redfish/v1/Systems".to_string(),
        ],
    })
}

fn match_stuck_workflow(events: &[Event]) -> Option<Diagnosis> {
    let temporal_events: Vec<&Event> = events.iter().filter(|e| e.source == "temporal").collect();

    if temporal_events.is_empty() {
        return None;
    }

    let has_terminal = temporal_events.iter().any(|e| {
        e.kind.contains("WorkflowExecutionCompleted") || e.kind.contains("WorkflowExecutionFailed")
    });
    if has_terminal {
        return None;
    }

    let threshold = Utc::now() - chrono::Duration::minutes(30);
    let all_old = temporal_events.iter().all(|e| e.ts < threshold);
    if !all_old {
        return None;
    }

    Some(Diagnosis {
        pattern: "stuck_workflow".to_string(),
        activity: "(none — workflow itself is stuck, no recent activity events)".to_string(),
        error_signature: "no events in the last 30m; workflow still Running".to_string(),
        next_commands: vec![
            "nico-doctor --skip cluster,logs,health,grpc,postgres".to_string(),
            "temporal workflow describe --workflow-id <id>".to_string(),
        ],
    })
}

fn activity_retry_exhaustion(events: &[Event]) -> Option<Diagnosis> {
    let exhausted: Vec<(&str, &str)> = events
        .iter()
        .filter(|e| e.tags.get("at_max_retries").map(|v| v == "true").unwrap_or(false))
        .filter_map(|e| {
            let name = e.tags.get("activity_name")?.as_str();
            let err = e.tags.get("error_signature")?.as_str();
            Some((name, err))
        })
        .collect();

    if exhausted.is_empty() {
        return None;
    }

    let (activity, _) = exhausted[0];

    let all_errors: Vec<&str> = events
        .iter()
        .filter(|e| e.tags.get("activity_name").map(|v| v == activity).unwrap_or(false))
        .filter_map(|e| e.tags.get("error_signature").map(|s| s.as_str()))
        .collect();

    if all_errors.is_empty() {
        return None;
    }

    let first = all_errors[0];
    if !all_errors.iter().all(|&e| e == first) {
        return None;
    }

    Some(Diagnosis {
        pattern: "activity_retry_exhaustion".to_string(),
        activity: activity.to_string(),
        error_signature: first.to_string(),
        next_commands: vec![
            format!("kubectl logs -l app=site-agent --tail=100 | grep {activity}"),
            "nico-correlate <workflow-id> --sources temporal,redfish".to_string(),
            "nico-doctor --check health,grpc".to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Severity;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    fn failed_event(activity: &str, error: &str, at_max: bool) -> Event {
        let mut tags = HashMap::new();
        tags.insert("activity_name".into(), activity.into());
        tags.insert("error_signature".into(), error.into());
        if at_max {
            tags.insert("at_max_retries".into(), "true".into());
        }
        Event {
            ts: Utc::now(),
            source: "temporal".into(),
            kind: "EVENT_TYPE_ACTIVITY_TASK_FAILED".into(),
            message: format!("{activity}: {error}"),
            severity: Severity::Error,
            tags,
        }
    }

    fn temporal_event_at(secs: i64, kind: &str) -> Event {
        Event {
            ts: Utc.timestamp_opt(secs, 0).unwrap(),
            source: "temporal".into(),
            kind: kind.into(),
            message: kind.into(),
            severity: Severity::Info,
            tags: Default::default(),
        }
    }

    fn k8s_state(pod: &str, value: &str) -> StateEntry {
        StateEntry { source: "k8s", key: pod.into(), value: value.into() }
    }

    fn no_state() -> Vec<StateEntry> {
        vec![]
    }

    // --- activity_retry_exhaustion ---

    #[test]
    fn fires_on_matching_event_slice() {
        let events = vec![
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Redfish 503", true),
        ];
        let diag = diagnose(&events, &no_state());
        assert!(diag.is_some());
        let d = diag.unwrap();
        assert_eq!(d.pattern, "activity_retry_exhaustion");
        assert_eq!(d.activity, "FirmwareUpdate");
        assert_eq!(d.error_signature, "Redfish 503");
        assert_eq!(d.next_commands.len(), 3);
    }

    #[test]
    fn does_not_fire_without_activity_failures() {
        let events = vec![temporal_event_at(Utc::now().timestamp(), "WorkflowExecutionStarted")];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn does_not_fire_without_max_retries_marker() {
        let events = vec![
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Redfish 503", false),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn does_not_fire_on_inconsistent_errors() {
        let events = vec![
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Timeout", true),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    // --- stuck_workflow ---

    #[test]
    fn stuck_workflow_fires_when_all_events_old_and_no_terminal() {
        let events = vec![
            temporal_event_at(0, "WorkflowExecutionStarted"),
            temporal_event_at(60, "ActivityTaskScheduled"),
        ];
        let d = diagnose(&events, &no_state()).unwrap();
        assert_eq!(d.pattern, "stuck_workflow");
        assert_eq!(d.next_commands.len(), 2);
    }

    #[test]
    fn stuck_workflow_does_not_fire_when_recent_event_exists() {
        let now = Utc::now().timestamp();
        let events = vec![
            temporal_event_at(0, "WorkflowExecutionStarted"),
            temporal_event_at(now, "ActivityTaskScheduled"),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn stuck_workflow_does_not_fire_when_completed() {
        let events = vec![
            temporal_event_at(0, "WorkflowExecutionStarted"),
            temporal_event_at(60, "WorkflowExecutionCompleted"),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn stuck_workflow_does_not_fire_when_failed() {
        let events = vec![
            temporal_event_at(0, "WorkflowExecutionStarted"),
            temporal_event_at(60, "WorkflowExecutionFailed"),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn stuck_workflow_does_not_fire_without_temporal_events() {
        assert!(diagnose(&[], &no_state()).is_none());
    }

    // --- provisioning_timeout ---

    #[test]
    fn provisioning_timeout_fires_on_two_failures() {
        let events = vec![
            failed_event("ProvisionHost", "deadline exceeded", false),
            failed_event("ProvisionHost", "deadline exceeded", false),
        ];
        let d = diagnose(&events, &no_state()).unwrap();
        assert_eq!(d.pattern, "provisioning_timeout");
        assert_eq!(d.activity, "ProvisionHost");
        assert_eq!(d.next_commands.len(), 2);
    }

    #[test]
    fn provisioning_timeout_fires_on_lowercase_provision() {
        let events = vec![
            failed_event("provisionDpu", "timeout waiting for BMC", false),
            failed_event("provisionDpu", "timeout waiting for BMC", false),
        ];
        let d = diagnose(&events, &no_state()).unwrap();
        assert_eq!(d.pattern, "provisioning_timeout");
    }

    #[test]
    fn provisioning_timeout_does_not_fire_on_single_failure() {
        let events = vec![failed_event("ProvisionHost", "timeout", false)];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn provisioning_timeout_does_not_fire_on_non_provisioning_activity() {
        let events = vec![
            failed_event("FirmwareUpdate", "timeout", false),
            failed_event("FirmwareUpdate", "timeout", false),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    #[test]
    fn provisioning_timeout_does_not_fire_on_non_timeout_error() {
        let events = vec![
            failed_event("ProvisionHost", "Redfish 503", false),
            failed_event("ProvisionHost", "Redfish 503", false),
        ];
        assert!(diagnose(&events, &no_state()).is_none());
    }

    // --- k8s_crash_loop ---

    #[test]
    fn k8s_crash_loop_fires_on_crash_loop_back_off_status() {
        let state = vec![k8s_state("worker-xyz", "CrashLoopBackOff (3 restarts)")];
        let d = diagnose(&[], &state).unwrap();
        assert_eq!(d.pattern, "k8s_crash_loop");
        assert!(d.error_signature.contains("worker-xyz"));
        assert_eq!(d.next_commands.len(), 2);
    }

    #[test]
    fn k8s_crash_loop_fires_on_high_restart_count() {
        let state = vec![k8s_state("worker-abc", "Running (6 restarts)")];
        let d = diagnose(&[], &state).unwrap();
        assert_eq!(d.pattern, "k8s_crash_loop");
        assert!(d.error_signature.contains("worker-abc"));
    }

    #[test]
    fn k8s_crash_loop_does_not_fire_on_low_restart_count() {
        let state = vec![k8s_state("worker-abc", "Running (5 restarts)")];
        assert!(diagnose(&[], &state).is_none());
    }

    #[test]
    fn k8s_crash_loop_does_not_fire_on_non_k8s_state() {
        let state = vec![StateEntry {
            source: "postgres",
            key: "worker-xyz".into(),
            value: "CrashLoopBackOff (3 restarts)".into(),
        }];
        assert!(diagnose(&[], &state).is_none());
    }

    #[test]
    fn k8s_crash_loop_next_commands_include_pod_name() {
        let state = vec![k8s_state("my-pod-123", "CrashLoopBackOff (2 restarts)")];
        let d = diagnose(&[], &state).unwrap();
        assert!(d.next_commands[0].contains("my-pod-123"));
        assert!(d.next_commands[1].contains("my-pod-123"));
    }
}
