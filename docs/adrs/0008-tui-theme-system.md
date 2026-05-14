# ADR-0008: TUI Theme System

- Status: Accepted
- Date: 2026-05-05

## Context

Both nico-doctor and nico-correlate have TUI dashboards rendered with ratatui. All colors are currently hardcoded in render functions — `Color::DarkGray`, named terminal colors for status indicators, and ad-hoc styles per widget. There is no mechanism to customize or theme the output.

ADR-004 established semantic color roles for plain-text output (ok/warn/fail/unknown via owo-colors). The TUI needs an equivalent system that:

- Expresses the same semantics consistently across both TUIs
- Allows user customization via popular terminal color schemes (Dracula, Nord, Gruvbox)
- Ships a curated default that looks great out of the box
- Remains togglable by the existing `--no-color` / `NO_COLOR` mechanism

## Decision

Introduce a `Theme` struct in nico-common with 7 semantic color roles, each defined as `Color::Rgb(r, g, b)`. Ship 4 built-in themes. Expose theme selection via `--theme <name>` CLI flag and `NICO_THEME` env var on both binaries. Add a `theme` field to `TuiContext` in each TUI crate. All render functions read colors from `ctx.theme` instead of hardcoding them.

### Semantic color roles (7 total)

| Role | Used for |
|------|----------|
| `ok` | success, available, FOLLOW indicator |
| `warn` | warning, fetching, PAUSED indicator |
| `error` | failure, errored source |
| `muted` | unknown, skipped, unavailable, dimmed hint text |
| `overlay_bg` | background of help and detail popups |
| `overlay_fg` | primary text inside popups |
| `overlay_key` | keybinding labels in help popup |

### Built-in themes

- `default` — new curated dark theme (replaces current grey/white palette; must look great out of the box)
- `dracula`
- `nord`
- `gruvbox`

### Color values

Every role in every theme uses `Color::Rgb(r, g, b)`. No named ratatui terminal colors (e.g. `Color::DarkGray`, `Color::Green`) anywhere in theme definitions.

### Modifiers

Text modifiers are hardcoded per-element in render code and are **not** part of the theme struct:

- `Modifier::REVERSED` — selected row highlight
- `Modifier::DIM` — hint text, overlay key labels
- `Modifier::BOLD` — status indicators (RUNNING, FOLLOW/PAUSED)

### Theme selection precedence

1. `--theme <name>` CLI flag (both binaries)
2. `NICO_THEME` env var as fallback
3. `default` theme
4. `--no-color` / `NO_COLOR` override: theme is ignored entirely; all render functions use `Style::default()`

Invalid theme name → hard fail at startup with an error message listing all valid theme names.

## Consequences

### Positive

- Consistent semantic palette shared across both TUIs
- Popular terminal themes supported out of the box
- nico-common owns all theme definitions — no duplication between crates
- Rgb-only values render identically regardless of terminal palette overrides
- Existing `--no-color` contract (ADR-004) is fully preserved

### Negative / Trade-offs

- Rgb colors may look unexpected on terminals with very low color depth (rare in modern emulators)
- Adding `theme` to `TuiContext` is a breaking change to both TUI crates — all call sites must be updated
- 7 roles is a deliberately small surface; adding roles later requires updating all theme definitions

## Alternatives Considered

- **Named terminal colors only**: rejected — named colors (e.g. `Color::Green`) render differently across emulators and user palette overrides; Rgb gives predictable, tested output.
- **Modifiers in theme struct**: rejected — modifiers express element semantics (bold = important indicator), not aesthetic preference; keeping them in render code avoids combinatorial theme complexity.
- **`selected` as an 8th role**: discussed and dropped — selected row highlight uses `Modifier::REVERSED` on the existing cell style, which is theme-agnostic.
- **Auto-detecting terminal background (dark vs light)**: deferred to a follow-up issue; graceful degradation is complex enough to warrant its own design pass.

## Amendment — Container vs. plain block split (issue #370, 2026-05-13)

PRD-006 Slice 4 introduces a two-method block factory on `Theme` so render call sites can express *intent* — "this is an outermost frame" vs. "this is an inner widget" — rather than each site copying `Borders::ALL` flags inline.

- `Theme::container_block()` returns a `Block` with `Borders::ALL`. Callers add titles and styles on top. Used by the outermost view containers only: per-cell Scorecard frame, per-card Spotlight frame, logs overlay frame, popup frame.
- `Theme::plain_block()` returns a `Block` with no borders. Callers may still attach a title (rendered on the first row of the area). Used by every inner widget: Scorecard header, drill-panel findings list, the empty-grid `layers` placeholder, the Spotlight `no incidents` placeholder.

Both methods take `&self` so the split can later evolve to apply theme-specific defaults (e.g., bordered with a themed border colour) without churning call sites.

The amendment also documents a bottom-bar pixel that pairs with the split: every layout (Scorecard + Spotlight) now carries a one-row severity legend immediately above the hint bar. `severity_legend_line(theme, width)` is the pure primitive: at width ≥ 60 it pairs each glyph with its `Fail`/`Warn`/`OK`/`Unknown` label; at width < 60 it collapses to glyphs-only so the row still fits. The legend is read-only — no interaction — and is rendered through the same `theme_color()` mapping used by every other glyph in the dashboard so the palette stays consistent.

## Related

- ADR-004: Color semantics (plain-text output color roles)
- ADR-007: Optional TUI (ratatui decision)
