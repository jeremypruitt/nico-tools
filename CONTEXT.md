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
- **Axis verdict** (nico-doctor specific) — the shared shape every per-DPU layer reduces its raw observation to: `AxisSummary { axis, status, message, next_command }`. Pure `<axis>_verdict()` functions (one per layer, e.g. `cert_verdict`, `isolation_verdict`, `hbn_verdict`, `ib_verdict`) live in `crates/nico-doctor/src/verdicts/`. Holistic per-DPU and fleet rollups (PRD-003 slices 5 + 6) consume `Vec<AxisSummary>` directly so the rollup never reaches back into per-layer rendering. Each layer's renderer lifts the `AxisSummary` into a single headline `Check` and appends zero-or-more detail rows the punchy headline elides; JSON ordering is headline first, detail after. See ADR-0015. Introduced by PRD-003 Slice 1 (#305) with `dpu_cert` as the first consumer; PRD-003 Slice 2 (#306) added `dpu_isolation`.
- **IB-typed alert** (nico-doctor specific) — an entry in `machines.dpu_agent_health_report`'s `alerts` array whose `id` starts with `Ib` (e.g. `IbPortDown`, `IbCleanupPending`, and any future `Ib*` probe). Owned by the `infiniband` layer and carved out of `dpu_health` per PRD-004 slice 3 (#313), exactly mirroring how BGP-typed alerts (`Bgp*` ids) were carved out of `dpu_health` into `hbn` by PRD-002. Together the two carve-outs partition the agent's alert stream: an `Ib*` alert surfaces in `nico doctor infiniband <id>` as a `Warn` trigger via `ib_verdict`, while every other category (`agent`, `operator`, `sku`, `cert`, `interface`, `other`) stays in `dpu_health`'s detail section. The split is prefix-based rather than enumerated so an upstream-added `Ib*` probe id surfaces automatically in `infiniband` without a code change here. Reads the same column the rest of the agent-alert pipeline reads — only the routing differs.
- **`dpu_cert` layer** (nico-doctor specific) — per-DPU client-certificate drill-down. Reads the `client_certificate_expiry` field (i64 unix epoch secs) out of `machines.network_status_observation` JSON (the producer-side storage; PRD-002). Headline `Check` sourced from the shared **Axis verdict** primitive (`cert_verdict()` → `AxisSummary`, see ADR-0015): four mutually-exclusive verdicts (`expired` > `expiring-soon` > `healthy`, plus `no-recent-status` when no `machines` row or no expiry field). Headline is followed by two cert-specific detail rows when an expiry exists: absolute RFC3339 expiry timestamp + warn-threshold echo. Warn threshold default 30 days, configurable per-invocation via `--warn`. CLI: `nico doctor dpu-cert <machine-id>`. Schema-probes the `machines` table and degrades to `no-recent-status` when absent.
- **`hbn` layer** (nico-doctor specific) — per-DPU HBN drill-down. Reads producer-side state from `machines.network_status_observation` JSON (applied versions, `network_config_error`, `observed_at`), `machines.network_config` JSON (desired managed-host config + quarantine), `machines.inventory` (running HBN component version), `machines.dpu_agent_health_report` JSON (BGP-typed alerts only — other categories live in `dpu_health` per PRD-002), and `instances.network_config_version` (joined on `instances.machine_id`, desired instance axis). Headline pinned to `Fail` and surfaces `network_config_error` verbatim when the agent reported one — explicit error beats "versions disagree" because it tells the operator *why*. Detail bullets cover NVUE/FMDS version minimums, applied-vs-desired drift on the same machine row, BGP alerts, quarantine, and last-seen freshness. CLI: `nico doctor hbn <machine-id>`. Schema-probes `machines` and degrades to `no-recent-status` when absent.
- **`dpu_isolation` layer** (nico-doctor specific) — per-DPU isolation verdict (PRD-002 / issue #260). Distinguishes the four reasons a DPU might have no traffic so the operator does not have to triage them by hand. Reads producer-side `machines` row: `m.id` (presence ⇒ registered), `m.network_config->'quarantine_state'->>'mode'` (desired-side quarantine intent — operator asked for it, not observed effect), and `(m.network_status_observation->>'observed_at')::timestamptz` (presence ⇒ agent has reported at least once; value ⇒ last-seen for freshness). Headline `Check` sourced from the shared **Axis verdict** primitive (`isolation_verdict()` → `AxisSummary`, see ADR-0015): four mutually-exclusive verdicts in precedence order: `not-yet-known` (no `machines` row OR no observation yet) > `quarantine-requested` (desired-side mode set; rendered "quarantine requested: <mode>" — distinct from observed-effect terms) > `lost-connection` (`observed_at` older than freshness threshold, default 90s) > `healthy`. Headline is followed by isolation-specific detail rows: `quarantine-state` (raw mode string when set), `last-seen` (RFC3339 timestamp when present), and `freshness-threshold` echo. CLI: `nico doctor dpu-isolation <machine-id>`. Schema-probes the `machines` table and degrades to `not-yet-known` when absent.
- **`dpu_health` layer** (nico-doctor specific) — per-DPU **holistic per-DPU summary** (PRD-002 / issue #262; PRD-003 slice 5, #309 turned it holistic; PRD-004 slice 4, #314 added `infiniband` as a fifth axis). Emits one headline `Check` per per-DPU axis (`dpu_cert`, `dpu_isolation`, `hbn`, `dpu_services`, `infiniband`, in that order) by calling the corresponding shared verdict (`cert_verdict`, `isolation_verdict`, `hbn_verdict`, `services_verdict`, `ib_verdict`); each headline carries `next_command` pointing at `nico doctor <axis> <id>` so the operator can drill in. The `infiniband` axis is gated by the boot-probe-resolved `infiniband_present` capability (PRD-004 slice 1): `Some(true)` ⇒ `ib_verdict` drives the row; `Some(false)` ⇒ row omitted entirely (n/a-by-design — no IB fabric); `None` ⇒ row renders `Unknown` "presence not detected" so the operator knows the boot probe was skipped (force mode, postgres unreachable, deployment-type unresolved, `rest-only-mock`, or the per-DPU CLI path that doesn't run the boot probe). Layer-specific agent-health detail rows render beneath the per-axis headlines, unchanged in shape from PRD-002 slice 6: (i) alerts grouped by category (`agent`, `operator`, `sku`, `cert`, `interface`, `other`) — BGP-typed and `NetworkConfigError` are owned by `hbn`, every `Ib*` probe id is owned by `infiniband` (PRD-004 slice 3 carve-out, mirror of the BGP→hbn split); (ii) agent-version drift detail ("agent X, superseded since Y") when `agent_version_superseded_at` is set, capped at `Warn`; (iii) per-interface DHCP staleness check at default 4h, configurable via `--dhcp-stale`. Alerts surface as `Fail` details, drift + DHCP staleness as `Warn`. JSON ordering: per-axis headlines first, then agent-health detail. The layer's bulk query reads `machines.network_status_observation` (agent version, cert expiry, network_config_error, applied versions, extension_service_observation, observed_at), `machines.network_config` (desired managed-host version + quarantine_state), `machines.inventory` (HBN component version), `machines.dpu_agent_health_report` (alerts incl. BGP-typed feed for the hbn verdict), `machine_interfaces.last_dhcp`, and `instances.network_config_version` (joined on `instances.machine_id`). CLI: `nico doctor dpu-health <machine-id>`. Schema-probes `machines` and degrades to "no machines row" Unknown headline when absent.
- **`infiniband` layer** (nico-doctor specific) — per-DPU InfiniBand fabric drill-down (PRD-004 / issues #311–#316). Reads `machines.infiniband_status_observation` JSON (`observed_at`, `ufm_observable`, `ports[]`), the producer-side IB observation column populated by core via `update_infiniband_status_observation`. Each port row carries the full GUID (the stable IB-fabric identifier; `pf_guid` deferred per PRD-004), `fabric_id`, `lid`, and `port_state`. Headline `Check` sourced from the shared **Axis verdict** primitive (`ib_verdict()` → `AxisSummary`, see ADR-0015): three mutually-exclusive verdicts in precedence order — `Fail` (any port with empty `fabric_id` OR `lid == 0xffff`) > `Warn` (UFM unobservable OR observation stale > `--stale` default 4h OR any **IB-typed alert** present) > `Ok`. `Unknown` when no observation row. Headline is followed by per-port detail rows (one per port: `guid`, `fabric_id`, `lid=<n>`, `port_state`) and a freshness detail when `observed_at` is set. CLI: `nico doctor infiniband <machine-id>`. Schema-probes the `machines` table and degrades gently when absent. Capability-gated by `infiniband_present` (PRD-004 slice 1) — when `Some(false)` the per-DPU CLI still runs (operators can drill into an isolated IB issue directly) but the fleet `dpu` rollup and the holistic `dpu_health` per-DPU summary both omit the IB axis.
- **`dpu_services` layer** (nico-doctor specific) — per-DPU extension-service inventory drill-down (PRD-002 / issue #263). Reads `machines.network_status_observation->'extension_service_observation'->'extension_service_statuses'` (per-service rows: `service_name`, `version`, `overall_state`, `message`, `removed`) plus the same sub-object's `observed_at` for staleness. Distinct from `dpu_health` because services are structured inventory, not an alert stream — one detail per service preserves the per-row shape. Verdict per `overall_state`: `Failed` / `Error` / unrecognized → `Warn` always; `Pending` / `Unknown` → `Warn` only when the observation is older than `--stale` (default 5m, the same threshold that gates the staleness verdict itself); `Running` → silent; `Terminating` / `Terminated` and the `removed` flag → info-only `Ok` lines. Stale `extension_service_observation->>'observed_at'` also emits a `Warn` detail. CLI: `nico doctor dpu-services <machine-id>`. Schema-probes `machines` and degrades to "no machines row" Unknown headline when absent. Forgedb-gated like `hbn` / `dpu_cert` / `dpu_isolation` — skips cleanly under `rest-only-mock`.
- **`dpu` layer** (nico-doctor specific) — fleet-wide DPU **holistic fleet summary** (PRD-003 slice 6, #310; PRD-004 slice 5, #315 added `infiniband`). Iterates the fleet, calls each per-DPU axis verdict (`cert_verdict`, `isolation_verdict`, `hbn_verdict`, `services_verdict`, `ib_verdict`), and emits one headline `Check` per axis (`dpu_cert`, `dpu_isolation`, `hbn`, `dpu_services`, `infiniband`) with a rollup count + worst-status DPU examples. The `infiniband` axis is omitted entirely when the boot-probe-resolved `infiniband_present` capability gate is `Some(false)` (n/a-by-design — no IB fabric to summarise); `Some(true)` or `None` includes it. Fleet-specific concerns that have not yet been carved into a verdict (`probe-stuck` — Fail when any DPU's `PostConfigCheckWait` agent alert has been in alert state longer than `probe_stuck_grace`) keep their own headlines beneath the per-axis section. Layer aggregate = worst-of across all headlines. Reads one bulk SELECT over `machines.network_status_observation` JSON (applied state, `client_certificate_expiry`, `network_config_error`, `extension_service_observation`) + `machines.infiniband_status_observation` JSON (`observed_at`, `ufm_observable`, `ports`) + `machines.network_config_version` + `machines.network_config` JSON (desired state, including `quarantine_state`) + `machines.inventory` JSON (running HBN component version) + `machines.dpu_agent_health_report` JSON (agent alerts: BGP-typed feeds `hbn`, `Ib*` feeds `infiniband`) + `instances.network_config_version` (joined on `instances.machine_id`). Detail rows are flat: collect every non-Ok per-DPU verdict across all axes, sort Fail > Warn > Unknown (then by axis: cert → isolation → hbn → services → infiniband), cap at `FINDINGS_CAP`. Each detail carries the verdict's `next_command` so the operator can drill in via the per-DPU layer command. JSON ordering: per-axis headlines first (in axis order, with `infiniband` after `dpu_services`), then `probe-stuck` headline, then all detail rows. Thresholds configurable in `[dpu]` config block; the per-axis verdicts use their own thresholds (cert warn 30d, isolation/hbn freshness 90s, services stale 5m, IB observation stale 4h) so the fleet rollup says exactly what the per-DPU drill-down would. The pre-PRD-003 sub-checks (`drift-managed-host`, `drift-instance`, `cert-fleet`, `quarantine`, `lost-connection`) are subsumed by the four axis verdicts; the old fleet-only cap "quarantine is always Warn, never Fail" was removed because `isolation_verdict` reports `Fail` for a quarantine-requested DPU and the rollup mirrors the verdict. See `docs/prds/002-dpu-layer-rewrite.md`, `docs/prds/003-holistic-summary-refactor.md`, and `docs/prds/004-infiniband-layer.md`. Encodes the muscle-memory tell from `docs/learning/topics/01-hbn.md`: most weird stuck states are version-drift, not network failure — so the default ladder asks "are any DPUs drifting?" before deeper diagnosis.
- **Popup primitive** (nico-ops specific) — the shared shape every overlay reduces to: `Popup { title, body, size_pct, dismiss_keys, body_margin, scroll }` in `crates/nico-ops/src/popup.rs`. `Popup::render(theme, frame, area)` centres a bordered modal, clears the underlay, and paints title + body. `Popup::dismisses(key)` answers whether a given key is in the popup's declared keymap so the dismiss contract lives next to the primitive. Modal-stack semantics — keys cannot leak past an active overlay to underlying-view bindings — are enforced one level up in `events::translate` and pinned by the `no_underlying_view_action_leaks_past_active_overlay` regression test. PRD-006 Slice 3 (#369) introduced the primitive and ported the three pre-existing overlays (Detail, Help, Correlate) to it; PRD-007 plugs the correlate mini-dashboard popup into the same primitive without further infra.

- **Holistic summary layer** (nico-doctor specific) — a Layer whose headline `Check`s are entirely sourced from per-DPU axis verdicts produced elsewhere, rather than owning its own per-axis classification logic. Two members: `dpu` (fleet rollup, PRD-003 slice 6, #310) and `dpu_health` per-DPU (slice 5, #309). A holistic layer renders the verdicts and adds layer-specific detail beneath; it never re-derives the verdict signal locally. This keeps "is this DPU OK on axis X?" answered by exactly one function in one place — `<axis>_verdict()` — and the fleet + per-DPU summaries cite that answer verbatim. Fleet-specific concerns not yet on the verdict primitive (`probe-stuck`, future `lost-connection` variants) live as additional non-verdict headlines beneath the per-axis section until they migrate. See ADR-0015.
- **Event** — a normalized, timestamped, Source-attributed occurrence in a Correlation's Timeline. Raw inputs (Temporal workflow events, k8s Warning events) are mapped into Events by their Source. Use "Temporal event" or "k8s event" when referring to the raw form.
  _Avoid_: entry, record, log line
- **Timeline** — the chronologically sorted sequence of Events in a Correlation, normalized across all Sources. The default human output format for `nico-correlate`.
  _Avoid_: log, history, trace
- **Deployment-type** (nico-doctor specific) — one of three named NICo cluster shapes: `full` (core+rest co-located), `core-only` (carbide-kind), `rest-only-mock` (the rest repo's documented quick-setup with mock-core stand-in). Detected automatically by signal ladder (workload → namespace inventory → CRD inventory; first match wins) inside the boot probe's `detect_deployment_type` step, or set explicitly via `--deployment-type` / `[cluster] deployment_type` / `NICO_DEPLOYMENT_TYPE` (precedence: CLI > env > file > auto). The resolved type carries deployment-type-derived defaults (controller namespace, gRPC address, postgres namespace, k8s temporal namespace where `temporal-frontend` runs) plus two feature gates: `forgedb_present` — forgedb-dependent layers (`dpu`, `hbn`, `dpu_cert`, `dpu_isolation`, `dpu_health`, `dpu_services`) consult it to skip cleanly on `rest-only-mock`; and `temporal_present` — the `workflows` layer and `port-forward: workflows` boot-probe step skip on `core-only`, which never deploys Temporal. Two senses of "temporal namespace" are kept distinct: the kubernetes namespace (`cluster.temporal_namespace`, deployment-type-derived) is where the port-forward connects; the Temporal tenancy namespace (`[temporal] namespace`) addresses workflow visibility and is independent. The boot banner surfaces the resolved type as `· type: <name> (<source>)` where `<source>` is one of `auto | flag | config | force`. `--deployment-type=force` is the escape hatch — short-circuits the `detect_deployment_type` step, disables type-derived defaults, and silences override-conflict warnings; runs with raw config. Distinct from the rest repo's "deployment topology" (co-located vs cloud-hosted), which is an orthogonal axis. See PRD-001 and ADR-0013.
  _Avoid_: profile, mode, shape, stack
- **Capabilities object** (nico-doctor specific) — the top-level `capabilities` key in `nico doctor --json` output that exposes the resolved deployment-type's capability bundle to external consumers (issue #242). Stable key set, every key always present once a `Deployment-type` is resolved: five `default_*` capabilities (`cluster_namespace`, `grpc_address`, `postgres_namespace`, `temporal_address`, `temporal_k8s_namespace` — string when the type carries a default, `null` when it doesn't, e.g. `temporal_k8s_namespace` is `null` for `core-only`); two boolean feature gates (`forgedb_present`, `temporal_present`); and `infiniband_present` (boolean or `null` — runtime-detected at boot probe per PRD-004 slice 1, so always `null` here since the deployment-type itself doesn't determine IB presence). Absent from JSON output when no deployment-type is resolved (auto runs that didn't reach the detection ladder, or per-DPU subcommands invoked without `--deployment-type`) — this is the "no regressions in existing JSON-schema snapshot tests for runs without `--deployment-type` set" guarantee. `force` deployment-type renders all five `default_*` capabilities as `null` and both feature gates as `true` per PRD-001 §"Capability vocabulary". Distinct from the in-memory capability bundle (methods on the `DeploymentType` enum) which is a Rust API; the `capabilities` object is the JSON wire shape.
- **`infiniband_present` capability** (nico-doctor specific) — the second runtime-resolved capability flag (parallel to `forgedb_present`), introduced by PRD-004 slice 1. Resolved by the boot probe's `detect_infiniband_present` step (see ADR-0013 amendment): `EXISTS` query against `machines.inventory->'infiniband_interfaces'` ⇒ `Some(true)` when at least one DPU is wired into an IB fabric, `Some(false)` when none are. `None` ⇒ the probe was skipped (force mode, `rest-only-mock`, deployment-type unresolved, or postgres unreachable) or it errored. Consumed by the fleet `dpu` layer and the holistic per-DPU `dpu_health` layer to gate their `infiniband` axis row: `Some(false)` omits the row entirely (n/a-by-design), `Some(true)` renders it via `ib_verdict`, `None` falls back per-layer (`dpu` includes it defensively; `dpu_health` renders an `Unknown` "presence not detected" headline so the operator sees that IB resolution didn't run). Distinct from `forgedb_present`, which is deployment-type-derived (a property of the cluster shape); `infiniband_present` is observation-derived (a property of the fleet's current inventory). The per-DPU `infiniband` layer itself does not consult the gate — operators can drill into an isolated IB issue directly via `nico doctor infiniband <id>` regardless of the fleet-wide flag.
  _Avoid_: capability map, capabilities bundle (when referring to JSON output)
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
- `crates/nico-ops` — interactive ratatui dashboard (~7,900 LOC). Exposes
  `run_ops()` (Layout A scorecard / Layout B Mission Control / Spotlight),
  the `App` reducer + `Action` enum, and the per-refresh `data::collect`
  fan-out. Hosts the `dpu` HBN panel via `nico ops hbn`. ADR-012's async
  Component event loop landed here in earlier slices.
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
