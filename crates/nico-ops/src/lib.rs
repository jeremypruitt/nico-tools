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
use nico_common::k8s::K8sClient;
use nico_common::temporal::{GrpcTemporalClient, TemporalClient};
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
use crate::model::{LayerSnapshot, PopoverEvent, PopoverSeverity, SourceError};

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
        temporal_address,
        temporal_namespace,
        k8s_client,
        _pf_guards,
        ..
    } = bootstrapped;
    let layers = Arc::new(layers);
    let temporal_client: Arc<dyn TemporalClient> =
        Arc::new(GrpcTemporalClient::new(temporal_address));
    let activity_since = chrono::Duration::from_std(opts.since)
        .unwrap_or_else(|_| chrono::Duration::minutes(10));

    let mut app = App::with_interval(interval);
    app.set_baseline(nico_doctor::baseline::load());
    let (tx, mut rx) = mpsc::channel::<Action>(64);

    let activity_ctx = ActivityCtx {
        temporal: temporal_client.clone(),
        k8s: k8s_client.clone(),
        namespace: temporal_namespace.clone(),
        since: activity_since,
    };

    spawn_refresh(layers.clone(), opts.clone(), tx.clone());
    spawn_activity_refresh(
        temporal_client.clone(),
        k8s_client.clone(),
        temporal_namespace.clone(),
        activity_since,
        tx.clone(),
    );
    let _ = app.handle(Action::Refresh);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if app.dirty() {
            terminal.draw(|f| view::render(&mut app, theme, f))?;
            app.clear_dirty();
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(ev)) => {
                        if let Some(action) = translate(&ev, Mode::Normal, app.layout(), app.overlay())
                            && dispatch(&mut app, action, &layers, &opts, &activity_ctx, &tx, terminal) {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            maybe_action = rx.recv() => {
                if let Some(action) = maybe_action
                    && dispatch(&mut app, action, &layers, &opts, &activity_ctx, &tx, terminal) {
                    break;
                }
            }
            _ = tick.tick() => {
                if dispatch(&mut app, Action::Tick(clock.now()), &layers, &opts, &activity_ctx, &tx, terminal) {
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

/// Bundle of dependencies the activity refresh spawner needs. Carried
/// alongside layer refresh so a single `StartRefresh` effect kicks both
/// off in lockstep.
struct ActivityCtx {
    temporal: Arc<dyn TemporalClient>,
    k8s: Option<Arc<dyn K8sClient>>,
    namespace: String,
    since: chrono::Duration,
}

fn dispatch(
    app: &mut App,
    action: Action,
    layers: &Arc<Vec<Box<dyn nico_doctor::layer::Layer>>>,
    opts: &nico_doctor::layer::RunOpts,
    activity: &ActivityCtx,
    tx: &mpsc::Sender<Action>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> bool {
    match app.handle(action) {
        Some(Effect::Quit) => true,
        Some(Effect::StartRefresh) => {
            spawn_refresh(layers.clone(), opts.clone(), tx.clone());
            spawn_activity_refresh(
                activity.temporal.clone(),
                activity.k8s.clone(),
                activity.namespace.clone(),
                activity.since,
                tx.clone(),
            );
            false
        }
        Some(Effect::EnableMouseCapture) => {
            let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
            false
        }
        Some(Effect::DisableMouseCapture) => {
            let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
            false
        }
        Some(Effect::CopyToClipboard(s)) => {
            if let Err(e) = copy_to_clipboard(&s) {
                let _ = tx.try_send(Action::ShowToast(format!("clipboard unavailable: {e}")));
            }
            false
        }
        Some(Effect::OpenUrl(url)) => {
            if let Err(e) = open_url(&url) {
                let _ = tx.try_send(Action::ShowToast(format!("open failed: {e}")));
            }
            false
        }
        Some(Effect::Correlate(workflow_id)) => {
            spawn_correlate(workflow_id, tx.clone());
            false
        }
        None => false,
    }
}

/// Run `nico_correlate::collect_all` for `workflow_id` on a background
/// task and post the resulting timeline back through the action channel.
/// All errors (config load, source preparation, collection) end up as
/// `Action::CorrelateResults` with a populated `source_errors` so the
/// popover can render them inline as `source_error` rows. (Issue #157.)
fn spawn_correlate(workflow_id: String, tx: mpsc::Sender<Action>) {
    tokio::spawn(async move {
        let (events, source_errors) = run_correlate_collect(&workflow_id).await;
        let _ = tx
            .send(Action::CorrelateResults {
                workflow_id,
                events,
                source_errors,
            })
            .await;
    });
}

async fn run_correlate_collect(workflow_id: &str) -> (Vec<PopoverEvent>, Vec<SourceError>) {
    let args = correlate_args(workflow_id);
    let cfg = match nico_correlate::resolve_config(&args) {
        Ok(c) => c,
        Err(nico_correlate::BootstrapErr::Fatal { message, .. }) => {
            return (
                vec![],
                vec![SourceError {
                    name: "config".into(),
                    reason: message,
                }],
            );
        }
    };
    let prepared = nico_correlate::prepare_sources(&args, &cfg).await;
    let results =
        nico_correlate::collect_all(&prepared.named_sources, &args.id, &cfg.id_type).await;
    drop(prepared._pf_guards);

    let mut events: Vec<PopoverEvent> = Vec::new();
    let mut source_errors: Vec<SourceError> = Vec::new();
    for r in results {
        match r {
            nico_correlate::source::SourceResult::Output(o) => {
                for e in o.events {
                    events.push(PopoverEvent {
                        ts: e.ts,
                        source: e.source,
                        kind: e.kind,
                        message: e.message,
                        severity: severity_to_popover(&e.severity),
                    });
                }
            }
            nico_correlate::source::SourceResult::Unavailable(u) => {
                source_errors.push(SourceError {
                    name: u.name.to_string(),
                    reason: u.reason.clone(),
                });
            }
        }
    }
    events.sort_by_key(|e| e.ts);
    (events, source_errors)
}

fn severity_to_popover(s: &nico_correlate::event::Severity) -> PopoverSeverity {
    match s {
        nico_correlate::event::Severity::Info => PopoverSeverity::Info,
        nico_correlate::event::Severity::Warning => PopoverSeverity::Warning,
        nico_correlate::event::Severity::Error => PopoverSeverity::Error,
    }
}

fn correlate_args(workflow_id: &str) -> nico_correlate::CorrelateArgs {
    nico_correlate::CorrelateArgs {
        id: workflow_id.to_string(),
        r#type: Some("workflow".to_string()),
        sources: vec![],
        pod: None,
        since: "1h".to_string(),
        json: false,
        tail: false,
        ascii: false,
        no_color: true,
        theme: None,
        config: None,
        mode: None,
    }
}

/// Best-effort clipboard copy via `arboard`. Failures (headless Linux,
/// no DISPLAY, etc.) are mapped to a toast by the caller.
fn copy_to_clipboard(s: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(s.to_string()).map_err(|e| e.to_string())
}

/// Best-effort URL open. Honors `$BROWSER` when set, falls back to the
/// platform default (`open` on macOS, `xdg-open` on Linux, `cmd /c start`
/// on Windows). Failures bubble up to a toast.
fn open_url(url: &str) -> Result<(), String> {
    use std::process::Command;
    if let Ok(browser) = std::env::var("BROWSER")
        && !browser.is_empty()
    {
        return Command::new(browser)
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/c", "start", url]);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
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

fn spawn_activity_refresh(
    temporal: Arc<dyn TemporalClient>,
    k8s: Option<Arc<dyn K8sClient>>,
    namespace: String,
    since: chrono::Duration,
    tx: mpsc::Sender<Action>,
) {
    let Some(k8s) = k8s else {
        // No reachable kubeconfig — leave the Activity feed empty.
        return;
    };
    tokio::spawn(async move {
        let events = nico_correlate::recent_namespace_events(temporal, k8s, &namespace, since).await;
        let _ = tx.send(Action::NamespaceEvents(events)).await;
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
