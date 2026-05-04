# ADR-004: Color is semantic, not decorative

- **Status:** Accepted
- **Date:** 2026-05-03

## Context

Colored output helps operators scan results quickly during incidents. Misused
color (decorative emphasis, branding) actively hurts — it trains operators to
ignore it and breaks accessibility for color-blind users and non-TTY contexts.

## Decision

Colors carry exactly one meaning each:

| Color | Meaning |
|-------|---------|
| Green | OK |
| Yellow | Warning (degraded but functional) |
| Red | Failure (broken) |
| Gray (dim) | Unknown / not-checked / skipped |

No other colors. No bold or underline as a substitute for color. Color is
never used for emphasis, branding, or decoration.

Color is automatically disabled when:
- `NO_COLOR` environment variable is set (any value), per
  https://no-color.org
- `--no-color` flag is passed
- Output is not a TTY (piping to a file, CI logs)
- `--ascii` is passed (also replaces Unicode status icons with ASCII)

The `--color` flag overrides auto-detection: `always` forces color even on
non-TTY, `auto` (default) is the auto-detection above, `never` is equivalent
to `--no-color`.

Status icons (Unicode, with ASCII fallback under `--ascii`):

| Icon | ASCII | Meaning | Color |
|------|-------|---------|-------|
| `✓` | `[ok]` | ok | green |
| `!` | `[!!]` | warn | yellow |
| `✗` | `[XX]` | fail | red |
| `?` | `[??]` | unknown | gray |
| `·` | `[--]` | skipped | dim |

## Consequences

### Positive
- Output stays scannable during real incidents.
- Accessibility-friendly out of the box.
- CI logs are clean by default (no escape codes).
- `NO_COLOR` compliance is table stakes for serious CLI tools.

### Negative / Trade-offs
- Tempting to bend the rules ("just one bold for the header") — must be
  resisted in code review.
- Slight implementation overhead for the auto-detection logic. Use
  `owo-colors` or `nu-ansi-term` which handle this for us.

## Alternatives Considered

- **256-color palette / theming:** rejected. More color = less signal. Four
  states is already the maximum useful palette.
- **Color-as-emphasis (e.g., bold cyan for headers):** rejected per the
  semantic-only rule.

## Related

- ADR-003 (output format) — color is part of the human format only.
