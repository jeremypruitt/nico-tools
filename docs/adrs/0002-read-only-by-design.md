# ADR-002: Read-only by design

- **Status:** Accepted (locked)
- **Date:** 2026-05-03

## Context

`nico-doctor` and `nico-correlate` are diagnostic tools. They run against
production environments where any write action could compound an existing
incident. There will be persistent pressure (from users, from agents, from
future-us) to add convenience features like `--fix`, `--restart-pod`, or
"auto-clear stuck workflows."

## Decision

Both tools are **read-only**. They never:

- Mutate Kubernetes resources (no patches, no deletes, no creates).
- Execute Temporal signals, terminations, resets, or workflow starts.
- Write to Postgres (no UPDATE, no DELETE, no INSERT — only SELECT).
- Call any Redfish action endpoint that has side effects.
- Modify local files outside their own structured output and stderr logs.

When the tools find a problem, they print the next command for a human to run
themselves. The tool suggests; the human decides.

This decision is **locked**. Reopening it requires a separate ADR that
explicitly supersedes this one and a threat-model review. "Just a small fix"
features should be rejected on sight.

## Consequences

### Positive
- No security or audit burden. The tools require read-only credentials only.
- Operators can run them in production without approval gates.
- No risk that an agent (Claude Code, sandcastle, CI) compounds an incident.
- Clear separation between investigation and remediation.

### Negative / Trade-offs
- Some incidents will require a follow-up command the tool could have run
  automatically.
- More verbose output (printing next-commands instead of executing them).

## Alternatives Considered

- **Read-only by default with `--fix` opt-in:** rejected. Once the code path
  exists, every incident is a temptation. The hard line is the safer line.
- **Separate `nico-fix` tool:** deferred. May make sense someday, but it's a
  different security and threat model and should not share a binary with the
  diagnostic tools.

## Related

- Any future Claude Code session, sandcastle run, or human contributor that
  proposes a `--fix`, `--restart`, `--terminate`, `--clear`, `--reset`, or
  similar flag should be redirected here before continuing.
