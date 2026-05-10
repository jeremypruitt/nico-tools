//! Per-axis verdict primitives. Each per-DPU layer reduces its raw
//! observation rows to a single [`AxisSummary`] via a pure
//! `<axis>_verdict()` function. Holistic per-DPU and fleet roll-ups
//! (PRD-003 slices 5 + 6) consume these summaries directly so the
//! rollup never reaches back into per-layer rendering.
//!
//! **Module location** (PRD-003 open question, resolved this slice):
//! verdicts live in their own module under `verdicts/` rather than
//! co-located inside each layer module. Co-locating would re-scatter
//! the same primitive PRD-003 wants to unify; pulling them together
//! makes the cross-layer convention discoverable in one place.
//!
//! **`next_command` field policy** (PRD-003 open question, resolved
//! this slice): included on the primitive. Downstream holistic rollups
//! need each axis to carry its own drill-down hint so the rollup can
//! surface "cert: expired (rotate dpu-agent client cert)" without
//! reaching back into the per-layer renderer.

pub mod cert;
pub mod hbn;
pub mod infiniband;
pub mod isolation;

pub use cert::cert_verdict;
pub use hbn::hbn_verdict;
pub use infiniband::ib_verdict;
pub use isolation::isolation_verdict;

use nico_common::output::Status;

/// One per-axis conclusion that a per-DPU layer reduces its raw
/// observation to. The shared shape that holistic per-DPU and fleet
/// rollups (PRD-003 slices 5 + 6) consume.
///
/// `axis` is the layer name (e.g. `"dpu_cert"`) so a rollup can
/// surface the source of each line. `message` is the same one-line
/// human verdict the layer would have rendered for its headline
/// `Check`. `next_command` is the layer's drill-down hint and survives
/// the rollup unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisSummary {
    pub axis: &'static str,
    pub status: Status,
    pub message: String,
    pub next_command: Option<String>,
}
