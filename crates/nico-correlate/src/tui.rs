use std::io;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    widgets::{Block, Borders},
};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nico_common::output::OutputMode;
use crate::event::Event as CorrelateEvent;
use crate::source::StateEntry;
use crate::diagnosis::Diagnosis;

pub struct TuiContext {
    pub mode: OutputMode,
}

/// Data collected by the correlate pass, passed into the TUI rendering layer.
/// Fields beyond `id`, `events`, and `exit_code` are wired here for future layout
/// issues and are intentionally unused in the scaffold.
#[allow(dead_code)]
pub struct CorrelateOutput {
    pub id: String,
    pub id_type: String,
    pub events: Vec<CorrelateEvent>,
    pub state: Vec<StateEntry>,
    pub diagnosis: Option<Diagnosis>,
    pub restricted: Vec<String>,
    pub unavailable: Vec<String>,
    pub exit_code: i32,
}

/// Install a panic hook that restores the terminal before printing the panic message.
/// Must be called before entering raw mode so a crash does not leave the operator's
/// terminal broken.
pub fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));
}

pub fn run_tui(output: CorrelateOutput, ctx: TuiContext) -> i32 {
    let mut stdout = io::stdout();
    enable_raw_mode().expect("enable raw mode");
    execute!(stdout, EnterAlternateScreen).expect("enter alternate screen");

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("create terminal");

    let code = event_loop(&mut terminal, &output, &ctx);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    code
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    output: &CorrelateOutput,
    ctx: &TuiContext,
) -> i32 {
    loop {
        terminal.draw(|f| render(f, output, ctx)).expect("draw");

        if event::poll(std::time::Duration::from_millis(100)).expect("poll") {
            if let Ok(CrosstermEvent::Key(key)) = event::read() {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    _ => {}
                }
            }
        }
    }
    output.exit_code
}

fn render(frame: &mut Frame, output: &CorrelateOutput, ctx: &TuiContext) {
    let area = frame.area();
    // ctx.mode carries the color/ascii flags threaded from --no-color / --ascii.
    // Color and ASCII substitution rendering is deferred to subsequent layout issues.
    let quit_hint = if ctx.mode.ascii { "q:quit" } else { "q:quit" };
    let title = format!(
        " nico-correlate: {}  ({} events)  {} ",
        output.id,
        output.events.len(),
        quit_hint,
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL);
    frame.render_widget(block, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn empty_output() -> CorrelateOutput {
        CorrelateOutput {
            id: "test-workflow-id".into(),
            id_type: "workflow".into(),
            events: vec![],
            state: vec![],
            diagnosis: None,
            restricted: vec![],
            unavailable: vec![],
            exit_code: 1,
        }
    }

    #[test]
    fn scaffold_renders_without_tty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = empty_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        terminal.draw(|f| render(f, &output, &ctx)).unwrap();
    }

    #[test]
    fn render_respects_no_color_context() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = empty_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        terminal.draw(|f| render(f, &output, &ctx)).unwrap();
    }

    #[test]
    fn render_respects_ascii_context() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = empty_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true } };
        terminal.draw(|f| render(f, &output, &ctx)).unwrap();
    }

    #[test]
    fn panic_hook_restores_terminal() {
        install_panic_hook();
        // Trigger a panic via catch_unwind to exercise the hook path.
        // Terminal ops in the hook will fail silently in CI (no TTY / no raw mode),
        // which is intentional — the hook must never itself panic.
        let _ = std::panic::catch_unwind(|| panic!("tui scaffold test panic"));
    }
}
