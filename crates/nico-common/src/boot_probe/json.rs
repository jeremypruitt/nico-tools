//! `--json` mode payload — silent during the probe, single structured
//! document at the end. ADR-0013 § "`--json`": failure document extends
//! today's `preflight::format_failure_json` with `siblings` and
//! `skipped_steps` so JSON consumers see the same diagnostic fan-out
//! that the visual block conveys.

use serde_json::{json, Value};

use super::state::{ProbeState, StepState};

/// Build the failure document for a probe that ended in at least one
/// failure. Mirrors the historical preflight failure JSON shape with the
/// boot-probe extensions.
pub fn failure_document(state: &ProbeState, namespace: &str) -> Value {
    let (failed_def, failed_state) = state
        .first_failure()
        .expect("failure_document called on a probe with no failure");

    let (failed_message, failed_next, failed_timed_out) = match failed_state {
        StepState::Failed {
            message,
            next_command,
            timed_out,
            ..
        } => (message.clone(), next_command.clone(), *timed_out),
        _ => unreachable!("first_failure must return Failed"),
    };

    let siblings: Vec<Value> = state
        .steps
        .iter()
        .filter(|(d, _)| d.section == failed_def.section && d.id != failed_def.id)
        .map(|(d, s)| step_value(d.id.technical_name(), s))
        .collect();

    // Skipped steps are explicitly skipped by upstream gate failure.
    let skipped_steps: Vec<&'static str> = state
        .steps
        .iter()
        .filter(|(_, s)| matches!(s, StepState::Skipped))
        .map(|(d, _)| d.id.technical_name())
        .collect();

    json!({
        "version": 1,
        "namespace": namespace,
        "preflight": {
            "ok": false,
            "failed_step": failed_def.id.technical_name(),
            "message": failed_message,
            "next_command": failed_next,
            "timed_out": failed_timed_out,
            "siblings": siblings,
            "skipped_steps": skipped_steps,
        }
    })
}

/// Build the success document, with per-step `elapsed` so JSON consumers
/// can audit boot timing.
pub fn success_document(state: &ProbeState, namespace: &str) -> Value {
    let steps: Vec<Value> = state
        .steps
        .iter()
        .map(|(d, s)| step_value(d.id.technical_name(), s))
        .collect();
    json!({
        "version": 1,
        "namespace": namespace,
        "preflight": {
            "ok": true,
            "steps": steps,
        }
    })
}

