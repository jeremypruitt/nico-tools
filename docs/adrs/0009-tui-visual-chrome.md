# ADR-0009: TUI visual chrome — exception to the semantic-only color rule

- **Status:** Accepted
- **Date:** 2026-05-06

## Context

ADR-004 established that color is semantic, not decorative, and explicitly
forbids color for emphasis, branding, or decoration. That rule was written
for one-shot CLI output where every colored token is a status signal an
operator scans during an incident — adding a second colored token (a header
banner, a brand accent) trains operators to ignore the palette and breaks
accessibility.

ADR-007 introduced an opt-in TUI mode. A TUI is a *dwell* surface, not a
*scan* surface — the operator stays in the view, navigating between layers
and findings, rather than reading once and moving on. Persistent chrome
(header surfaces, footer backgrounds, focus highlight, working indicators)
is genuinely useful in this mode for separating data from non-data and
orienting the operator.

ADR-004 as written prohibits this: a flat `overlay_bg` strip behind a brand
tagline is "decoration"; an animated braille spinner whose glyph cycles is
"emphasis"; richer status iconography (`✔/⚠/◯` instead of `✓/!/?`) is
arguably stylistic.

Two coupled changes are needed: a TUI-only carve-out, and a guarantee that
the carve-out cannot drift back into the CLI surface.

## Decision

In TUI mode only (when ADR-007's `--tui` flag is active), color and glyph
variation may additionally be used for:

1. **Chrome surfaces** — header strips, footer backgrounds, focus reverse,
   dimmed hint text. Implemented via existing theme roles (`overlay_bg`,
   `overlay_fg`, `muted`).
2. **Compact tool-identification glyph art** — a logomark of at most three
   rows, in flat color from existing theme roles. No per-cell color
   variation across the glyph.
3. **Functional animation** — working indicators whose cycling glyph (and
   optionally hue, drawn from the existing semantic palette) *is* the
   motion cue. The animation conveys "in progress"; the variation is
   functional, not ornamental.
4. **Structural iconography** — checklist-style state glyphs (`✔/⚠/✗/◯/·`).
   These are upgrades of ADR-004's status icon table, applied uniformly
   across CLI and TUI to avoid divergence.

### Hard limits

- All chrome color must draw from existing theme roles (ADR-008). **No new
  `accent` role.** A future ADR may revisit this; until then, the 7-role
  surface is fixed.
- **No per-cell color interpolation (no gradients).** A gradient across a
  string conveys no information and is the textbook case ADR-004 was
  written to resist. Two-tone block-character glyph art is permitted (each
  cell is one of two flat colors from theme roles); a gradient sweep is
  not.
- **Semantic roles (`ok/warn/error/muted`) retain their exact meanings**
  everywhere. They may not be repurposed for chrome — e.g., a brand strip
  cannot be painted in `theme.ok`, even though it would be theme-coherent.
  The semantic-to-color mapping established by ADR-004 remains the single
  source of meaning for those four colors.
- The carve-out applies to the TUI surface only. CLI output (human and
  JSON) is unchanged; ADR-004 governs it in full.

### Relationship to ADR-004

ADR-004 remains in force. This ADR carves out a narrow exception for TUI
chrome and structural iconography; it does not relax the "color = signal,
not decoration" rule for data rows in either mode, nor for any CLI output.
ADR-004's status stays `Accepted` — the rule is intact; the carve-out is
additive.

## Consequences

### Positive

- Sanctions practice that already exists in the code (the bottom bar's
  `overlay_bg` background) and unblocks the brand strip + spinner +
  iconography polish without re-litigating ADR-004 each time.
- Keeps the carve-out narrow and listable (4 numbered cases, 3 hard
  limits) so future "but what about…" requests can be checked against the
  list.
- No new theme role; the 7-role surface stays fixed.
- CLI surface is untouched — operators reading plain-text output during
  incidents still get the strict ADR-004 contract.

### Negative / Trade-offs

- Two ADRs now govern color (004 and 009). A reader looking up "what color
  rules apply" must read both. Mitigated by cross-links in both files.
- The line between "structural iconography" and "decoration" is a judgment
  call; reviewers will need to apply the hard-limits checklist rather
  than a single-sentence rule.
- Permitting functional animation introduces a render-time cost (one
  redraw per spinner frame). Bounded by ADR-007's existing 100 ms poll
  cadence; no new rendering primitives needed.

## Alternatives Considered

- **Amend ADR-004 in place.** Rejected. The repo convention is that ADRs
  are immutable once accepted (per `docs/adrs/README.md`); changes happen
  via new ADRs. ADR-007 itself superseded a CONTEXT.md/PRD clause this
  way — same precedent.
- **Add an `accent` theme role for branding color.** Rejected. ADR-008
  explicitly considered and dropped an 8th role ("selected") on the
  grounds that the 7-role surface is deliberately small. Adding `accent`
  would re-open that boundary for an aesthetic, not semantic, reason.
- **Permit gradients (per-cell RGB interpolation) on chrome.** Rejected.
  A gradient on a brand string conveys no information beyond what a flat
  fill conveys; it's the precise category ADR-004 was written to resist.
  Two-tone block-character logomarks are sufficient for tool
  identification.
- **TUI-local icon helper diverging from `Status::icon`.** Rejected.
  Same `Status` enum rendering as different glyphs across CLI and TUI is
  the worst of all worlds — two visual languages, no single source of
  truth. The icon upgrade applies uniformly.

## Related

- ADR-003 (output format) — TUI is a third output mode; this ADR governs
  its visual contract.
- ADR-004 (color semantics) — this ADR carves out a TUI exception; ADR-004
  remains in force for CLI output and for data rows in TUI.
- ADR-007 (optional TUI) — establishes the dwell-mode context that
  motivates this carve-out.
- ADR-008 (theme system) — supplies the roles this ADR draws from. Inline
  edit: `overlay_bg` and `overlay_fg` "Used for" column extended from
  "popups" to "popups and chrome surfaces".
