# nico-tools

Repository: https://github.com/jeremypruitt/nico-tools

## Agent skills

### Issue tracker

Issues live in GitHub Issues. See `docs/agents/issue-tracker.md`.

### Triage labels

Default label vocabulary (needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix). See `docs/agents/triage-labels.md`.

### Priority scoring

Every issue carries a 1-100 priority score in the project board's **Score** number field. Score is set by Claude (autonomous mode of `/priority-score`) at issue creation; project automation derives the band label (`crit`/`top`/`high`/`med`/`low`) and the Priority single-select field automatically when Score changes. Manual edits to label or Priority field stick until next Score change. See `docs/agents/issue-tracker.md` §"Priority scoring".

**When filing any GitHub issue:** run `/priority-score` (autonomous mode), include the one-liner rationale in the body's `## Priority` section between Acceptance criteria and Blocked by, and set the Score project field via `gh api graphql`. Do not write the band label or Priority field directly — let the workflow propagate. Override path: if you judge a score band wrong despite the math, set Score per the math AND write Priority + label directly with a chat callout explaining why; the override sticks until next re-scoring.

### Domain docs

Single-context layout — one `CONTEXT.md` + `docs/adrs/` at the repo root. See `docs/agents/domain.md`.

## Plan Mode

- Make the plan extremely concise. Sacrifice grammar for the sake of concision.
- At the end of each plan, give me a list of unresolved questions to answer, if any.
