# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues. Use the `gh` CLI for all operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering comments by `jq` and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply / remove labels**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close <number> --comment "..."`

Infer the repo from `git remote -v` — `gh` does this automatically when run inside a clone.

## PR conventions

Every PR body **must** include a closing reference so the linked issue auto-closes on merge:

```
Closes #NNN
```

Accepted keywords: `Closes`, `Fixes`, `Resolves` (case-insensitive). The `pr-validation` workflow enforces this — PRs without a valid reference will fail the `validate-pr` check.

Required status check contexts (branch ruleset ID `16012805`):

- **`ci`** — `jobs.ci` in `.github/workflows/ci.yml`. Skipped (reports success) on docs-only PRs.
- **`validate-pr`** — `jobs.validate-pr` in `.github/workflows/pr-validation.yml`. Runs on every PR.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## Labeling conventions

Three label families layer on top of the triage labels (see `triage-labels.md`):

- **`epic`** — applied to a master/parent issue that fans out into child sub-issues. The epic body lists the children as a GitHub tasklist (`- [ ] #N — title`). Children carry `Parent: #<epic>` in their body.
- **`prd-NNN`** — zero-padded, ADR-style. Applied to the epic AND every child sub-issue spawned from it, so `gh issue list --label prd-NNN` returns the whole tree. Allocate the next free number at PRD-creation time: `gh label list | grep '^prd-' | sort -r | head -1`.
- **`adr-NNNN`** — 4-digit, matching the ADR filename (`0013-boot-probe.md` → `adr-0013`). Applied to issues that touch or amend that ADR. Bidirectional backlink: ADR docs reference issues; issues reference ADRs via this label. An issue may carry multiple `adr-*` labels if it amends more than one.

When a new PRD or ADR is created, also create its label with a one-line description:

```
gh label create prd-002 --description "PRD-002: <short title>" --color 1d76db
gh label create adr-0014 --description "Touches ADR-0014 (<short title>)" --color 0e8a16
```

Conventional colors: `5319e7` (epic, purple), `1d76db` (prd-NNN, blue), `0e8a16` (adr-NNNN, green).

A fourth family, the **priority bands** (`crit`/`top`/`high`/`med`/`low`), is described in §"Priority scoring" below — those labels are derived from the Score project field, never set by hand.

## Priority scoring

Every issue carries a 1-100 priority score, tracked across three coupled surfaces. The score drives the board's pickup order and informs triage/cleanup decisions. The math is `(Breadth × Depth) / Cost × 4`, capped at 100; see the `/priority-score` skill for litmus tests.

### The three surfaces

| Surface | Role | Source of truth? |
|---------|------|------------------|
| **Score** project field (number, 1-100) | Sortable on the board; canonical numeric value | **Yes** |
| **Priority** project field (single-select: `crit`/`top`/`high`/`med`/`low`) | Coarse band, visible on board cards | Derived from Score |
| **Label** (one of `crit`/`top`/`high`/`med`/`low`) | Queryable via `gh issue list --label crit` | Derived from Score |
| **`## Priority` section in issue body** | One-liner rationale + b/d/c breakdown | Authored by Claude |

Score → band mapping (matches priority-score skill bands):

| Score | Band | Action |
|------:|------|--------|
| 50-100 | `crit` | Stop what you are doing |
| 25-49  | `top`  | Current or next sprint |
| 13-24  | `high` | Schedule in a future sprint |
| 5-12   | `med`  | Only if nothing better exists |
| 1-4    | `low`  | Backlog or kill |

### Auto-maintenance workflow

`.github/workflows/project-automation.yml` has a `priority` job that fires on `projects_v2_item.edited`. When the Score field changes, it derives the band, updates the Priority single-select, and applies the band label (removing the others). Manual edits to Priority or label are **not reverted** — they stick until the next Score change. This lets a human override the derived band temporarily while keeping Score as the canonical numeric anchor.

Field IDs are baked into the workflow `env:` block (Score, Priority, plus the five Priority option IDs). Re-resolve via the same GraphQL query used for Status field IDs (see §"Project board automation") if the project is rebuilt.

### When scoring happens

- **At issue creation** (autonomous): Claude runs `/priority-score` and writes the Score field. The workflow propagates.
- **During triage** (ratification): the `triage` skill verifies the score's `b/d/c` assumptions still hold and re-scores if they shifted.
- **On context change** (manual): user invokes `/priority-score <issue>` when something material changes — a related issue ships, a deadline appears, an operator cites the gap.
- **Never on a clock.** Drift-by-time is not a refresh signal.

### How priority drives execution

- **Board sort.** The Ready column is sorted by Score descending — top of the column is the next thing to pick up.
- **Triage cleanup.** `low`-band items idle >30 days are candidates for triage to close (`wontfix — score never moved`). No auto-close — triage's call.
- **`crit` callouts.** When Claude scores something into `crit`, it surfaces this explicitly in chat ("scored #N as `crit` — recommend stopping current work").

