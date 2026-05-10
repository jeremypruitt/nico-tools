//! Data model for the boot probe — see ADR-0013.
//!
//! A probe owns an ordered list of `StepDef`s grouped into sections. Each
//! step has a `StepState` that the orchestrator transitions through:
//! `Pending` → `Active` → (`Passed` | `Failed` | `Skipped`).
//!
//! The model is pure data; rendering and orchestration live in sibling
//! modules so the state can be unit-tested without I/O.

use std::time::Duration;

/// Stable identifier for each step the boot probe tracks. Maps 1:1 to a
/// "technical name" that surfaces in the JSON failure payload and the
/// failure card's `step:` line. The plain-English label rendered in the
/// live block is a separate field on `StepDef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepId {
    LoadKubeconfig,
    ReachApiServer,
    Credentials,
    /// Capability-based deployment-type detection (PRD-001 slice 1).
    /// Sits between `Credentials` and `NamespaceExists` because the
    /// latter needs the resolved namespace from the deployment-type's
    /// capability bundle.
    DetectDeploymentType,
    NamespaceExists,
    Rbac,
    PortForwardWorkflows,
    PortForwardGrpc,
    PortForwardPostgres,
    ReachPostgres,
    /// Capability-based InfiniBand presence probe (PRD-004 slice 1).
    /// Runs after `ReachPostgres` because it queries
    /// `machines.inventory->'infiniband_interfaces'`. Skipped when
    /// `forgedb_present != Some(true)` or when `--deployment-type=force`.
    DetectInfinibandPresent,
}

impl StepId {
    /// Stable identifier used in JSON payloads and the failure card's
    /// `step:` line. ADR-0013 distinguishes this from the live block's
    /// plain-English label.
    pub fn technical_name(self) -> &'static str {
        match self {
            Self::LoadKubeconfig => "kube_client",
            Self::ReachApiServer => "reachability",
            Self::Credentials => "token_expiry",
            Self::DetectDeploymentType => "detect_deployment_type",
            Self::NamespaceExists => "namespace_exists",
            Self::Rbac => "rbac",
            Self::PortForwardWorkflows => "port_forward_workflows",
            Self::PortForwardGrpc => "port_forward_grpc",
            Self::PortForwardPostgres => "port_forward_postgres",
            Self::ReachPostgres => "postgres_reach",
            Self::DetectInfinibandPresent => "detect_infiniband_present",
        }
    }
}

/// Sections render as named groups in the live block and govern the
/// orchestrator's gate semantics: `Connecting` runs sequentially as a
/// gate; `Validating` and `Serving` fan out concurrently after the gate
/// passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Connecting,
    Validating,
    Serving,
}

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Validating => "validating",
            Self::Serving => "serving",
        }
    }
}

/// One row in the live block. `label` is plain-English; the technical
/// name comes from `id.technical_name()`.
#[derive(Debug, Clone)]
pub struct StepDef {
    pub id: StepId,
    pub label: String,
    pub section: Section,
    pub budget: Duration,
}

/// Per-step lifecycle state.
#[derive(Debug, Clone)]
pub enum StepState {
    Pending,
    Active { elapsed: Duration },
    Passed { elapsed: Duration },
    Failed {
        elapsed: Duration,
        message: String,
        timed_out: bool,
        next_command: String,
    },
    Skipped,
}

impl StepState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Passed { .. } | Self::Failed { .. } | Self::Skipped
        )
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// The whole probe's state at a single instant — what a renderer needs.
#[derive(Debug, Clone)]
pub struct ProbeState {
    pub steps: Vec<(StepDef, StepState)>,
    /// `(<mode>, <source>)` to drop into the block header
    /// `◐ booting nico  ·  reach: <mode> (<source>)` line.
    pub reach_mode: String,
    pub reach_source: String,
    /// Resolved deployment-type label for the banner's `type: …` segment
    /// (PRD-001). `None` means `auto` and detection has not produced a
    /// resolved type yet — the banner reads `type: auto`. `Some("full")`
    /// + `deployment_type_source = "flag"` reads `type: full (flag)`.
    pub deployment_type: Option<String>,
    pub deployment_type_source: String,
    /// Override-conflict warnings rendered between the banner header
    /// and the first section (PRD-001 §"Capability vocabulary >
    /// Override-conflict warning rule"). One pre-formatted line per
    /// contradicting key.
    pub warnings: Vec<String>,
    /// Resolved InfiniBand presence for the banner's `ib: …` segment
    /// (PRD-004 slice 1). `None` renders `ib: unknown` (auto pre-probe,
    /// `--deployment-type=force`, gate unmet, or detection skipped);
    /// `Some(true)` → `present`; `Some(false)` → `absent`.
    pub infiniband_present: Option<bool>,
    /// Total wall time elapsed since the probe started — used by the
    /// success receipt and the failure card.
    pub total_elapsed: Duration,
}

