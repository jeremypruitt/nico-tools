# Contributing

## Pull requests

### Required status checks

The branch ruleset (ID `16012805`) requires the following checks to pass before any PR can merge to `main`:

- **`ci`** — corresponds to `jobs.ci` in `.github/workflows/ci.yml`. Runs `cargo test` and `cargo clippy`. Skipped (and reports as success) when a PR only touches `docs/**` or `**/*.md`. If you rename the job, update the ruleset's required status check context to match.
- **`validate-pr`** — corresponds to `jobs.validate-pr` in `.github/workflows/pr-validation.yml`. Runs on every PR (including docs-only) and enforces the closing-issue reference in the PR body.

The ruleset also enforces `strict_required_status_checks_policy: true`, meaning the PR branch must be up to date with `main` before merge. A CI run from before a rebase does **not** satisfy the requirement — you must push the rebased branch and wait for the new run. `gh pr merge <num> --squash --auto --delete-branch` queues the merge so you don't have to babysit the rerun.

### Closing issues

Every PR body **must** contain a closing keyword that links to the issue it resolves:

```
Closes #NNN
```

Accepted keywords: `Closes`, `Fixes`, `Resolves` (case-insensitive). The `validate-pr` job enforces this automatically — PRs without a valid closing reference will fail the `validate-pr` check.

On merge to `main`, GitHub automatically closes the referenced issue.

### Commit messages

Follow the conventional-commits style used in this repo: `type(scope): short description`. See recent commits for examples.
