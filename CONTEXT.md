# nico-doctor — Domain Context

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

## Decisions made:
- Workspace: convert nico-doctor to nico-tools monorepo with nico-common, nico-doctor, nico-correlate
- Log source: Loki primary, k8s streaming fallback, no Datadog
- Stuck threshold: globally configurable (stuck_threshold in [temporal]), default 30m
- Timeline filtering: recency + severity first, Diagnosis-scoped filtering as the goal
- JSON schema: additive-only is safe, everything else is breaking (ADR-0001)
- Read-only constraint: no remediation actions, ever (ADR-0002)
- DPU: first-class Entity type alongside Host, Workflow, Tenant, Request
- REST Source: deferred — check log structure empirically before deciding

## Open questions

- **REST access log structure**: does `infra-controller-rest` emit structured JSON access logs with `request_id` and `workflow_id` fields? If yes, build a thin `rest` Source to link `req-` IDs to workflow starts. If no, fall back to grepping Loki logs for `req-` patterns. Check with `kubectl logs -l app=rest | head -5` on a live cluster.

## Out of scope (explicit)
- Remediation actions
- Persistent state (no SQLite, no daemons)
- Web UI / TUI
- Alerting (Datadog already does that)
- Self-update or telemetry