### Override path

If Claude (or a human) judges the band wrong despite the math, write Score per the math AND directly write Priority field + label to the desired band. Note the override + reason in chat / triage notes. The override sticks until next Score change reverts it. If the override has lasting reasons, the underlying b/d/c judgment should change — re-score instead of leaving a permanent override.

### Label colors

`crit` = `b60205` (red) · `top` = `d93f0b` (orange) · `high` = `fbca04` (yellow) · `med` = `c5def5` (light blue) · `low` = `cccccc` (gray). Hot-to-cold, mirrors the action urgency.

## Dependency tracking between issues

Use these GitHub features in order of preference:

1. **Native sub-issues** (REST API) — the strongest parent-child link; renders in the GitHub UI as a sub-issue card. The `gh` CLI does not expose this directly; use `gh api`:

   ```
   PARENT=<epic-number>
   CHILD_DB_ID=$(gh api repos/:owner/:repo/issues/<child-number> --jq '.id')
   gh api -X POST repos/:owner/:repo/issues/$PARENT/sub_issues -f sub_issue_id=$CHILD_DB_ID
   ```

   Note: `sub_issue_id` is the issue's database ID (`.id`), not the issue *number*.

2. **Tasklist in the epic body** — `- [ ] #N — title`. GitHub renders this as a checklist that auto-tracks the children's open/closed state. Easy to read; weaker than (1) for queries.

3. **`Blocked by: #N` line in a child's body** — used for ordering between siblings. The to-issues skill template already has this slot. Plain markdown; not machine-queryable, but humans read it.

Use (1) for parent-child relationships within a PRD. Use (3) for ordering constraints between sibling sub-issues. (2) is a nice-to-have summary view at the top of the epic body.

## PRD format and lifecycle

PRDs are forward-looking specs. The canonical doc lives in `docs/prds/NNN-slug.md` (zero-padded numbering, parallel to `docs/adrs/`). The doc is the source of truth for the spec; an Epic GitHub issue (label `epic` + `prd-NNN`) tracks implementation progress.

**Every PRD ships as an Epic with native sub-issues.** Before any implementation begins, run `/to-issues` against the PRD to spawn one issue per vertical slice, each linked to the Epic via the native sub-issue API (§"Dependency tracking" → step 1) AND carrying `Parent: #<epic>` in its body. This is non-negotiable because:

1. The GitHub UI renders an `N/M completed` progress badge on the Epic only when children are linked as native sub-issues — markdown checklists don't count. That badge is the primary at-a-glance signal of how a PRD is progressing.
2. The `prd-NNN` label query (`gh issue list --label prd-NNN`) returns the whole tree (epic + children), letting the triage flow operate on a PRD as a unit.
3. Slices that surface as concrete issues are graspable independently (`ready-for-agent`, picked up AFK, merged); placeholder checklists are not.

A PRD whose epic body has only markdown checklist placeholders is in an unfinished state, even if the PRD doc itself is complete. Run `/to-issues` to finish it.

### File layout

- `docs/prds/` — forward-looking PRDs awaiting or in implementation. One file per PRD: `001-deployment-type.md`, `002-dpu-layer-rewrite.md`, etc.
- `docs/design/` — historical design references; baseline / foundational designs that have already shipped (e.g., `nico-doctor-and-correlate.md`). Distinct from PRDs because they're not forward-looking specs.
- `docs/adrs/` — narrow architectural decisions. PRDs may reference or amend ADRs; ADRs do not depend on PRDs.

### PRD doc structure

Required sections at the top of every `docs/prds/NNN-slug.md`:

```
# PRD-NNN — <title>

- **Status:** <Specced | In progress | Done | Superseded>
- **Epic:** #<issue-number> (carries `prd-NNN` label)
- **Touches:** ADR-XXXX (if applicable)
- **Deferred follow-up:** #<issue-number> (if any)
```

Then: Problem · Personas · Goals · Non-goals · High-level design · UX · Open questions. Implementation breakdown goes in the Epic, not the doc — it changes during implementation and is better tracked as a tasklist of issues.

### Allocating a new PRD number

Find the next free number:

```
ls docs/prds/ | grep -oE '^[0-9]+' | sort -n | tail -1
```

Create the matching label at the same time:

```
gh label create prd-NNN --description "PRD-NNN: <short title>" --color 1d76db
```

### Epic ↔ PRD doc coupling

The Epic issue's body is a thin shell:

- One-paragraph summary
- Link to `docs/prds/NNN-slug.md`
- Children tasklist (`- [ ] #N — title` with `Parent: #<epic>` set on each child via `gh api` per §"Dependency tracking")
- Related issues / pre-existing bugs / upstream deps

The Epic body can be edited freely as implementation progresses. The PRD doc is more stable — major spec changes warrant a new commit (and possibly a new PRD if the change is substantive enough).

