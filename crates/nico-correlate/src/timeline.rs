use crate::event::{Event, Severity};

pub fn filter_timeline(mut events: Vec<Event>, first_n: usize, last_n: usize) -> Vec<Event> {
    events.sort_by_key(|e| e.ts);
    if events.len() <= first_n + last_n {
        return events;
    }
    let mut keep = vec![false; events.len()];
    for i in 0..first_n.min(events.len()) {
        keep[i] = true;
    }
    for i in events.len().saturating_sub(last_n)..events.len() {
        keep[i] = true;
    }
    for (i, e) in events.iter().enumerate() {
        if matches!(e.severity, Severity::Error | Severity::Warning) {
            keep[i] = true;
        }
    }
    events.into_iter().zip(keep).filter_map(|(e, k)| k.then_some(e)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Severity};
    use chrono::{TimeZone, Utc};

    fn event(secs: i64, kind: &str, severity: Severity) -> Event {
        Event {
            ts: Utc.timestamp_opt(secs, 0).unwrap(),
            source: "temporal".into(),
            kind: kind.into(),
            message: kind.into(),
            severity,
        }
    }

    #[test]
    fn stable_sort_preserves_insertion_order_on_tie() {
        let ts = Utc.timestamp_opt(100, 0).unwrap();
        let events = vec![
            Event { ts, source: "temporal".into(), kind: "First".into(), message: "".into(), severity: Severity::Info },
            Event { ts, source: "postgres".into(), kind: "Second".into(), message: "".into(), severity: Severity::Info },
        ];
        let result = filter_timeline(events, 10, 10);
        assert_eq!(result[0].kind, "First");
        assert_eq!(result[1].kind, "Second");
    }

    #[test]
    fn always_includes_error_events() {
        let mut events: Vec<Event> = (1..=10)
            .map(|i| event(i * 100, &format!("E{i}"), Severity::Info))
            .collect();
        events[4].severity = Severity::Error;
        events[4].kind = "E5-error".into();
        let result = filter_timeline(events, 2, 2);
        let kinds: Vec<&str> = result.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["E1", "E2", "E5-error", "E9", "E10"]);
    }

    #[test]
    fn keeps_first_and_last_n() {
        let events: Vec<Event> = (1..=10)
            .map(|i| event(i * 100, &format!("E{i}"), Severity::Info))
            .collect();
        let result = filter_timeline(events, 2, 3);
        let kinds: Vec<&str> = result.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["E1", "E2", "E8", "E9", "E10"]);
    }

    #[test]
    fn sorts_chronologically() {
        let events = vec![
            event(300, "C", Severity::Info),
            event(100, "A", Severity::Info),
            event(200, "B", Severity::Info),
        ];
        let result = filter_timeline(events, 10, 10);
        let kinds: Vec<&str> = result.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["A", "B", "C"]);
    }
}
