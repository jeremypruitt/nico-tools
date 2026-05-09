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

Accepted keywords: `Closes`, `Fixes`, `Resolves` (case-insensitive). The CI workflow enforces this — PRs without a valid reference will fail the `ci` check.

The required CI status check context is **`ci`** (matches `jobs.ci` in `.github/workflows/ci.yml`). The branch ruleset ID is `16012805`.

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

The default `GITHUB_TOKEN` lacks project scope, so the workflow needs a fine-grained PAT:

1. Generate a fine-grained PAT at `https://github.com/settings/tokens?type=beta` scoped to *only* this repo, with:
   - **Repository permissions:** `Contents: Read`, `Issues: Read`, `Pull requests: Read`
   - **Account permissions:** `Projects: Read and write` (required to mutate ProjectV2 items)
2. Save it as the repo secret `PROJECT_PAT` (`gh secret set PROJECT_PAT --repo jeremypruitt/nico-tools`).
3. Rotation: PATs expire. When `PROJECT_PAT` expires, the workflow steps fail with `401 Bad credentials` — regenerate and re-save.

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
