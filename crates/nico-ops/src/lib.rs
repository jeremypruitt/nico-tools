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
pub mod hbn_panel;
pub mod model;
pub mod popup;
pub mod pulse;
pub mod ringbuffer;
pub mod view;
pub mod widgets;

use crate::action::Action;
use crate::app::{App, Effect};
use crate::clock::{Clock, SystemClock};
use crate::events::{Mode, translate};
use crate::model::{
    LayerSnapshot, LogLine, PopoverEvent, PopoverSeverity, SourceError, log_level_from_text,
};

pub use cli::{HbnPanelArgs, OpsArgs, OpsCommand};

/// How often the host loop sends a `Tick` to the reducer. Drives both
/// auto-refresh deadline checks and throbber animation.
const TICK: Duration = Duration::from_millis(100);

const NON_TTY_MESSAGE: &str = "nico ops requires an interactive terminal (stdout is not a TTY)";

/// Top-level entry point. Runs the dashboard against a live cluster.
/// Returns a process exit code (0 = clean exit, 3 = preflight failure or
/// not a TTY).
pub async fn run_ops(args: OpsArgs) -> i32 {
    if let Some(OpsCommand::Hbn(hbn_args)) = args.command.clone() {
        return run_ops_hbn(args, hbn_args).await;
    }

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
            human_message: _,
            json_payload,
            format,
        }) => {
            if matches!(format, OutputFormat::Json) {
                println!("{json_payload}");
            }
            // Non-JSON modes already had the failure card painted on
            // stderr by the boot-probe orchestrator; reprinting
            // `human_message` duplicates the same fields.
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
    // Mission Control (Layout B) was removed in PRD-006 slice 1 (issue
    // #367). Its Activity quadrant was the only consumer of
    // `temporal_address`, `temporal_namespace`, and `k8s_client` in this
    // event loop, so they are destructured but ignored — bootstrap still
    // resolves them so the doctor layers keep working.
    let nico_doctor::Bootstrapped {
        layers,
        opts,
        temporal_address: _,
        temporal_namespace: _,
        k8s_client: _,
        log_source,
        log_collector,
        _pf_guards,
        ..
    } = bootstrapped;
    let layers = Arc::new(layers);

    let mut app = App::with_interval(interval);
    app.set_baseline(nico_doctor::baseline::load());
    let (tx, mut rx) = mpsc::channel::<Action>(64);

    let refresh_ctx = RefreshCtx {
        logs: LogsCtx {
            log_source: log_source.clone(),
            namespace: opts.namespace.clone(),
            since: opts.since,
        },
        log_collector,
    };

    spawn_refresh(
        layers.clone(),
        opts.clone(),
        refresh_ctx.log_collector.clone(),
        tx.clone(),
    );
    spawn_logs_refresh(&refresh_ctx.logs, tx.clone());
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
                            && dispatch(&mut app, action, &layers, &opts, &refresh_ctx, &tx, terminal) {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            maybe_action = rx.recv() => {
                if let Some(action) = maybe_action
                    && dispatch(&mut app, action, &layers, &opts, &refresh_ctx, &tx, terminal) {
                    break;
                }
            }
            _ = tick.tick() => {
                if dispatch(&mut app, Action::Tick(clock.now()), &layers, &opts, &refresh_ctx, &tx, terminal) {
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

/// All non-layer fan-out dependencies the host loop spawns alongside a
/// `StartRefresh`. One `R` press kicks off layer collection (via
/// `spawn_refresh`) plus the snapshot logs panel in lockstep.
/// `log_collector` runs once per refresh before layers fan out (issue
/// #201). The Activity feed used to live here too; it was removed
/// together with Mission Control in PRD-006 slice 1 (issue #367).
struct RefreshCtx {
    logs: LogsCtx,
    log_collector: Option<Arc<nico_doctor::log_collector::LogCollectorStage>>,
}

struct LogsCtx {
    log_source: Option<Arc<dyn nico_doctor::log_source::LogSource>>,
    namespace: String,
    since: Duration,
}

fn dispatch(
    app: &mut App,
    action: Action,
    layers: &Arc<Vec<Box<dyn nico_doctor::layer::Layer>>>,
    opts: &nico_doctor::layer::RunOpts,
    refresh: &RefreshCtx,
    tx: &mpsc::Sender<Action>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> bool {
    match app.handle(action) {
        Some(Effect::Quit) => true,
        Some(Effect::StartRefresh) => {
            spawn_refresh(
                layers.clone(),
                opts.clone(),
                refresh.log_collector.clone(),
                tx.clone(),
            );
            spawn_logs_refresh(&refresh.logs, tx.clone());
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
    let id_str = args.id.clone().unwrap_or_default();
    let results = nico_correlate::collect_all(&prepared.named_sources, &id_str, &cfg.id_type).await;
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
        command: None,
        id: Some(workflow_id.to_string()),
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
        postgres_url: None,
        timeouts: None,
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
    log_collector: Option<Arc<nico_doctor::log_collector::LogCollectorStage>>,
    tx: mpsc::Sender<Action>,
) {
    tokio::spawn(async move {
        let snapshots: Vec<LayerSnapshot> = data::collect(layers, opts, log_collector).await;
        let _ = tx.send(Action::Snapshots(snapshots)).await;
    });
}

/// Fetch-side firehose cap for the snapshot logs panel — matches
/// `LogsLayer` so the panel and the layer see the same data. The
/// renderer (`render_logs_panel`) is the sole cap on visible row count;
/// see ADR-0014.
const LOG_PANEL_FETCH_LIMIT: usize = 500;

/// Fan-out partner of `spawn_refresh` for the snapshot logs panel. Calls
/// `LogSource::collect` (the same `best_effort_chain` the `LogsLayer`
/// uses) and posts an `Action::LogLines` carrying the top-N. Failures
/// surface as an empty panel; the layer card already raises the warning
/// for an unreachable source.
fn spawn_logs_refresh(ctx: &LogsCtx, tx: mpsc::Sender<Action>) {
    let Some(source) = ctx.log_source.clone() else {
        return;
    };
    let namespace = ctx.namespace.clone();
    let since = ctx.since;
    tokio::spawn(async move {
        // The snapshot panel runs outside the doctor refresh path, so
        // there is no shared `LogCollectorStage` cache to read from.
        // Pass an empty map and let `K8sLogSource` fall back to
        // a direct `pod_logs` fetch.
        let prefetched = std::collections::HashMap::new();
        let lines = match source
            .collect(&namespace, since, LOG_PANEL_FETCH_LIMIT, &prefetched)
            .await
        {
            Ok(c) => log_lines_from_entries(c.entries),
            Err(_) => Vec::new(),
        };
        let _ = tx.send(Action::LogLines(lines)).await;
    });
}

/// Convert raw `(pod, line)` entries from a `LogCollection` into
/// `LogLine`s for the snapshot panel. Returns every classified entry —
/// `render_logs_panel` is the sole cap on visible row count (ADR-0014).
/// The timestamp is the snapshot fetch time — the shared `LogSource` API
/// drops Loki's per-line timestamps today; carrying them is a follow-up.
fn log_lines_from_entries(entries: Vec<(String, String)>) -> Vec<LogLine> {
    let now = chrono::Utc::now();
    entries
        .into_iter()
        .map(|(pod, line)| LogLine {
            ts: now,
            pod,
            level: log_level_from_text(&line),
            message: line,
        })
        .collect()
}

/// `nico ops hbn` — focused per-DPU HBN panel (issue #209).
///
/// Skips the doctor-style multi-layer bootstrap (no k8s / Temporal / Loki
/// needed) and goes straight to forgedb via [`nico_doctor::hbn::SqlxHbnClient`].
/// Auto-refreshes on the same cadence chain as the dashboard
/// (`--interval` flag → `[output] tui_refresh` → `NICO_TUI_REFRESH` →
/// 30s). Layout switches by terminal width (Option A wide, Option B
/// narrow); sort defaults to status-first triage.
pub async fn run_ops_hbn(args: OpsArgs, hbn_args: HbnPanelArgs) -> i32 {
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
    let config = match nico_doctor::load_minimal_config(&doctor_args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let interval = match resolve_interval(args.interval.as_deref(), config.output.tui_refresh) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let sort_col = match hbn_args.sort.as_str() {
        "status" => hbn_panel::SortColumn::Status,
        "machine" | "machine-id" | "machine_id" => hbn_panel::SortColumn::MachineId,
        other => {
            eprintln!("error: invalid --sort {other:?}; expected `status` or `machine`");
            return 1;
        }
    };

    let client: Arc<dyn nico_doctor::hbn::HbnClient> =
        match nico_doctor::hbn::SqlxHbnClient::new(&config.postgres.url) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("error: invalid postgres URL: {e}");
                return 1;
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

    let result = run_hbn_event_loop(&mut terminal, &theme, client, interval, sort_col).await;

    let _ = restore_terminal(&mut terminal);

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

async fn run_hbn_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    theme: &Theme,
    client: Arc<dyn nico_doctor::hbn::HbnClient>,
    interval: Duration,
    initial_sort: hbn_panel::SortColumn,
) -> io::Result<()> {
    use chrono::Utc;
    use crossterm::event::{Event, KeyCode, KeyEventKind};

    let (tx, mut rx) = mpsc::channel::<HbnTick>(8);
    let mut sort_col = initial_sort;
    let mut rows: Vec<nico_doctor::hbn::HbnRow> = Vec::new();
    let mut last_error: Option<String> = None;
    let mut refreshing = true;
    let mut last_refreshed: Option<chrono::DateTime<chrono::Local>> = None;

    spawn_hbn_refresh(client.clone(), tx.clone());

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick; the spawn above already kicked one off.
    tick.tick().await;

    loop {
        terminal.draw(|f| {
            let area = f.area();
            let layout = hbn_panel::select_layout(area.width);
            hbn_panel::render_panel(&rows, layout, theme, f, area);
            paint_hbn_status(
                f,
                theme,
                area,
                refreshing,
                last_refreshed,
                last_error.as_deref(),
                sort_col,
            );
        })?;

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('r') | KeyCode::Char('R')
                                if !refreshing =>
                            {
                                refreshing = true;
                                spawn_hbn_refresh(client.clone(), tx.clone());
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                sort_col = match sort_col {
                                    hbn_panel::SortColumn::Status => hbn_panel::SortColumn::MachineId,
                                    hbn_panel::SortColumn::MachineId => hbn_panel::SortColumn::Status,
                                };
                                hbn_panel::sort_rows(&mut rows, sort_col);
                            }
                            _ => {}
                        }
                    }
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            maybe_tick = rx.recv() => {
                match maybe_tick {
                    Some(HbnTick::Snapshots(snaps)) => {
                        let now = Utc::now();
                        rows = snaps.iter().map(|s| nico_doctor::hbn::aggregate_row(s, now)).collect();
                        hbn_panel::sort_rows(&mut rows, sort_col);
                        last_error = None;
                        refreshing = false;
                        last_refreshed = Some(chrono::Local::now());
                    }
                    Some(HbnTick::Error(msg)) => {
                        last_error = Some(msg);
                        refreshing = false;
                        last_refreshed = Some(chrono::Local::now());
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                if !refreshing {
                    refreshing = true;
                    spawn_hbn_refresh(client.clone(), tx.clone());
                }
            }
        }
    }

    Ok(())
}

enum HbnTick {
    Snapshots(Vec<nico_doctor::hbn::HbnSnapshot>),
    Error(String),
}

fn spawn_hbn_refresh(client: Arc<dyn nico_doctor::hbn::HbnClient>, tx: mpsc::Sender<HbnTick>) {
    tokio::spawn(async move {
        let result = client.fetch_all_snapshots().await;
        let msg = match result {
            Ok(snaps) => HbnTick::Snapshots(snaps),
            Err(e) => HbnTick::Error(e.to_string()),
        };
        let _ = tx.send(msg).await;
    });
}

fn paint_hbn_status(
    frame: &mut ratatui::Frame,
    theme: &Theme,
    area: ratatui::layout::Rect,
    refreshing: bool,
    last_refreshed: Option<chrono::DateTime<chrono::Local>>,
    last_error: Option<&str>,
    sort_col: hbn_panel::SortColumn,
) {
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    if area.height < 1 {
        return;
    }
    let strip = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };

    let sort_label = match sort_col {
        hbn_panel::SortColumn::Status => "status",
        hbn_panel::SortColumn::MachineId => "machine",
    };
    let refreshed = last_refreshed
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "—".to_string());
    let mut spans = vec![
        Span::styled(
            format!(" sort:{sort_label}  refreshed:{refreshed}  "),
            Style::default().fg(theme.muted),
        ),
        Span::styled("[r]efresh [s]ort [q]uit", Style::default().fg(theme.muted)),
    ];
    if refreshing {
        spans.insert(
            0,
            Span::styled("⟳ refreshing  ", Style::default().fg(theme.warn)),
        );
    }
    if let Some(err) = last_error {
        spans.push(Span::styled(
            format!("  error: {err}"),
            Style::default().fg(theme.error),
        ));
    }
    let p = Paragraph::new(Line::from(spans));
    frame.render_widget(p, strip);
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

    #[test]
    fn log_lines_from_entries_returns_all_entries_uncapped() {
        // ADR-0014: the data path no longer caps; the renderer is the sole
        // cap on visible row count (see view::tests for renderer behavior).
        let entries: Vec<(String, String)> = (0..50)
            .map(|i| (format!("pod-{i}"), format!("ERROR line {i}")))
            .collect();
        let out = log_lines_from_entries(entries);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0].pod, "pod-0");
        assert_eq!(out[49].pod, "pod-49");
    }

    #[test]
    fn log_lines_from_entries_classifies_levels() {
        use nico_common::output::Status;
        let entries = vec![
            ("a".into(), "FATAL: oom".into()),
            ("b".into(), "ERROR: bad".into()),
            ("c".into(), "trace".into()),
        ];
        let out = log_lines_from_entries(entries);
        assert_eq!(out[0].level, Status::Fail);
        assert_eq!(out[1].level, Status::Warn);
        assert_eq!(out[2].level, Status::Unknown);
    }
}
