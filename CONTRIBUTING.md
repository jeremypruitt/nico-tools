# Contributing

## Pull requests

### Required CI check

The branch ruleset (ID `16012805`) requires a passing **`ci`** status check before any PR can merge to `main`. This check corresponds to the `jobs.ci` job in `.github/workflows/ci.yml`. If you rename that job, update the ruleset's required status check context to match.

The ruleset also enforces `strict_required_status_checks_policy: true`, meaning the PR branch must be up to date with `main` before merge. A CI run from before a rebase does **not** satisfy the requirement — you must push the rebased branch and wait for the new run.

### Closing issues

Every PR body **must** contain a closing keyword that links to the issue it resolves:

```
Closes #NNN
```

Accepted keywords: `Closes`, `Fixes`, `Resolves` (case-insensitive). The CI job enforces this automatically — PRs without a valid closing reference will fail the `ci` check.

On merge to `main`, GitHub automatically closes the referenced issue.

### Commit messages

Follow the conventional-commits style used in this repo: `type(scope): short description`. See recent commits for examples.

### Editing `.github/workflows/claude-code-review.yml`

The `claude-code-review` GitHub App enforces that the workflow file on the PR branch is byte-identical to the version on `main`. A PR that modifies that file will fail OIDC token exchange with `401 Workflow validation failed` and `claude-review` will go red. This is expected. Merge the workflow change first, then validate the new behavior on a follow-up PR that does not touch the workflow file.
