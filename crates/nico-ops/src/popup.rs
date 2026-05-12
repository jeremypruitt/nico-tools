//! Reusable popup primitive for `nico ops`.
//!
//! Every overlay in the dashboard (detail, help, correlate, and the
//! drill-downs PRD-007 will add) shares the same shape: a centered
//! bordered modal with a title, a body, and a small set of keys that
//! dismiss it. This module collects that shape into one [`Popup`] type
//! so the render layer has a single place to evolve it and so the event
//! layer has one helper ([`Popup::dismisses`]) to consult instead of
//! duplicating dismiss-key matching per overlay.
//!
//! Modal-stack semantics live one level up in
//! [`crate::events`]: while a popup is active, the translator routes
//! input through the overlay branch instead of the underlying view's
//! key bindings, so keys cannot leak past the active popup. The
//! key-leak regression test in `events_tests` pins that behaviour.

use crossterm::event::KeyCode;
use nico_common::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

/// Centered bordered modal: title in the top border, body lines inside,
/// dismiss keys consulted by [`Popup::dismisses`].
///
/// Built fresh per frame from the relevant slice of `App` state — the
/// type carries no lifetime so it can hold owned `Line<'static>` body
/// content gathered from snapshots that don't outlive the render call.
pub struct Popup {
    pub title: String,
    pub body: Vec<Line<'static>>,
    pub size_pct: PopupSize,
    pub dismiss_keys: Vec<KeyCode>,
    pub body_margin: Margin,
    pub scroll: u16,
}

/// Percent-of-viewport width × height for the centered modal frame.
#[derive(Clone, Copy)]
pub struct PopupSize {
    pub width_pct: u16,
    pub height_pct: u16,
}

impl Popup {
    /// Centered modal: clears the underlying buffer, paints the border
    /// with `title`, then renders `body` inside the inner area with the
    /// configured margin and scroll offset.
    pub fn render(&self, theme: &Theme, frame: &mut Frame, area: Rect) {
        let inner_area = centered(area, self.size_pct.width_pct, self.size_pct.height_pct);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title.clone())
            .style(Style::default().bg(theme.overlay_bg).fg(theme.overlay_fg));
        let inner = block.inner(inner_area);
        frame.render_widget(Clear, inner_area);
        frame.render_widget(block, inner_area);
        frame.render_widget(
            Paragraph::new(self.body.clone())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            inner.inner(self.body_margin),
        );
    }

    /// Whether the given key dismisses this popup. The event layer
    /// consults this so the dismiss contract lives next to the
    /// primitive rather than scattered across translator branches.
    pub fn dismisses(&self, key: KeyCode) -> bool {
        self.dismiss_keys.contains(&key)
    }
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let h = (area.width * pct_x) / 100;
    let v = (area.height * pct_y) / 100;
    let x = area.x + (area.width.saturating_sub(h)) / 2;
    let y = area.y + (area.height.saturating_sub(v)) / 2;
    Rect {
        x,
        y,
        width: h,
        height: v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nico_common::theme::DEFAULT;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Span;

    fn render_to_string(popup: &Popup, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| popup.render(&DEFAULT, f, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            out.push('\n');
        }
        out
    }

    fn sample_popup() -> Popup {
        Popup {
            title: " hello ".into(),
            body: vec![Line::from(Span::raw("first line".to_string()))],
            size_pct: PopupSize {
                width_pct: 60,
                height_pct: 50,
            },
            dismiss_keys: vec![KeyCode::Esc],
            body_margin: Margin {
                horizontal: 1,
                vertical: 0,
            },
            scroll: 0,
        }
    }

    #[test]
    fn renders_title_and_body() {
        let popup = sample_popup();
        let out = render_to_string(&popup, 80, 20);
        assert!(out.contains("hello"), "title should render:\n{out}");
        assert!(out.contains("first line"), "body should render:\n{out}");
    }

    #[test]
    fn renders_at_multiple_sizes() {
        // Smaller terminal still draws the centered frame.
        let popup = sample_popup();
        let narrow = render_to_string(&popup, 40, 12);
        assert!(narrow.contains("hello"));
        assert!(narrow.contains("first line"));

        // Larger terminal also draws it.
        let wide = render_to_string(&popup, 120, 30);
        assert!(wide.contains("hello"));
        assert!(wide.contains("first line"));
    }

    #[test]
    fn dismisses_returns_true_for_keymap_entry_only() {
        let popup = Popup {
            title: " t ".into(),
            body: vec![],
            size_pct: PopupSize {
                width_pct: 50,
                height_pct: 50,
            },
            dismiss_keys: vec![KeyCode::Esc, KeyCode::Enter],
            body_margin: Margin {
                horizontal: 1,
                vertical: 0,
            },
            scroll: 0,
        };
        assert!(popup.dismisses(KeyCode::Esc));
        assert!(popup.dismisses(KeyCode::Enter));
        assert!(!popup.dismisses(KeyCode::Char('q')));
        assert!(!popup.dismisses(KeyCode::Char('?')));
    }
}
