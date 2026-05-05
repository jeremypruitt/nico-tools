use crate::event::Event;

#[derive(Debug, PartialEq)]
pub struct Diagnosis {
    pub pattern: String,
    pub activity: String,
    pub error_signature: String,
    pub next_commands: Vec<String>,
}

pub fn diagnose(events: &[Event]) -> Option<Diagnosis> {
    activity_retry_exhaustion(events)
}

fn activity_retry_exhaustion(events: &[Event]) -> Option<Diagnosis> {
    // Find events flagged at max retries with activity and error info
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

    // Collect all error signatures for this activity across every attempt
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
    use chrono::Utc;
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

    fn info_event() -> Event {
        Event {
            ts: Utc::now(),
            source: "temporal".into(),
            kind: "EVENT_TYPE_WORKFLOW_EXECUTION_STARTED".into(),
            message: "WorkflowStarted".into(),
            severity: Severity::Info,
            tags: Default::default(),
        }
    }

    #[test]
    fn fires_on_matching_event_slice() {
        let events = vec![
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Redfish 503", true),
        ];
        let diag = diagnose(&events);
        assert!(diag.is_some());
        let d = diag.unwrap();
        assert_eq!(d.pattern, "activity_retry_exhaustion");
        assert_eq!(d.activity, "FirmwareUpdate");
        assert_eq!(d.error_signature, "Redfish 503");
        assert_eq!(d.next_commands.len(), 3);
    }

    #[test]
    fn does_not_fire_without_activity_failures() {
        let events = vec![info_event()];
        assert!(diagnose(&events).is_none());
    }

    #[test]
    fn does_not_fire_without_max_retries_marker() {
        let events = vec![
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Redfish 503", false),
        ];
        assert!(diagnose(&events).is_none());
    }

    #[test]
    fn does_not_fire_on_inconsistent_errors() {
        let events = vec![
            failed_event("FirmwareUpdate", "Redfish 503", false),
            failed_event("FirmwareUpdate", "Timeout", true),
        ];
        assert!(diagnose(&events).is_none());
    }
}