fn step_value(technical: &str, s: &StepState) -> Value {
    match s {
        StepState::Passed { elapsed } => json!({
            "step": technical,
            "state": "passed",
            "elapsed_ms": elapsed.as_millis() as u64,
        }),
        StepState::Failed {
            elapsed,
            message,
            timed_out,
            ..
        } => json!({
            "step": technical,
            "state": if *timed_out { "timed_out" } else { "failed" },
            "elapsed_ms": elapsed.as_millis() as u64,
            "message": message,
        }),
        StepState::Skipped => json!({
            "step": technical,
            "state": "skipped",
        }),
        StepState::Pending => json!({
            "step": technical,
            "state": "pending",
        }),
        StepState::Active { elapsed } => json!({
            "step": technical,
            "state": "active",
            "elapsed_ms": elapsed.as_millis() as u64,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::boot_probe::state::{Section, StepDef, StepId};

    fn def(id: StepId, sec: Section) -> StepDef {
        StepDef {
            id,
            label: id.technical_name().to_string(),
            section: sec,
            budget: Duration::from_secs(1),
        }
    }

    fn fail(msg: &str, timed_out: bool) -> StepState {
        StepState::Failed {
            elapsed: Duration::from_millis(200),
            message: msg.into(),
            timed_out,
            next_command: "kubectl ...".into(),
        }
    }

    fn passed_ms(ms: u64) -> StepState {
        StepState::Passed {
            elapsed: Duration::from_millis(ms),
        }
    }

    #[test]
    fn failure_doc_includes_failed_step_and_message() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::NamespaceExists, Section::Validating),
                def(StepId::Rbac, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        s.set_state(StepId::Credentials, fail("401", false));
        s.set_state(StepId::NamespaceExists, passed_ms(50));
        s.set_state(StepId::Rbac, passed_ms(60));

        let doc = failure_document(&s, "nico");
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["namespace"], "nico");
        assert_eq!(doc["preflight"]["ok"], false);
        assert_eq!(doc["preflight"]["failed_step"], "token_expiry");
        assert_eq!(doc["preflight"]["message"], "401");
        assert_eq!(doc["preflight"]["timed_out"], false);
    }

    #[test]
    fn failure_doc_includes_sibling_outcomes_in_failed_section() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::NamespaceExists, Section::Validating),
                def(StepId::Rbac, Section::Validating),
                // a step in a different section — should NOT appear as a sibling
                def(StepId::PortForwardPostgres, Section::Serving),
            ],
            "port-forward",
            "auto",
        );
        s.set_state(StepId::Credentials, fail("401", false));
        s.set_state(StepId::NamespaceExists, passed_ms(50));
        s.set_state(StepId::Rbac, fail("denied", false));
        s.set_state(StepId::PortForwardPostgres, StepState::Skipped);

        let doc = failure_document(&s, "nico");
        let siblings = doc["preflight"]["siblings"]
            .as_array()
            .expect("siblings must be array");
        let names: Vec<&str> = siblings
            .iter()
            .map(|v| v["step"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"namespace_exists"), "got: {names:?}");
        assert!(names.contains(&"rbac"), "got: {names:?}");
        assert!(
            !names.contains(&"port_forward_postgres"),
            "siblings must not cross sections: {names:?}"
        );
        assert!(
            !names.contains(&"token_expiry"),
            "failed step itself must not be in siblings: {names:?}"
        );
    }

    #[test]
    fn failure_doc_lists_skipped_steps() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::ReachApiServer, Section::Connecting),
                def(StepId::Credentials, Section::Validating),
                def(StepId::PortForwardPostgres, Section::Serving),
                def(StepId::ReachPostgres, Section::Serving),
            ],
            "port-forward",
            "auto",
        );
        s.set_state(StepId::ReachApiServer, fail("connection refused", false));
        s.set_state(StepId::Credentials, StepState::Skipped);
        s.set_state(StepId::PortForwardPostgres, StepState::Skipped);
        s.set_state(StepId::ReachPostgres, StepState::Skipped);

        let doc = failure_document(&s, "nico");
        let skipped = doc["preflight"]["skipped_steps"]
            .as_array()
            .expect("skipped_steps must be array");
        let names: Vec<&str> = skipped.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"token_expiry"));
        assert!(names.contains(&"port_forward_postgres"));
        assert!(names.contains(&"postgres_reach"));
    }

    #[test]
    fn failure_doc_marks_timed_out_when_step_timed_out() {
        let mut s = ProbeState::new(
            vec![def(StepId::ReachApiServer, Section::Connecting)],
            "port-forward",
            "auto",
        );
        s.set_state(StepId::ReachApiServer, fail("timed out after 5s", true));
        let doc = failure_document(&s, "nico");
        assert_eq!(doc["preflight"]["timed_out"], true);
    }

    #[test]
    fn success_doc_lists_all_steps_with_elapsed() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::LoadKubeconfig, Section::Connecting),
                def(StepId::ReachApiServer, Section::Connecting),
            ],
            "port-forward",
            "auto",
        );
        s.set_state(StepId::LoadKubeconfig, passed_ms(100));
        s.set_state(StepId::ReachApiServer, passed_ms(300));
        let doc = success_document(&s, "nico");
        assert_eq!(doc["preflight"]["ok"], true);
        let steps = doc["preflight"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["step"], "kube_client");
        assert_eq!(steps[0]["state"], "passed");
        assert_eq!(steps[0]["elapsed_ms"], 100);
    }
}
