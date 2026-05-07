# Architecture Decision Records

This directory holds [Architecture Decision Records](https://adr.github.io/)
in MADR-lite format. One file per decision. ADRs are immutable once accepted
— if a decision changes, write a new ADR that supersedes the old one and
update the old one's status.

## Format

Use `0000-template.md` as the starting point. File names are
`NNNN-kebab-case-title.md`, four-digit zero-padded.

## Conventions

- **Status: Accepted** — decision is in effect.
- **Status: Accepted (locked)** — decision is in effect and reopening it
  requires explicit threat-model or design review. Used for ADR-002.
- **Status: Superseded by ADR-NNNN** — keep the file; readers may follow the
  link.
- **Status: Deprecated** — decision was rolled back without a replacement;
  document why in a Consequences update.

## How agents use these

The Pocock skill set (`/tdd`, `/diagnose`,
`/improve-codebase-architecture`, etc.) is configured via
`docs/agents/domain.md` to read this directory before exploring the codebase.
If an agent's proposal contradicts an ADR, it must surface the conflict
explicitly rather than silently overriding.

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-exit-code-semantics.md) | Exit code semantics | Accepted |
| [0002](0002-read-only-by-design.md) | Read-only by design | Accepted (locked) |
| [0003](0003-output-format-contract.md) | Output format — human-first, JSON-stable | Accepted |
| [0004](0004-color-semantics.md) | Color is semantic, not decorative | Accepted (amended by 0013) |
| [0005](0005-reach-mode-autodetect.md) | Reach mode — auto-detect port-forward vs. in-cluster | Accepted |
| [0006](0006-concurrency-discipline.md) | Concurrency — bounded parallelism, layered timeouts | Accepted (amended by 0013) |
| [0007](0007-optional-tui.md) | Optional `--tui` mode (now scoped to `nico ops`) | Accepted |
| [0008](0008-tui-theme-system.md) | TUI theme system | Accepted |
| [0009](0009-umbrella-binary.md) | Umbrella binary `nico` with clap subcommand dispatch | Accepted |
| [0010](0010-nico-ops-dashboard-architecture.md) | `nico-ops` dashboard architecture (Layout A) | Accepted |
| [0011](0011-strip-tui-from-text-subcommands.md) | Strip TUI from text-only subcommands | Accepted |
| [0012](0012-async-component-tui-event-loop.md) | Async Component-style TUI event loop for `nico ops` | Proposed |
| [0013](0013-boot-probe.md) | Boot probe — multi-line bootstrap progress visualization | Proposed |
