//! Boot probe — multi-line bootstrap progress visualization (ADR-0013).
//!
//! Replaces the old `nico: reach mode: …` line + 20s blinking cursor with
//! a themed status block on stderr that updates in place via cursor
//! moves, runs checks in parallel where dependencies allow, and surfaces
//! all diagnostic results in one boot.
//!
//! Modules:
//!   - `state`: pure data — step IDs, sections, per-step state.
//!   - `render`: pure block rendering — produces the multi-line string.
//!   - `log`: non-TTY degradation — per-event log lines.
//!   - `json`: success/failure documents for `--json` mode.
//!   - `orchestrate`: async runner driving rendering + state transitions.

pub mod json;
pub mod log;
pub mod orchestrate;
pub mod render;
pub mod state;

pub use orchestrate::{
    next_command_for, standard_steps, standard_steps_with_grpc, BootProbe, ProbeMode,
    ProbeOutcome, ProbeSink, StderrSink, Tracker,
};
pub use render::{render_block, render_bar, RenderMode};
pub use state::{ProbeState, Section, StepDef, StepId, StepState};
