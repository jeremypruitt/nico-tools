# ADR-0016: Boot probe renders via ratatui inline viewport

- **Status:** Accepted
- **Date:** 2026-05-10
- **Amends:** ADR-0013 (Layout — TTY rendering: replaces hand-rolled
  `\x1b[F` / `\x1b[J` cursor moves with ratatui's `Viewport::Inline`)

## Context

ADR-0013 established the multi-line boot-probe block and named
`crossterm` cursor moves as the redraw mechanism. The implementation
shipped in `crates/nico-common/src/boot_probe/orchestrate.rs::repaint_tty`
took a shortcut and wrote raw ANSI escapes directly:

```text
for _ in 0..rendered_line_count(&state) {
    sink.write_str("\x1b[F");          // CPL — cursor previous line
}
sink.write_str("\x1b[J");              // clear from cursor to end of screen
sink.write_str(&render_block(...));    // pre-formatted multi-line String
```

`rendered_line_count` counts **logical** lines emitted by the renderer
(one per `\n`). The clear strategy assumes one logical line equals one
physical terminal row. That assumption breaks the moment any rendered
line is wider than the terminal:

- A wrapped header occupies two physical rows; cursor-up moves only
  one. `\x1b[J` clears from the wrong starting row, leaving wrap
  residue at the top.
- Each tick the residue grows. The previously-painted header's tail
  collides with the next paint's start on the same physical row, so
  the user sees a single line repeating horizontally rather than the
  multi-line block re-painting in place.

This is the bug behind the user-reported "mess of a startup" on a
120-column terminal: PRD-001 + PRD-004 grew the header from 114 chars
(`type: auto · ib: unknown`) to 131 chars
(`type: rest-only-mock (auto) · ib: present`), pushing it past the
terminal width.

The same assumption is fragile for several reasons beyond this specific
wrap case:

- Double-width characters (CJK locales, emoji) make logical-char
  width != cell width even without wrapping.
- `SIGWINCH` (terminal resize) during boot does nothing today; the
  renderer keeps using the cached line count.
- Every new status segment we add to the header (next: deployment-mode
  details, postgres URL hints, etc.) re-pays the same risk.

`nico-ops`'s dashboard already uses ratatui (ADR-0010, ADR-0012). The
boot probe is the only surface in the project that hand-rolls terminal
escapes — a one-off renderer with no other consumer of its primitives.

## Decision

The boot probe will paint via ratatui's `Viewport::Inline(N)` against a
`Terminal<CrosstermBackend<io::Stderr>>`. The hand-rolled cursor-move
clear strategy is removed.

**Layout shape from ADR-0013 is preserved** — header, three sections,
bar, success receipt printed on completion, failure card printed on
failure. The change is purely the rendering primitive.

**Render layer becomes ratatui widgets.** `render.rs` is rewritten to
expose `HeaderWidget`, `SectionWidget`, and `BarWidget` (or
equivalents) that implement `ratatui::widgets::Widget`. The current
`render_block(state, mode, frame) -> String` API is removed.

**Tests move to `ratatui::backend::TestBackend`.** Existing string-based
unit tests in `render.rs` (~25 cases asserting glyphs, label
alignment, bar coloring, ascii fallback, etc.) are ported to assert on
rendered cells via TestBackend. This is a like-for-like migration —
the behaviors under test do not change.

**Inline viewport sized per-frame.** `Terminal::draw` is called with a
viewport height equal to the current `rendered_line_count(&state)`.
Ratatui handles the cursor positioning, screen clearing, wrap, and
resize. When the probe finishes, the receipt or card is emitted via
`Terminal::insert_before` so it lands in scrollback exactly as today,
then the terminal is dropped — leaving the user's cursor on a fresh
line below.

**No new dependencies.** `ratatui` and `crossterm` are already in
`nico-common`'s transitive dependency tree (ratatui is a direct dep
of `nico-common` for shared widget types). `crossterm` becomes a
direct dep of `nico-common` so we can construct a backend without
reaching through ratatui re-exports.

**`ProbeMode::NonTty` and `ProbeMode::Json` paths are unchanged.** They
do not paint the live block, so the renderer change does not touch
them. Per-transition log lines and the JSON success/failure documents
keep their current shapes — ADR-0003's output-format contract is
unaffected.

## Consequences

### Positive

- Wrap correctness becomes a property of the rendering primitive, not
  something each future contributor has to remember when adding a
  status segment.
- Free `SIGWINCH` handling: ratatui re-queries terminal size on each
  draw, so resize during boot redraws cleanly at the new width.
- Free double-width / grapheme handling: ratatui uses
  `unicode-width` for cell-width math.
- One way to render in this project, not two. Future contributors
  reading `boot_probe/render.rs` see the same widget patterns as
  `nico-ops/view.rs`.
- ADR-0013's claim of "via `crossterm` cursor moves" stops being
  aspirational — the actual implementation now matches the ADR's
  spirit (the literal phrasing is amended above).
- The renderer becomes composable. If we ever want a "current
  connection state" panel inside the dashboard that mirrors the boot
  probe's section layout, the widgets are reusable. (Not currently
  planned — listed as a free property, not a requirement driving the
  decision.)

### Negative / Trade-offs

- ~25 render unit tests are rewritten against TestBackend. The
  string-substring assertion idiom (`out.contains("connecting")`)
  becomes cell-grid assertion (`backend.assert_buffer(...)`) which is
  more verbose but more precise.
- Adds a hard dependency on `crossterm` for `nico-common`. It is
  already present transitively, so this is essentially formalizing
  what is already there.
- The `ProbeSink` trait abstraction (current testability seam — tests
  pass `Vec<u8>`, prod passes `StderrSink`) becomes less natural; the
  ratatui `Terminal<CrosstermBackend<W: Write>>` swallows the writer.
  Tests instead use ratatui's `TestBackend` which is a different
  abstraction in the same role. Net: no loss of test isolation; the
  shape changes.

## Alternatives Considered

- **(A) Wrap-aware cursor accounting (smallest fix).** Detect terminal
  width, sum `ceil(visible_width / cols)` per rendered line, use that
  physical count for `\x1b[F`. Rejected as the long-term answer because
  it preserves the hand-rolled-escapes architecture and leaves the
  next "logical != physical" trap in place (double-width chars,
  resize, Bidi, etc.). May still ship as a stop-gap PR ahead of the
  ratatui port — that is an implementation-sequencing question, not
  an architectural one.
- **(B) Drop multi-line, switch to a single-line spinner
  (`cargo`-style).** Rejected because ADR-0013 explicitly chose the
  multi-line block for the visibility-while-failing UX and that
  decision has not been revisited.
- **(C) Truncate every rendered line to terminal width.** Rejected
  because it loses content the user wants to see (`ib: present`
  becomes `ib: pre…`), and because it is a renderer-level workaround
  for a primitive-level problem.

## Related

- ADR-0010 — `nico-ops` dashboard architecture (precedent for ratatui
  as the project's terminal-rendering primitive).
- ADR-0012 — Async Component-style TUI event loop for `nico ops`
  (current ratatui usage pattern).
- ADR-0013 — Boot probe (this ADR amends its "Layout — TTY rendering"
  section).
- Implementation lives in `crates/nico-common/src/boot_probe/`
  (`orchestrate.rs`, `render.rs`).
