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
pub mod clock;
pub mod data;
pub mod events;
pub mod model;
pub mod pulse;
pub mod ringbuffer;
pub mod view;
pub mod widgets;

use crate::action::Action;
use crate::app::{App, Effect};
use crate::clock::{Clock, SystemClock};
use crate::events::{Mode, translate};
use crate::model::LayerSnapshot;

pub use cli::OpsArgs;

/// How often the host loop sends a `Tick` to the reducer. Drives both
/// auto-refresh deadline checks and throbber animation.
const TICK: Duration = Duration::from_millis(100);

const NON_TTY_MESSAGE: &str = "nico ops requires an interactive terminal (stdout is not a TTY)";

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

    let interval = match resolve_interval(args.interval.as_deref(), bootstrapped.tui_refresh) {
        Ok(d) => d,
        Err(e) => {
            let _ = restore_terminal(&mut terminal);
            eprintln!("error: {e}");
            return 1;
        }
    };

    let result = run_event_loop(&mut terminal, &theme, bootstrapped, interval, SystemClock).await;

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

async fn run_event_loop<C: Clock>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    theme: &Theme,
    bootstrapped: nico_doctor::Bootstrapped,
    interval: Duration,
    clock: C,
) -> io::Result<()> {
    let nico_doctor::Bootstrapped {
        layers,
        opts,
        _pf_guards,
        ..
    } = bootstrapped;
    let layers = Arc::new(layers);

    let mut app = App::with_interval(interval);
    app.set_baseline(nico_doctor::baseline::load());
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
                if dispatch(&mut app, Action::Tick(clock.now()), &layers, &opts, &tx) {
                    break;
                }
            }
        }
    }

    drop(_pf_guards);
    Ok(())
}

/// Resolve the auto-refresh cadence using the ADR-007 precedence chain:
/// `--interval` flag > env (already absorbed into `bootstrap_default`) >
/// config file > default. The flag layer is applied here on top of the
/// already-resolved `bootstrap_default` from `Bootstrapped::tui_refresh`.
pub fn resolve_interval(
    interval_flag: Option<&str>,
    bootstrap_default: Duration,
) -> Result<Duration, String> {
    match interval_flag {
        Some(s) => {
            humantime::parse_duration(s).map_err(|e| format!("invalid --interval {s:?}: {e}"))
        }
        None => Ok(bootstrap_default),
    }
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

    #[test]
    fn resolve_interval_falls_back_to_bootstrap_default() {
        let d = resolve_interval(None, Duration::from_secs(30)).unwrap();
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn resolve_interval_flag_overrides_default() {
        let d = resolve_interval(Some("5s"), Duration::from_secs(30)).unwrap();
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn resolve_interval_flag_overrides_bootstrap_default_from_config() {
        // `bootstrap_default` already encodes config + env; the flag wins.
        let d = resolve_interval(Some("1m"), Duration::from_secs(10)).unwrap();
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn resolve_interval_rejects_invalid_input() {
        let err = resolve_interval(Some("not-a-duration"), Duration::from_secs(30)).unwrap_err();
        assert!(err.contains("invalid --interval"), "{err}");
    }
}
