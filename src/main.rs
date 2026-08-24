use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use std::{
    io::{self, Stdout},
    path::PathBuf,
    sync::Arc,
};
use tui_calendar::{
    app::{App, spawn_worker},
    backend::{
        CalendarBackend, EventKitBackend, IPC_PROTOCOL_VERSION, MockBackend, OfflineBackend,
        resolve_service,
    },
    cache::Cache,
    config::Config,
    terminal_input::{pointer_cancel_from_focus_loss, pointer_event_from_crossterm},
    ui,
};

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "tui-calendar {}\n\nUSAGE:\n    tui-calendar [--mock]\n    tui-calendar doctor [--mock]\n    tui-calendar cache <info|vacuum|prune>\n    tui-calendar config path\n\nOPTIONS:\n    --mock       Use demo calendars without EventKit\n    doctor       Check configuration, cache, and EventKit connectivity\n    -h, --help   Print help\n    -V, --version  Print version",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("tui-calendar {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.as_slice() == ["config", "path"] {
        println!("{}", Config::path().display());
        return Ok(());
    }

    let config = Config::load().context("loading configuration")?;
    let data_dir = std::env::var_os("TUI_CALENDAR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(Config::data_directory);
    let (cache, cache_recovery) = Cache::open_with_recovery(data_dir.join("cache.sqlite3"))
        .context("opening local calendar cache")?;
    if cache_recovery.is_some() {
        eprintln!(
            "Cache was corrupted and has been quarantined. Calendar data will be reloaded from EventKit."
        );
    }
    if args.first().is_some_and(|arg| arg == "cache") {
        return run_cache_command(args.get(1).map(String::as_str), &cache, &config);
    }
    let use_mock =
        args.iter().any(|arg| arg == "--mock") || config.backend.eq_ignore_ascii_case("mock");
    if args.first().is_some_and(|arg| arg == "doctor") {
        return run_doctor(&config, &cache, use_mock).await;
    }
    config
        .write_example_if_missing()
        .context("creating default configuration")?;
    let snapshot = cache.load_snapshot().context("loading cached calendars")?;

    let (backend, startup_error): (Arc<dyn CalendarBackend>, Option<String>) = if use_mock {
        (Arc::new(MockBackend::seeded()), None)
    } else {
        match EventKitBackend::connect(config.service_path.as_deref()).await {
            Ok(backend) => (Arc::new(backend), None),
            Err(error) => {
                let message = format!("{error}; showing offline cache");
                (
                    Arc::new(OfflineBackend::new(error.to_string())),
                    Some(message),
                )
            }
        }
    };
    let refresh_seconds = config.refresh_seconds;
    let cache_past_days = config.cache.past_days;
    let cache_future_days = config.cache.future_days;
    let mut app = App::new(config, snapshot);
    if cache_recovery.is_some() {
        app.apply_update(tui_calendar::app::WorkerUpdate::Error(
            "Cache was corrupted and has been quarantined. Calendar data will be reloaded from EventKit."
                .into(),
        ));
    }
    if let Some(error) = startup_error {
        app.apply_update(tui_calendar::app::WorkerUpdate::Error(error));
        app.apply_update(tui_calendar::app::WorkerUpdate::BackendState(
            tui_calendar::backend::BackendState::Disconnected,
        ));
    } else {
        app.apply_update(tui_calendar::app::WorkerUpdate::BackendState(
            tui_calendar::backend::BackendState::Connected,
        ));
    }
    let (commands, mut updates) = spawn_worker(
        backend,
        cache,
        refresh_seconds,
        cache_past_days,
        cache_future_days,
    );
    let mut terminal = TerminalSession::enter()?;
    let mut events = EventStream::new();
    // Input and backend updates wake the loop immediately. The low-frequency tick
    // only advances relative-time labels and expires transient status messages.
    let mut redraw = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        terminal.terminal.draw(|frame| ui::draw(frame, &app))?;
        let size = terminal.terminal.size()?;
        let calendar_geometry =
            ui::calendar_hit_geometry(&app, Rect::new(0, 0, size.width, size.height));
        tokio::select! {
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) => {
                    if let Some(command) = app.handle_key(key) {
                        app.note_dispatched_mutation(&command);
                        let _ = commands.send(command);
                    }
                }
                Some(Ok(Event::Mouse(mouse))) => {
                    if let Some(action) = app.handle_pointer_with_hit_test(
                        pointer_event_from_crossterm(mouse),
                        calendar_geometry.as_ref(),
                    ) {
                        // Pointer drag is only another source of the existing
                        // action workflow. No event state is changed until the
                        // worker confirms the normal update request.
                        app.cancel_drag_session();
                        if let Some(command) = app.execute_action(action) {
                            app.note_dispatched_mutation(&command);
                            let _ = commands.send(command);
                        }
                    }
                }
                Some(Ok(Event::FocusLost)) => {
                    app.handle_pointer(pointer_cancel_from_focus_loss());
                }
                Some(Ok(Event::Resize(_, _))) => terminal.terminal.autoresize()?,
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            Some(update) = updates.recv() => {
                let recovered = matches!(
                    update,
                    tui_calendar::app::WorkerUpdate::BackendState(
                        tui_calendar::backend::BackendState::Connected
                    )
                ) && !matches!(app.backend_state, tui_calendar::backend::BackendState::Connected);
                app.apply_update(update);
                if recovered {
                    let _ = commands.send(tui_calendar::app::WorkerCommand::RefreshRange(
                        app.visible_range_request(),
                    ));
                }
            },
            _ = redraw.tick() => {},
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn run_cache_command(command: Option<&str>, cache: &Cache, config: &Config) -> Result<()> {
    match command {
        Some("info") => {
            let stats = cache.stats()?;
            println!(
                "Terminal Calendar Cache\n\nPath                  {}\nSize                  {} bytes\nEvents                {}\nCalendars             {}\nFetched ranges        {}\nOldest cached event   {}\nNewest cached event   {}\nSchema version        {}\nRetention window      -{} / +{} days\nIntegrity             OK",
                cache.path().display(),
                stats.bytes,
                stats.event_count,
                stats.calendar_count,
                stats.fetched_range_count,
                stats.oldest_event.as_deref().unwrap_or("none"),
                stats.newest_event.as_deref().unwrap_or("none"),
                stats.schema_version,
                config.cache.past_days,
                config.cache.future_days,
            );
            Ok(())
        }
        Some("vacuum") => {
            cache.vacuum()?;
            println!("Vacuumed {}", cache.path().display());
            Ok(())
        }
        Some("prune") => {
            let before = cache.stats()?.bytes;
            let now = chrono::Utc::now();
            let removed = cache.prune_outside(
                now - chrono::Duration::days(i64::from(config.cache.past_days)),
                now + chrono::Duration::days(i64::from(config.cache.future_days)),
            )?;
            let after = cache.stats()?.bytes;
            println!("Pruned {removed} cached events ({before} → {after} bytes)");
            Ok(())
        }
        _ => anyhow::bail!("usage: tui-calendar cache <info|vacuum|prune>"),
    }
}

async fn run_doctor(config: &Config, cache: &Cache, use_mock: bool) -> Result<()> {
    let macos = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|version| version.trim().to_owned())
        .unwrap_or_else(|| "unavailable".into());
    let sqlite = match cache.health_check() {
        Ok(()) => format!("OK (schema v{})", cache.schema_version()?),
        Err(error) => format!("FAILED: {error}"),
    };
    let helper_path = if use_mock {
        None
    } else {
        resolve_service(config.service_path.as_deref())
    };
    let backend: Arc<dyn CalendarBackend> = if use_mock {
        Arc::new(MockBackend::seeded())
    } else {
        match EventKitBackend::connect(config.service_path.as_deref()).await {
            Ok(backend) => Arc::new(backend),
            Err(error) => {
                print_doctor(
                    &macos,
                    cache,
                    &sqlite,
                    "unavailable",
                    "unavailable",
                    &format!("FAILED: {error}"),
                    helper_path.as_deref(),
                    None,
                );
                return Ok(());
            }
        }
    };
    let authorization = backend
        .authorization_status()
        .await
        .map(|status| format!("{status:?}"))
        .unwrap_or_else(|error| format!("FAILED: {error}"));
    let calendars = backend
        .calendars()
        .await
        .map(|items| items.len().to_string())
        .unwrap_or_else(|error| format!("unavailable ({error})"));
    let capabilities = backend.calendar_capabilities().await.ok();
    print_doctor(
        &macos,
        cache,
        &sqlite,
        &authorization,
        &calendars,
        "OK",
        helper_path.as_deref(),
        capabilities.as_ref(),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_doctor(
    macos: &str,
    cache: &Cache,
    sqlite: &str,
    authorization: &str,
    calendars: &str,
    backend: &str,
    helper_path: Option<&std::path::Path>,
    capabilities: Option<&tui_calendar::model::CalendarCapabilities>,
) {
    let support = |value: Option<bool>| {
        if value == Some(true) {
            "supported"
        } else {
            "unsupported"
        }
    };
    println!(
        "Terminal Calendar Doctor\n\nVersion                {}\nmacOS                  {macos}\nArchitecture           {}\nConfig                 {}\nDatabase               {}\nEventKit helper        {}\nCalendar permission    {authorization}\nEventKit backend       {backend}\nSQLite                 {sqlite}\nCalendars              {calendars}\nCalendar create        {}\nCalendar rename        {}\nCalendar color         {}\nCalendar delete        {}\nIPC protocol           v{}\n\nStatus: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        Config::path().display(),
        cache.path().display(),
        helper_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| if backend == "OK" {
                "mock backend".into()
            } else {
                "not found".into()
            }),
        support(capabilities.map(|value| value.can_create)),
        support(capabilities.map(|value| value.can_update)),
        support(capabilities.map(|value| value.can_change_color)),
        support(capabilities.map(|value| value.can_delete)),
        IPC_PROTOCOL_VERSION,
        if backend == "OK" && !authorization.starts_with("FAILED") && !sqlite.starts_with("FAILED")
        {
            "healthy"
        } else {
            "needs attention"
        }
    );
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}
