# nico-tools — Domain Context

## Purpose
Diagnostic CLI for nico/carbide/ncx installations. Read-only. Compresses the
operator diagnostic ladder into seconds. Never hides the underlying tools —
every output line points at where to dig deeper.

## Ubiquitous language

- **NICo** — NVIDIA Infrastructure Controller. gRPC service. Source of truth
  for desired host state. This is the external/marketing name for carbide.
- **Carbide** — This is the internal name for NICO
  packages NICo + dependencies into a deployable form.
- **NCX** — Umbrella term for the NICO/carbide components
- **Host** — physical machine managed by NICo. Has a BMC and (usually) a DPU.
- **DPU** — data processing unit attached to a host.
- **Workflow** — Temporal workflow orchestrating host lifecycle (e.g.
  HostProvisioning).
- **Activity** — atomic step inside a workflow, retried independently.
- **Site Agent** — the in-cluster worker that executes activities.
- **Layer** (nico-doctor specific) — one of the six categories the doctor
  checks: cluster, logs, workflows, health, grpc, postgres.
- **Finding** — a single warning or failure produced by a layer.
- **Entity** — the thing being correlated: a Workflow, Host, DPU, Tenant, or Request. The subject handed to `nico-correlate` via its ID. DPU is a first-class Entity type because operators sometimes identify incidents by DPU ID before knowing which Host is involved.
  _Avoid_: object, resource, subject
- **Correlation** — entity-scoped aggregation of events and current state from every source, unified into a timeline, scoped to a single Entity ID. What `nico-correlate` produces. Not statistical correlation.
  _Avoid_: investigation, trace, report
- **Diagnosis** — a conservative, pattern-matched hypothesis about root cause, always accompanied by the next commands a human should run to confirm it. Produced by `nico-correlate`. A suggestion, not a verdict.
  _Avoid_: conclusion, result, finding (Finding is nico-doctor's term)
- **Source** — a system that can emit Events or State for a given Entity (e.g. Temporal, Loki, Postgres, k8s, Redfish). Each Source in `nico-correlate` is independently optional and reports unavailable rather than crashing the Correlation. Loki is the primary logs Source (serial console output lives there); k8s log streaming is the fallback. No Datadog.
  _Avoid_: data source, backend, plugin
- **Stuck** — a Workflow that has been in Running status longer than `stuck_threshold` (default 30m, configurable globally in `[temporal]`). A Stuck workflow produces a Finding in the `workflows` Layer.
  _Avoid_: hung, frozen, stalled
- **Event** — a normalized, timestamped, Source-attributed occurrence in a Correlation's Timeline. Raw inputs (Temporal workflow events, k8s Warning events) are mapped into Events by their Source. Use "Temporal event" or "k8s event" when referring to the raw form.
  _Avoid_: entry, record, log line
- **Timeline** — the chronologically sorted sequence of Events in a Correlation, normalized across all Sources. The default human output format for `nico-correlate`.
  _Avoid_: log, history, trace

## What "diagnostic" means here
Read-only. No remediation. Output is human-readable by default and JSON under
`--json`. Exit codes are 0 / 1 / 2 / 3 (see ADR-001).

## Operator's diagnostic ladder
1. Is the cluster healthy?     → `cluster` layer
2. Are pods logging errors?    → `logs` layer
3. Are workflows stuck?        → `workflows` layer
4. Are services healthy?       → `health` layer
5. Is gRPC reachable?          → `grpc` layer
6. Is Postgres pressured?      → `postgres` layer

## Key design choices made:
- RunOpts holds config only (namespace, since, timeout) — no clients. Each layer holds its own Arc<dyn K8sClient>. Keeps
   the runner testable with zero k8s setup.
- todo!() comment in main.rs marks where the real k8s client plugs in — that's issue #5 when the cluster layer gets
  wired to a live cluster.

## Open questions

- **REST access log structure**: does `infra-controller-rest` emit structured JSON access logs with `request_id` and `workflow_id` fields? If yes, build a thin `rest` Source to link `req-` IDs to workflow starts. If no, fall back to grepping Loki logs for `req-` patterns. Check with `kubectl logs -l app=rest | head -5` on a live cluster.

## TUI mode (ADR-007)

`--tui` is an opt-in third output mode (alongside human and `--json`). It is additive — default modes are unchanged. Key decisions:

- **Activation:** `--tui` flag. Hard-errors (exit 3) if stdout is not a TTY. Mutually exclusive with `--json`.
- **Incremental load:** renders immediately with a skeleton; Timeline populates as Sources resolve. Cursor tracks by **event identity** (timestamp + source + kind), not row index, so it follows its event as others are inserted above it.
- **Bottom bar:** four-state source indicators — `⟳` fetching → `●` available → `✗` errored → `○` unavailable/skipped. Also shows Diagnosis, `FOLLOW`/`PAUSED` indicator (tail mode), and `?:help q:quit` hint.
- **Filter (`/`):** substring match against source name OR event detail text. Pane title shows `(12/47)` count. `Escape` clears.
- **Collapse threshold:** below 100 columns the right detail pane hides; `Enter` opens a full-screen overlay instead. Pane title shows `(Enter for detail)`.
- **Auto-follow (`--tail --tui`):** `G`/`End` jumps to last row and re-enables follow. `f` toggles explicitly. `FOLLOW`/`PAUSED` shown in bottom bar.
- **Source errors during tail:** synthetic `source_error` event injected into Timeline (red, kind `source_error`) AND bottom bar flips `●` → `✗`. Timeline is the primary truth surface.
- **`?` overlay:** full keybindings list, always discoverable via `?:help` hint in bottom bar.
- **Right pane empty states:** dim hint `↑↓ to select an event` on startup; `No events match filter` when filter returns zero.
- **nico-doctor `--tui`:** live-refresh dashboard, `--interval` flag + `[output] tui_refresh` in config (default 30s). Details deferred to separate issue.
- **Panic hook:** mandatory — must restore terminal cooked mode before printing panic.
- **Implementation library:** `ratatui` + `crossterm` backend only. No other TUI libraries.

## Out of scope (explicit)
- Remediation actions
- Persistent state (no SQLite, no daemons)
- Web UI or always-on TUI (opt-in `--tui` flag is in scope, see ADR-007)
- Alerting (Datadog already does that)
- Self-update or telemetry
- Mouse support in TUI (deferred)
