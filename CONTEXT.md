# nico-tools — Domain Context

## Purpose
Diagnostic CLI for nico/carbide/ncx installations. Read-only. Compresses the
operator diagnostic ladder into seconds. Never hides the underlying tools —
every output line points at where to dig deeper.

## Ubiquitous language

- **nico** (the binary) — single umbrella CLI that dispatches subcommands:
  `nico ops` (live dashboard, see `nico-ops`), `nico doctor` (read-only
  health check), `nico correlate <id>` (entity-scoped event correlation).
  Replaces the historic `nico-doctor` and `nico-correlate` standalone
  binaries. The dispatcher is a thin clap shell — all bootstrap and flow
  logic lives in the library crates (`nico_doctor::run_doctor`,
  `nico_correlate::run_correlate`, `nico_ops::run_ops`). See ADR-009.
- **nico-ops** — the live operational dashboard crate. Currently a
  placeholder (`run_ops()` prints "not yet" and exits 3). Will host the
  forward-looking async Component-style TUI event loop (ADR-012) and
  compose the doctor and correlate library APIs (`prepare_layers`,
  `run_streaming`, `prepare_sources`, `collect_all`) into a single
  multi-pane operator view. The subcommand and crate exist now so the
  workspace builds cleanly while the dashboard is built in subsequent
  slices.
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
- **Layer** (nico-doctor specific) — one of the seven categories the doctor
  checks: cluster, logs, workflows, health, grpc, postgres, dpu. Layers run
  concurrently, are independently skippable, produce Findings, and contribute
  to exit codes 0/1/2.
- **LayerResult** (nico-doctor specific) — what a Layer produces: a name, an
  aggregated `Status`, a `Vec<Check>`, and a measured `duration_ms`. The name
  comes from the Layer itself; the status is derived from the checks via
  `aggregate_status` (worst-case priority: Fail > Warn > Unknown > Ok), unless
  the Layer reports `LayerOutcome::Skipped`, which produces `Status::Skipped`
  independent of any checks.
- **LayerOutcome** (nico-doctor specific) — what a Layer's `collect` method
  returns. Two variants: `Checks(Vec<Check>)` (the layer ran and produced
  findings, possibly empty; the runner aggregates the worst status), or
  `Skipped` (the layer sat out — `--skip` flag or layer not enabled). The
  default `Layer::run` impl maps a `LayerOutcome` to a `LayerResult` and
  handles timing.
