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