**Reference epic to imitate:** PRD-002's epic (#253) is the canonical example of a fully-fleshed Epic — children #257-#265 are filed and linked, the body lists them by issue number, and the GitHub UI shows an `N/9 completed` progress badge. PRD-001's epic (#245) was brought into compliance retroactively on 2026-05-09 via `/to-issues`; new PRDs should reach this shape *before* any implementation slice is picked up.

## Project board automation

Every actionable issue and PR appears on project 1 (`https://github.com/users/jeremypruitt/projects/1`). The board's `Status` column is the project's primary SDLC oversight surface — *cards must move through it as work happens*. The `.github/workflows/project-automation.yml` workflow drives this automatically; manual `gh project item-edit` is the escape hatch.

### SDLC states

| State         | Meaning                                                              |
|---------------|----------------------------------------------------------------------|
| `Backlog`     | Filed; not yet specced or triaged                                    |
| `Ready`       | Triaged + specced; carries `ready-for-agent` or `ready-for-human`    |
| `In progress` | A draft PR referencing the issue exists                              |
| `Validating`  | A non-draft PR is open; CI / review running                          |
| `Done`        | Issue closed or PR merged                                            |

### Transition table

| Event                                            | Resulting state                                                  |
|--------------------------------------------------|------------------------------------------------------------------|
| `issues.opened` / `issues.reopened`              | `Backlog` (issue added to board)                                 |
| `issues.labeled` with `ready-for-agent`/`-human` | `Ready`                                                          |
| `issues.closed`                                  | `Done`                                                           |
| `pull_request.opened` (draft)                    | linked issues + PR → `In progress` (PR added to board)           |
| `pull_request.opened` (non-draft)                | linked issues + PR → `Validating`                                |
| `pull_request.ready_for_review`                  | linked issues + PR → `Validating`                                |
| `pull_request.converted_to_draft`                | linked issues + PR → `In progress`                               |
| `pull_request.closed` (merged)                   | linked issues + PR → `Done`                                      |
| `pull_request.closed` (closed without merge)     | PR → `Done`; linked issues untouched                             |

Linked-issue resolution uses GraphQL `closingIssuesReferences` — only `Closes #N` / `Fixes` / `Resolves` count, plain `#N` mentions don't. A PR with multiple closing references drives all linked issues. The CI workflow (`ci.yml`) already enforces that every PR contains a closing reference.

### Required setup (`PROJECT_PAT` secret)

The default `GITHUB_TOKEN` lacks project scope, so the workflow needs a personal access token. Use a **classic PAT** — fine-grained PATs do *not* support user-owned ProjectsV2 (the "Projects" account-permission entry only renders for org-owned projects). Until/unless project 1 is moved to an org, classic is the only working path.

1. Open the classic-token page (NOT `?type=beta`): `https://github.com/settings/tokens/new`
2. Verify the page header reads "New personal access token (classic)".
3. Scopes — check exactly:
   - `repo` (auto-selects the sub-scopes)
   - `project` (auto-selects `read:project` + `write:project`)
4. Set an expiration (90 days is typical) and generate. Copy the token — it's shown once.
5. Save as the repo secret:
   ```
   gh secret set PROJECT_PAT --repo jeremypruitt/nico-tools
   ```
   (paste the token when prompted)
6. Rotation: classic PATs expire. When `PROJECT_PAT` expires, the workflow's `sync` job fails with `401 Bad credentials` (or empty `GH_TOKEN` if the secret is missing). Regenerate and re-save.

### Hardcoded IDs in the workflow

The workflow embeds the project ID, the `Status` field ID, and the five option IDs as `env:` constants. They were resolved once at workflow-creation time:

```bash
gh project field-list 1 --owner jeremypruitt --format json \
  | jq '.fields[] | select(.name == "Status") | .options'
```

If the project is rebuilt or the `Status` field is replaced, re-resolve and update the constants. Adding a new state (e.g., `Blocked`) is similarly a workflow edit — append the option ID as a new `STATUS_*` env var and add the case in the `Determine target status from event` step.

### Manual override

When the workflow misclassifies (rare; usually a triage label edge case), correct manually:

```bash
PROJECT_ID=PVT_kwHOAAE0oM4BXFC0
STATUS_FIELD=PVTSSF_lAHOAAE0oM4BXFC0zhSUcro
ITEM_ID=$(gh project item-list 1 --owner jeremypruitt --format json --limit 100 \
  | jq -r '.items[] | select(.content.number == <issue-or-pr-number>) | .id')
# Status option IDs: a18af1e3=Backlog, 0855fb61=Ready, 7c242164=In progress,
#                    f5779989=Validating, a8a3d908=Done
gh project item-edit --project-id $PROJECT_ID --id $ITEM_ID \
  --field-id $STATUS_FIELD --single-select-option-id <option-id>
```

The next lifecycle event for that card will re-assert the workflow's view, so manual overrides are not durable across state-changing events. That's by design.