- **Pre-flight check** (nico-doctor specific) — a check that runs before
  all Layers as part of the **Boot probe**. If pre-flight fails the tool
  exits immediately with code 3 (can't-run); the diagnostic ladder never
  starts. Pre-flight checks are not skippable. Auth pre-flight runs four
  sub-checks: reachability is a sequential gate; if reachability passes,
  the remaining three (token expiry, namespace exists, list-pods RBAC)
  run in parallel and are **fail-aware** — siblings that are already in
  flight when one fails are allowed to complete, so the user sees all
  diagnostic results in one boot. Each failure message includes the next
  command for the operator to run. Running other checks against an
  unreachable apiserver is vacuous; the reachability gate ensures
  preconditions are met before fan-out. See ADR-0013 for the broader
  Boot probe design.
- **Boot probe** — the unified, multi-line, themed status visualization
  for all bootstrap I/O between `nico` starting and the TUI being entered
  (or the error card printing on failure). Owns three sections in
  topological order: `connecting` (kubeconfig load + reachability gate),
  `validating` (the four pre-flight auth checks), and `serving`
  (per-service port-forwards + postgres reachability). After the
  reachability gate, `validating` and `serving` run concurrently with
  each other, with parallel fail-aware semantics within each section.
  Replaces the previous behavior of a `nico: reach mode: …` line followed
  by a blinking cursor for up to ~20s. See ADR-0013.
- **Finding** — a single warning or failure produced by a layer.
- **Baseline** (nico-doctor specific) — the Layer-level status snapshot persisted from the most recent completed run (`~/.local/share/nico-doctor/last-run.json`). Keyed by layer name; value is the aggregate status (ok / warn / fail / unknown / skipped). Written only on exit codes 0, 1, or 2 (diagnostic ladder completed). Exit code 3 (can't-run) leaves the existing baseline untouched — a failed auth pre-flight must not overwrite a good baseline with an empty record.
- **Delta** (nico-doctor specific) — the per-layer comparison between the current run and the Baseline. Three states: `new` (layer was ok/skipped last run, now warn/fail), `fixed` (layer was warn/fail last run, now ok/skipped), `unchanged`. Computed at Layer granularity, not Finding granularity — dynamic Finding text (pod names, timestamps) makes Finding-level identity brittle. Interaction rule: `--spotlight` hides ok/skipped layers, but layers carrying a `new` or `fixed` Delta badge are always shown regardless of `--spotlight` — delta signal takes priority over spotlight suppression.
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
- **`dpu` layer** (nico-doctor specific) — fleet-wide DPU/HBN roll-up. Five parallel sub-checks (each its own headline Check; layer aggregate = worst-of) reading one bulk SELECT over `machines.network_status_observation` + `machines.network_config` + `machines.dpu_agent_health_report` JSON columns (the actual producer-side storage — see `docs/design/dpu-layer-rewrite.md`): `drift-managed-host`, `drift-instance` (per-axis because the two version axes have different normal-churn profiles — fabric topology vs tenant churn), `cert-fleet`, `quarantine` (always Warn, never Fail — quarantine is sometimes deliberate), `lost-connection` (Fail on > 30m absolute OR > 5% of fleet). Drift detail lines link to `nico doctor hbn <id>` (#205) for per-DPU drill-down. Thresholds configurable in `[dpu]` config block. Encodes the muscle-memory tell from `docs/learning/topics/01-hbn.md`: most weird stuck states are version-drift, not network failure — so the default ladder asks "are any DPUs drifting?" before deeper diagnosis.
- **Event** — a normalized, timestamped, Source-attributed occurrence in a Correlation's Timeline. Raw inputs (Temporal workflow events, k8s Warning events) are mapped into Events by their Source. Use "Temporal event" or "k8s event" when referring to the raw form.
  _Avoid_: entry, record, log line
- **Timeline** — the chronologically sorted sequence of Events in a Correlation, normalized across all Sources. The default human output format for `nico-correlate`.
  _Avoid_: log, history, trace
- **Deployment-type** (nico-doctor specific) — one of three named NICo cluster shapes: `full` (core+rest co-located), `core-only` (carbide-kind), `rest-only-mock` (the rest repo's documented quick-setup with mock-core stand-in). Detected automatically by signal ladder, or set explicitly via `--deployment-type` / `[cluster] deployment_type` / `NICO_DEPLOYMENT_TYPE`. `--deployment-type=force` is the escape hatch — disables detection, type-derived defaults, and conflict warnings; runs with raw config. Distinct from the rest repo's "deployment topology" (co-located vs cloud-hosted), which is an orthogonal axis. See PRD-001.
  _Avoid_: profile, mode, shape, stack
- **Epic** — a master/parent GitHub issue that has child sub-issues. Carries the `epic` label plus the PRD label of the work it heads. Body lists children as a tasklist; children carry `Parent: #<epic>` and the same PRD label.
- **PRD label** — `prd-NNN` (zero-padded, ADR-style numbering). Applied to the Epic AND every child sub-issue, so `gh issue list --label prd-NNN` returns the whole tree. Numbering is allocated at PRD-creation time; the next free number is found via `gh label list | grep '^prd-' | sort -r | head -1`.
- **ADR label** — `adr-NNNN` (4-digit, matching the ADR filename). Applied to issues that touch or amend that ADR. Bidirectional backlink: ADR docs reference issues; issues reference ADRs via this label.
- **SDLC state** — the column an issue or PR sits in on the project board (project 1, `https://github.com/users/jeremypruitt/projects/1`). Five values: `Backlog` (filed, not yet specced), `Ready` (specced + triaged + carries `ready-for-agent` or `ready-for-human`), `In progress` (a draft PR exists), `Validating` (a non-draft PR is open and CI/review is running), `Done` (issue closed or PR merged). Set automatically by `.github/workflows/project-automation.yml` on lifecycle events; manual edits via `gh project item-edit` are allowed but expected to be rare and re-asserted on the next event. The state is the project's primary SDLC oversight surface — every actionable issue and PR must appear on the board.
- **PR↔issue linkage** — every PR with `Closes #N` (or `Fixes`/`Resolves`) drives the linked issue's SDLC state via the project-automation workflow. Required by the repo CI ruleset (`ci.yml` blocks PRs without one). Linked-issue resolution uses GraphQL `closingIssuesReferences`; only those three keywords count, plain `#N` mentions don't. A PR closed *without* merging moves the PR card to `Done` but leaves linked issues' states untouched.

## Umbrella binary layout

- `crates/nico` — thin clap dispatcher binary; the only user-visible
  executable. Subcommands embed `nico_doctor::DoctorArgs` and
  `nico_correlate::CorrelateArgs` directly via clap's `Args` derive.
- `crates/nico-doctor` — library only. Public API: `DoctorArgs`,
  `bootstrap`, `prepare_layers`, `run_once`, `run_streaming`, `run_doctor`.
  No `[[bin]]` section, no `ratatui`/`crossterm` deps. (ADR-011)
- `crates/nico-correlate` — library only. Public API: `CorrelateArgs`,
  `resolve_config`, `prepare_sources`, `collect_all`, `run_correlate`.
  No `[[bin]]` section, no `ratatui`/`crossterm` deps. (ADR-011)
- `crates/nico-ops` — placeholder lib crate, exposes `run_ops()`. The new
  TUI architecture (async Component event loop, ADR-012) will land here.
- `crates/nico-common` — shared config, theme, output, k8s, temporal,
  reach-manager primitives. Unchanged by the umbrella restructure.

`nico ops` is the default subcommand: bare `nico` invokes it. `nico doctor`
and `nico correlate <id>` produce output identical to the historic
`nico-doctor` and `nico-correlate` binaries.

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
7. Are DPUs drifting/healthy?  → `dpu` layer

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
- Persistent state (no embedded database, no daemons, no always-on processes). Exception: a single local cache file (`~/.local/share/nico-doctor/last-run.json`) written by `nico-doctor` to support historical delta badges is in scope. It is written only on exit codes 0/1/2 (diagnostic ladder completed) and read at startup; it is not a database and does not require a running process.
- Web UI or always-on TUI (opt-in `--tui` flag is in scope, see ADR-007)
- Alerting (Datadog already does that)
- Self-update or telemetry
- Mouse support in TUI (deferred)
