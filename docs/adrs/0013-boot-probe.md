# ADR-0013: Boot probe — multi-line bootstrap progress visualization

- **Status:** Proposed
- **Date:** 2026-05-07
- **Amended:** 2026-05-09 (PRD-001: `detect_deployment_type` step added to `validating` section) — **superseded** by 2026-05-10
- **Amended:** 2026-05-10 (PRD-001 slice 9 / #321: `detect_deployment_type` re-placed as a sequential gate at the end of `connecting`; supersedes the 2026-05-09 placement)

## Context

The pre-TUI bootstrap path (`KubeRsK8sClient::try_new`, the four pre-flight auth
checks, `ReachManager` port-forwards, and the postgres reachability probe) runs
with no progress feedback today. The user sees a single `nico: reach mode: …`
line followed by a blinking cursor for up to ~20s before the first hang's failure
prints. When the cluster is unreachable or credentials are stale, the user is
about to receive bad news; the current absence of feedback amplifies the dread.

This ADR replaces that experience with a visible, themed, multi-line status
block that renders all hanging bootstrap operations as bullets, runs them in
parallel where dependencies allow, and surfaces what worked, what didn't, and
what never had a chance to finish.

This ADR introduces a new domain term and amends prior ADRs:

- New term: **Boot probe** (see CONTEXT.md)
- Amends ADR-004 — adds `accent` (in-progress) and `pending` color/state roles;
  redefines the skipped glyph as `─`
- Amends ADR-006 — within the boot probe, timeouts render as failures (`✗`
  red), not unknowns (`?`). ADR-006's "timeout → unknown" rule remains the
  correct posture for layer checks in `nico-doctor`, where unknown is a valid
  diagnostic outcome
- Amends CONTEXT.md "Pre-flight check" definition to reflect the new fail-aware
  parallel semantics

## Decision

### The boot probe

The **Boot probe** is the unified status visualization for all bootstrap I/O.
It owns the user's screen between the moment `nico` starts and the moment the
TUI is entered (or, on failure, the moment the error card prints).

The boot probe runs three sections in topological order, with parallelism
within each section after a sequential gate:

```
load kubeconfig                [seq, ~0.5s]
  ↓
reach API server               [seq gate, ≤5s]
  ↓
detect deployment-type         [seq gate, ≤5s]   (auto mode only;
  ↓                                              instant-pass otherwise)
fan out (parallel):
  ├ credentials                ≤5s     ┐
  ├ namespace '<ns>' exists    ≤5s     ├ "validating" section
  ├ list-pods permission       ≤5s     ┘
  ├ port-forward: workflows    ≤3s     ┐
  ├ port-forward: grpc         ≤3s     ├ "serving" section
  └ port-forward: postgres     ≤3s ─→ reach postgres ≤2s  ┘
```

After the reachability gate succeeds, `validating` and `serving` sections run
concurrently with each other; within each section all steps run concurrently.
All concurrent Kubernetes API operations go through the ADR-006 8-permit
semaphore (the boot probe's worst-case 7 concurrent kube ops fits comfortably).

### Layout — TTY rendering

Multi-line block on stderr, updated in place via `crossterm` cursor moves:

```
  ◐ booting nico  ·  reach: port-forward (auto)

    connecting
      ✓  load kubeconfig          0.1s
      ⠋  reach API server         0.4s / 5.0s

    validating
      ○  credentials                    5.0s
      ○  namespace 'foo' exists         5.0s
      ○  list-pods permission           5.0s

    serving
      ○  port-forward: workflows        3.0s
      ○  port-forward: grpc             3.0s
      ○  port-forward: postgres         3.0s
      ○  reach postgres                 2.0s

  ▰▱▱▱▱▱▱▱▱  1 / 9 checks
```

#### Bullet vocabulary

| State    | Glyph     | Color    | ASCII fallback | Meaning                              |
|----------|-----------|----------|----------------|--------------------------------------|
| pending  | `○`       | dim gray | `[..]`         | will run, not yet started            |
| active   | `⠋…⠏`     | accent   | `\|/-\\` cycle | currently running                    |
| passed   | `✓`       | green    | `[ok]`         | completed successfully               |
| failed   | `✗`       | red      | `[XX]`         | errored or timed out                 |
| skipped  | `─`       | dim gray | `[--]`         | upstream gate failed; never started  |

Active rows reuse `THROBBER_FRAMES` from `nico-ops/src/app.rs:42`. The `accent`
color role is theme-accent for the boot probe (new role added to ADR-004).
ASCII fallback is engaged under `--ascii`.

#### Per-row format

- **Active row:** full-brightness label, dim budget; shows `elapsed / budget`
- **Passed row:** dim label (recedes), full-color glyph; shows `elapsed`
- **Pending row:** dim everything; shows budget faintly so user can sum the
  worst-case wait at a glance
- **Failed row:** full-brightness label in red

#### Wording

The live block uses plain-English, verb-first labels ("reaching API server",
"checking your credentials", "finding namespace 'foo'", "verifying permissions").
The technical step name (`reachability`, `token_expiry`, etc.) and
`next_command` appear only in the failure card.

#### Bar

Steps-based, not time-based. Each chit reflects its own step's state — not a
global summary: passed → green, failed → red, skipped → dimmed (filled glyph
preserved), active → cyan, pending → dimmed empty. A failure does **not**
cascade visually to other chits; a partial-failure run shows green chits for
the steps that passed alongside a single red chit for the one that failed.

### Concurrency: fail-aware

If any step in a parallel group fails, in-flight peers are **not** cancelled —
they are allowed to complete so the user sees all diagnostic results in one
boot. After the group settles, the section as a whole is considered failed and
all downstream sections render their steps as `─` skipped without ever starting.

This amends the prior CONTEXT.md "Pre-flight check" definition, which described
strict short-circuit semantics.

### Transitions

**On success:** the multi-line block clears. A single one-line receipt remains
scrolled above the TUI:

```
nico: cluster ready (9 checks · 1.6s)
```

The TUI then enters.

**On failure:** the block stays rendered. The failed bullet does a clean
instant glyph swap to `✗` (no animation theatre). The bar's already-completed
cells stay green; the failed cell flips fully red; remaining cells stay dim.
Downstream sections show `─` skipped. The error card prints below:

```
✗ pre-flight failed: credential expired or invalid (HTTP 401)
  step:  token_expiry
  try:   kubectl auth whoami
```

The current `nico: reach mode: …` line (`bootstrap.rs:241`) is folded into the
block header (`◐ booting nico  ·  reach: port-forward (auto)`).

### Degradation

**Non-TTY** (piped stderr, CI logs): no animation, no cursor moves. One log
line per state transition:

```
nico: load kubeconfig: ok (0.1s)
nico: reach API server: ok (0.3s)
nico: credentials: ok (0.4s)
…
nico: cluster ready (9 checks · 1.6s)
```

On failure: same per-event lines, plus per-line `: failed: <message>` /
`: timed out after Xs` / `: skipped`.

**`--no-color` / `NO_COLOR`** (TTY without color): keep the multi-line block,
cursor moves, and animation; rely on glyphs alone to convey state.

**`--ascii`**: replace Unicode glyphs and bar with ASCII equivalents per the
table above. Bar `▰▱` becomes `=` (filled) and `-` (empty) inside `[…]`.
All other behavior unchanged.

**`--json` / `--format json`**: silent during the probe; emit a single
structured document on completion. The success document includes per-step
`elapsed`. The failure document extends the existing
`preflight::format_failure_json` payload with:

- `failed_step` — the step that failed first
- `siblings: [{ step, state, elapsed, message? }, …]` — all parallel
  results in the failed section
- `skipped_steps: [step, …]` — downstream steps that never started

This gives JSON consumers the same diagnostic completeness that the
fail-aware visual provides.

### Per-step timeout budgets

Each step has an explicit, configurable timeout (see issue #1):

| Step                              | Default budget |
|-----------------------------------|----------------|
| load kubeconfig / kube client     | 5s             |
| reach API server                  | 5s             |
| credentials / namespace / RBAC    | 5s each        |
| port-forward (per service)        | 3s             |
| reach postgres                    | 2s             |

Worst-case bound on the boot probe with all parallelism applied: ~10s.

### `detect_deployment_type` step (2026-05-10 amendment, supersedes 2026-05-09)

PRD-001 adds a `detect_deployment_type` step (plain-English label
`"detect deployment-type"`, technical name `detect_deployment_type`) that
resolves the active cluster to one of the three named `DeploymentType`
variants — `full`, `core-only`, `rest-only-mock` — so the rest of
bootstrap (namespace existence check, gRPC address) can use the
deployment-type's defaults instead of failing on a hardcoded namespace
that doesn't exist on the active cluster.

**Placement (re-amended 2026-05-10, slice 9 / #321).** Sequential gate at
the end of the `connecting` section, between `reach API server` and the
`validating` fan-out. The previous 2026-05-09 placement (peer of
`credentials` / `namespace_exists` / `rbac` inside `validating`) is
**superseded**. The reason for the re-placement: detection's result is
the bundle layer of `Config::load`'s precedence chain (`defaults < bundle
< file < env < CLI`), and the `validating` step labels (e.g. `namespace
'<ns>' exists`, `port-forward: grpc → <addr>`) consume the resolved
namespace and gRPC address. Running detection as a peer of those steps
left the resolved type stranded — it appeared in the boot banner but
the labels still rendered the pre-detection (hardcoded) values, so a
`kind-nico-rest-local` cluster booted with `pre-flight failed:
namespace 'forge-system' not found` even though detection had already
identified it as `rest-only-mock` (closure case from slice 6 #283).
Slice 9 makes detection a true sequential gate: bootstrap loads a
minimal "boot config" (just enough for the kube client + reach gate),
runs `connecting` (`load kubeconfig` → `reach API server` → `detect
deployment-type`), then re-calls `Config::load(..., detected_dt)` and
uses the resolved namespace/gRPC values to render `validating` and
`serving`.

**Force mode short-circuits the step.** When the user passed
`--deployment-type=<value>` (CLI), set `[cluster] deployment_type` (config),
or set `NICO_DEPLOYMENT_TYPE` (env) — including `force` — the step passes
instantly without consulting the cluster. Detection only runs in `auto`
mode (the default when no override is specified). `force` follows the
same short-circuit path because PRD-001's hybrid trust model is "trust
the user's intent over detection"; force just means "and skip the
deployment-type-derived defaults too".

**Failure semantics.** Two distinct failure modes, both rendered through
the boot probe's existing `Failed` row treatment (full-brightness label
in red, error card below):

- *Timeout* — the step exceeds `timeouts.preflight` (5s default). The
  detection ladder makes 3 cluster reads in the worst case
  (`Service`, `Namespace`, `CRD` lists), so a 5s budget is generous on a
  responsive cluster and trips when the API server is degraded. Per the
  ADR's general timeout-as-failure rule for the boot probe, this
  surfaces as a failed step with the `try:` line pointing at the
  manual-override escape hatch (`pass --deployment-type=<...> or =force`).
- *No-match-with-diagnostic-data* — the detection ladder
  (workload → namespace inventory → CRD inventory) ran to completion but
  none of the three rungs matched a known shape. The error message
  carries the diagnostic payload — observed namespaces, observed
  services, observed CRDs — so the user can see *why* nothing matched
  before re-running with `--deployment-type=<...>` or `=force`.

In both cases the step's `next_command` is the same recovery hint:
`pass --deployment-type=<full|core-only|rest-only-mock> or =force`.

**Budget.** 5s (`timeouts.preflight`). The step is now sequential rather
than concurrent with the validating fan-out, so the worst-case bound is
~5s longer than the 2026-05-09 placement (concurrency no longer hides
detection latency behind credentials/RBAC). On a responsive cluster the
detection ladder typically resolves in ≤200ms because rung 1 (workload
probe) hits a single Service and short-circuits.

## Consequences

### Positive

- The 20s blinking-cursor hang is replaced by visible, bounded progress
- Users see *all* diagnostic problems in one boot, not just the first
- Sectioned layout doubles as an at-a-glance mental model of nico's
  bootstrap (connecting → validating → serving)
- Hangs become localizable: "stuck in serving" is a different kind of
  problem than "stuck in connecting", visible without reading code
- Failure carries forward more diagnostic context (sibling results,
  skipped steps) into both the human card and the JSON payload
- Theme accent as "in-progress" gives in-flight rows a distinct visual
  identity that animation alone doesn't carry on slow terminals

### Negative / Trade-offs

- More implementation surface: a renderer with a tick task, crossterm
  cursor moves, parallel orchestration, three degradation modes
- Worst-case wait is ~10s instead of strict-short-circuit's first-failure
  exit. On bad days the user waits longer to see *all* failures, but
  re-runs nico fewer times overall
- ADR-004's color/state table grows from 4 roles to 6 (adds `accent` and
  `pending`) and changes the skipped glyph (`·` → `─`)
- Boot probe is a new term in CONTEXT.md and a new module in `nico-common`

## Alternatives Considered

- **Single-line spinner with elapsed counter**: rejected. Four sub-checks
  already exist as a list; a single line can't honestly carry bullets +
  bar + label without becoming busy.
- **Strict fail-fast with sibling cancellation**: rejected. Throws away
  half the diagnostic value of going multi-bullet. Users would re-run
  nico to learn the next layer of bad news — exactly the experience this
  ADR is removing.
- **Time-based progress bar / countdown to timeout**: rejected.
  Visualizing the deadline closing in amplifies dread. Per-row
  `elapsed / budget` text preserves the "how long until this gives up"
  signal without a dread-amplifier visual.
- **`--json` NDJSON event stream**: rejected. JSON consumers want one
  structured outcome per run, not a tail of probe events.
- **Treat timeouts as `unknown` per ADR-006**: rejected within the boot
  probe's scope. Layer checks correctly distinguish timeout-as-unknown
  from error-as-fail, because layer outcomes can be inconclusive. A
  bootstrap probe's timeout is conclusive — the boot can't proceed — so
  it's a failure.

## Related

- ADR-001 (exit codes) — boot probe failure exits with code 3 (cannot-run)
- ADR-004 (color semantics) — amended by this ADR (`accent`, `pending`,
  skipped glyph)
- ADR-005 (reach mode autodetect) — the reach mode display folds into the
  boot probe header
- ADR-006 (concurrency discipline) — amended by this ADR (timeout-as-failure
  scoping for the boot probe)
- ADR-008 (TUI theme system) — TUI-only; the boot probe is pre-TUI plain
  text and uses ADR-004's palette plus the new `accent` role