impl ProbeState {
    pub fn new(
        steps: Vec<StepDef>,
        reach_mode: impl Into<String>,
        reach_source: impl Into<String>,
    ) -> Self {
        Self {
            steps: steps.into_iter().map(|d| (d, StepState::Pending)).collect(),
            reach_mode: reach_mode.into(),
            reach_source: reach_source.into(),
            deployment_type: None,
            deployment_type_source: "auto".into(),
            warnings: Vec::new(),
            infiniband_present: None,
            total_elapsed: Duration::ZERO,
        }
    }

    /// Builder-style chain to populate the deployment-type banner tag.
    /// Pass `Some(label)` for a resolved type and the source-tag string
    /// (`auto | flag | config | force`).
    pub fn with_deployment_type(
        mut self,
        deployment_type: Option<String>,
        source: impl Into<String>,
    ) -> Self {
        self.deployment_type = deployment_type;
        self.deployment_type_source = source.into();
        self
    }

    /// Builder-style chain to attach override-conflict warning lines
    /// (PRD-001 slice 5). Renderer paints them between the banner
    /// header and the first section.
    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }

    /// Builder-style chain to seed the resolved InfiniBand presence
    /// (PRD-004 slice 1). Boot probe replaces this at runtime via the
    /// `detect_infiniband_present` step's outcome.
    pub fn with_infiniband_present(mut self, val: Option<bool>) -> Self {
        self.infiniband_present = val;
        self
    }

    pub fn step_state(&self, id: StepId) -> Option<&StepState> {
        self.steps.iter().find(|(d, _)| d.id == id).map(|(_, s)| s)
    }

    pub fn set_state(&mut self, id: StepId, new: StepState) {
        if let Some((_, s)) = self.steps.iter_mut().find(|(d, _)| d.id == id) {
            *s = new;
        }
    }

    pub fn completed_count(&self) -> usize {
        self.steps.iter().filter(|(_, s)| s.is_terminal()).count()
    }

    pub fn total_count(&self) -> usize {
        self.steps.len()
    }

    pub fn any_failed(&self) -> bool {
        self.steps.iter().any(|(_, s)| s.is_failed())
    }

    pub fn all_passed(&self) -> bool {
        self.steps
            .iter()
            .all(|(_, s)| matches!(s, StepState::Passed { .. }))
    }

    /// First failed step's def + state, in step order. Useful for the
    /// failure card and JSON `failed_step` field.
    pub fn first_failure(&self) -> Option<(&StepDef, &StepState)> {
        self.steps
            .iter()
            .find(|(_, s)| s.is_failed())
            .map(|(d, s)| (d, s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: StepId, sec: Section) -> StepDef {
        StepDef {
            id,
            label: id.technical_name().to_string(),
            section: sec,
            budget: Duration::from_secs(1),
        }
    }

    #[test]
    fn technical_names_match_adr_0013() {
        assert_eq!(StepId::LoadKubeconfig.technical_name(), "kube_client");
        assert_eq!(StepId::ReachApiServer.technical_name(), "reachability");
        assert_eq!(StepId::Credentials.technical_name(), "token_expiry");
        assert_eq!(
            StepId::DetectDeploymentType.technical_name(),
            "detect_deployment_type"
        );
        assert_eq!(StepId::NamespaceExists.technical_name(), "namespace_exists");
        assert_eq!(StepId::Rbac.technical_name(), "rbac");
        assert_eq!(
            StepId::PortForwardWorkflows.technical_name(),
            "port_forward_workflows"
        );
        assert_eq!(StepId::ReachPostgres.technical_name(), "postgres_reach");
    }

    #[test]
    fn detect_infiniband_present_step_id_has_stable_technical_name() {
        // PRD-004 slice 1: new boot-probe step name must be stable so the
        // failure card / JSON `step:` fields are meaningful.
        assert_eq!(
            StepId::DetectInfinibandPresent.technical_name(),
            "detect_infiniband_present"
        );
    }

    #[test]
    fn pending_state_is_not_terminal_or_failed() {
        let s = StepState::Pending;
        assert!(!s.is_terminal());
        assert!(!s.is_failed());
    }

    #[test]
    fn passed_and_skipped_are_terminal_but_not_failed() {
        let p = StepState::Passed {
            elapsed: Duration::from_millis(100),
        };
        assert!(p.is_terminal());
        assert!(!p.is_failed());

        let s = StepState::Skipped;
        assert!(s.is_terminal());
        assert!(!s.is_failed());
    }

    #[test]
    fn failed_is_both_terminal_and_failed() {
        let s = StepState::Failed {
            elapsed: Duration::from_millis(200),
            message: "boom".into(),
            timed_out: false,
            next_command: "kubectl ...".into(),
        };
        assert!(s.is_terminal());
        assert!(s.is_failed());
    }

    #[test]
    fn probe_state_reports_completed_count_and_total() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::LoadKubeconfig, Section::Connecting),
                def(StepId::ReachApiServer, Section::Connecting),
                def(StepId::Credentials, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        assert_eq!(s.total_count(), 3);
        assert_eq!(s.completed_count(), 0);
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(50),
            },
        );
        assert_eq!(s.completed_count(), 1);
        s.set_state(StepId::ReachApiServer, StepState::Skipped);
        assert_eq!(s.completed_count(), 2);
    }

    #[test]
    fn any_failed_flips_when_any_step_fails() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::Rbac, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        assert!(!s.any_failed());
        s.set_state(
            StepId::Rbac,
            StepState::Failed {
                elapsed: Duration::from_millis(10),
                message: "denied".into(),
                timed_out: false,
                next_command: "kubectl auth can-i ...".into(),
            },
        );
        assert!(s.any_failed());
        assert!(!s.all_passed());
    }

    #[test]
    fn all_passed_only_when_every_step_is_passed() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::Rbac, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        assert!(!s.all_passed());
        s.set_state(
            StepId::Credentials,
            StepState::Passed {
                elapsed: Duration::from_millis(1),
            },
        );
        assert!(!s.all_passed());
        s.set_state(
            StepId::Rbac,
            StepState::Passed {
                elapsed: Duration::from_millis(1),
            },
        );
        assert!(s.all_passed());
    }

    #[test]
    fn probe_state_default_infiniband_present_is_none() {
        // PRD-004 slice 1: `infiniband_present` defaults to `None`
        // (== "unknown"). The boot probe sets it to Some(_) after the
        // detect step resolves.
        let s = ProbeState::new(
            vec![def(StepId::LoadKubeconfig, Section::Connecting)],
            "port-forward",
            "auto",
        );
        assert_eq!(s.infiniband_present, None);
    }

    #[test]
    fn probe_state_with_infiniband_present_sets_field() {
        let s = ProbeState::new(
            vec![def(StepId::LoadKubeconfig, Section::Connecting)],
            "port-forward",
            "auto",
        )
        .with_infiniband_present(Some(true));
        assert_eq!(s.infiniband_present, Some(true));
    }

    #[test]
    fn first_failure_returns_first_in_step_order() {
        let mut s = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::NamespaceExists, Section::Validating),
                def(StepId::Rbac, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        s.set_state(
            StepId::NamespaceExists,
            StepState::Failed {
                elapsed: Duration::from_millis(20),
                message: "first".into(),
                timed_out: false,
                next_command: "x".into(),
            },
        );
        s.set_state(
            StepId::Rbac,
            StepState::Failed {
                elapsed: Duration::from_millis(30),
                message: "second".into(),
                timed_out: false,
                next_command: "y".into(),
            },
        );
        let (d, st) = s.first_failure().expect("expected a failure");
        assert_eq!(d.id, StepId::NamespaceExists);
        match st {
            StepState::Failed { message, .. } => assert_eq!(message, "first"),
            _ => panic!("expected Failed"),
        }
    }
}
