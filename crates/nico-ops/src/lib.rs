use std::io::{self, IsTerminal, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use nico_common::config::OutputFormat;
use nico_common::theme::{self, Theme};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub mod action;
pub mod app;
pub mod cli;
pub mod data;
pub mod events;
pub mod model;
pub mod view;

use crate::action::Action;
use crate::app::{App, Effect};
use crate::events::{Mode, translate};
use crate::model::LayerSnapshot;

pub use cli::OpsArgs;

const TICK: Duration = Duration::from_millis(250);

const NON_TTY_MESSAGE: &str =
    "nico ops requires an interactive terminal (stdout is not a TTY)";

/// Top-level entry point. Runs the dashboard against a live cluster.
/// Returns a process exit code (0 = clean exit, 3 = preflight failure or
/// not a TTY).
pub async fn run_ops(args: OpsArgs) -> i32 {
    if !io::stdout().is_terminal() {
        eprintln!("{NON_TTY_MESSAGE}");
        return 3;
    }

    let theme = match theme::resolve_theme(args.theme.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let doctor_args = args.to_doctor_args();

    let bootstrapped = match nico_doctor::bootstrap(&doctor_args).await {
        Ok(b) => b,
        Err(nico_doctor::BootstrapErr::Preflight {
            human_message,
            json_payload,
            format,
        }) => {
            if matches!(format, OutputFormat::Json) {
                println!("{json_payload}");
            } else {
                eprintln!("{human_message}");
            }
            return 3;
        }
        Err(nico_doctor::BootstrapErr::Fatal { message, code }) => {
            eprintln!("{message}");
            return code;
        }
    };

    install_panic_hook();

    let mut terminal = match init_terminal() {
        Ok(t) => t,
        Err(e) => {
            let _ = restore_terminal_raw();
            eprintln!("error: failed to enter TUI: {e}");
            return 1;
        }
    };

    let result = run_event_loop(&mut terminal, &theme, bootstrapped).await;

    let _ = restore_terminal(&mut terminal);

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn restore_terminal_raw() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal_raw();
        original(info);
    }));
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    theme: &Theme,
    bootstrapped: nico_doctor::Bootstrapped,
) -> io::Result<()> {
    let nico_doctor::Bootstrapped {
        layers,
        opts,
        _pf_guards,
        ..
    } = bootstrapped;
    let layers = Arc::new(layers);

    let mut app = App::new();
    let (tx, mut rx) = mpsc::channel::<Action>(64);

    spawn_refresh(layers.clone(), opts.clone(), tx.clone());
    let _ = app.handle(Action::Refresh);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if app.dirty() {
            terminal.draw(|f| view::render(&app, theme, f))?;
            app.clear_dirty();
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(ev)) => {
                        if let Some(action) = translate(&ev, Mode::Normal, app.overlay())
                            && dispatch(&mut app, action, &layers, &opts, &tx) {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            maybe_action = rx.recv() => {
                if let Some(action) = maybe_action
                    && dispatch(&mut app, action, &layers, &opts, &tx) {
                    break;
                }
            }
            _ = tick.tick() => {
                // Tick exists to wake the loop on a cadence so cancellation
                // and resize-detection stay responsive; rendering is gated
                // on `app.dirty()` per ADR-010.
            }
        }
    }

    drop(_pf_guards);
    Ok(())
}

fn dispatch(
    app: &mut App,
    action: Action,
    layers: &Arc<Vec<Box<dyn nico_doctor::layer::Layer>>>,
    opts: &nico_doctor::layer::RunOpts,
    tx: &mpsc::Sender<Action>,
) -> bool {
    match app.handle(action) {
        Some(Effect::Quit) => true,
        Some(Effect::StartRefresh) => {
            spawn_refresh(layers.clone(), opts.clone(), tx.clone());
            false
        }
        None => false,
    }
}

fn spawn_refresh(
    layers: Arc<Vec<Box<dyn nico_doctor::layer::Layer>>>,
    opts: nico_doctor::layer::RunOpts,
    tx: mpsc::Sender<Action>,
) {
    tokio::spawn(async move {
        let snapshots: Vec<LayerSnapshot> = data::collect(layers, opts).await;
        let _ = tx.send(Action::Snapshots(snapshots)).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_tty_message_is_concrete() {
        assert!(NON_TTY_MESSAGE.contains("interactive terminal"));
        assert!(NON_TTY_MESSAGE.contains("not a TTY"));
    }
}
