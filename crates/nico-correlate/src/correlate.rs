use crate::id::IdType;
use crate::source::SourceResult;

pub fn exit_code(id_type: Option<&IdType>, results: &[SourceResult]) -> i32 {
    if id_type.is_none() {
        return 1;
    }
    let has_unavailable = results.iter().any(|r| matches!(r, SourceResult::Unavailable(_)));
    let has_events = results.iter().any(|r| matches!(r, SourceResult::Output(o) if !o.events.is_empty()));
    if !has_events {
        return 1;
    }
    if has_unavailable { 2 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Severity};
    use crate::source::{SourceOutput, SourceUnavailable};
    use chrono::Utc;

    fn info_event() -> Event {
        Event { ts: Utc::now(), source: "temporal".into(), kind: "Started".into(), message: "".into(), severity: Severity::Info, tags: Default::default() }
    }

    fn ok_result(events: Vec<Event>) -> SourceResult {
        SourceResult::Output(SourceOutput { events, state: vec![] })
    }

    #[test]
    fn unrecognized_id_exits_1() {
        assert_eq!(exit_code(None, &[]), 1);
    }

    #[test]
    fn found_exits_0() {
        let results = vec![ok_result(vec![info_event()])];
        assert_eq!(exit_code(Some(&IdType::Workflow), &results), 0);
    }

    #[test]
    fn partial_availability_exits_2() {
        let results = vec![
            ok_result(vec![info_event()]),
            SourceResult::Unavailable(SourceUnavailable { name: "postgres", reason: "timeout".into() }),
        ];
        assert_eq!(exit_code(Some(&IdType::Workflow), &results), 2);
    }
}
