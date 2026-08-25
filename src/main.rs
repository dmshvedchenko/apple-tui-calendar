use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use std::{
    collections::BTreeMap,
    io::{self, Stdout},
    path::PathBuf,
    sync::Arc,
};
use tui_calendar::{
    app::{App, View, spawn_worker},
    backend::{
        CalendarBackend, EventKitBackend, IPC_PROTOCOL_VERSION, MockBackend, OfflineBackend,
        resolve_service, service_search_paths,
    },
    cache::Cache,
    config::Config,
    layout::{item_for_day, layout_overlaps},
    model::{Event as CalendarEvent, FetchRequest, InstantRange},
    range::{RangeLoader, RangePriority, RangeReason, RangeRequest},
    terminal_input::{pointer_cancel_from_focus_loss, pointer_event_from_crossterm},
    ui,
};

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "tui-calendar {}\n\nUSAGE:\n    tui-calendar [--mock]\n    tui-calendar doctor [--mock]\n    tui-calendar debug-events YYYY-MM-DD [--refresh] [--range-start YYYY-MM-DD --range-end YYYY-MM-DD] [--mock]\n    tui-calendar cache <info|vacuum|prune>\n    tui-calendar config path\n\nOPTIONS:\n    --mock         Use demo calendars without EventKit\n    doctor         Check configuration, cache, and EventKit connectivity\n    debug-events   Event-pipeline diagnostic for one local calendar day\n    --refresh      Authoritatively refresh the diagnostic/provider range\n    --range-start  Inclusive local provider diagnostic range start\n    --range-end    Exclusive local provider diagnostic range end\n    -h, --help     Print help\n    -V, --version  Print version",
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
    if args.first().is_some_and(|arg| arg == "debug-events") {
        let date = args
            .get(1)
            .context("usage: tui-calendar debug-events YYYY-MM-DD [--mock]")?
            .parse::<chrono::NaiveDate>()
            .context("debug date must use YYYY-MM-DD")?;
        let parse_range_date = |flag: &str| -> Result<Option<chrono::NaiveDate>> {
            let Some(index) = args.iter().position(|arg| arg == flag) else {
                return Ok(None);
            };
            args.get(index + 1)
                .with_context(|| format!("{flag} requires YYYY-MM-DD"))?
                .parse::<chrono::NaiveDate>()
                .with_context(|| format!("{flag} must use YYYY-MM-DD"))
                .map(Some)
        };
        let provider_start_date = parse_range_date("--range-start")?.unwrap_or(date);
        let provider_end_date = parse_range_date("--range-end")?
            .unwrap_or_else(|| date.succ_opt().expect("date has a successor"));
        if provider_end_date <= provider_start_date {
            anyhow::bail!("--range-end must be after --range-start");
        }
        return run_debug_events(
            &config,
            &cache,
            date,
            provider_start_date,
            provider_end_date,
            use_mock,
            args.iter().any(|arg| arg == "--refresh"),
        )
        .await;
    }
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
        let size = terminal.terminal.size()?;
        ui::sync_timeline_viewport(&mut app, Rect::new(0, 0, size.width, size.height));
        terminal.terminal.draw(|frame| ui::draw(frame, &app))?;
        let calendar_geometry =
            ui::calendar_hit_geometry(&app, Rect::new(0, 0, size.width, size.height));
        tokio::select! {
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) => {
                    if let Some(command) = app.handle_key(key) {
                        let command = app.begin_mutation_session(command);
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
                            let command = app.begin_mutation_session(command);
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

fn local_day_range(
    date: chrono::NaiveDate,
) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    use chrono::{Duration, Local, TimeZone, Utc};
    let midnight = |day: chrono::NaiveDate| {
        Local
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight is valid"))
            .earliest()
            .expect("local midnight is representable")
            .with_timezone(&Utc)
    };
    (midnight(date), midnight(date + Duration::days(1)))
}

fn event_intersects_day(
    event: &CalendarEvent,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    date: chrono::NaiveDate,
) -> bool {
    if event.all_day_date_range().is_some() {
        event.all_day_intersects_dates(date, date.succ_opt().expect("date has a successor"))
    } else {
        event.start < end && event.end > start
    }
}

fn event_kind(event: &CalendarEvent) -> &'static str {
    if event.all_day_date_range().is_some() {
        "trusted-all-day"
    } else if event.all_day {
        "legacy-all-day"
    } else {
        "timed"
    }
}

fn duplicate_occurrence_ids(events: &[CalendarEvent]) -> BTreeMap<String, Vec<&CalendarEvent>> {
    let mut occurrences = BTreeMap::<String, Vec<&CalendarEvent>>::new();
    for event in events {
        occurrences.entry(event.id.clone()).or_default().push(event);
    }
    occurrences.retain(|_, events| {
        events
            .windows(2)
            .any(|pair| pair[0].start != pair[1].start || pair[0].end != pair[1].end)
    });
    occurrences
}

fn print_event_diagnostic(
    events: &[CalendarEvent],
    calendars: &[tui_calendar::model::CalendarInfo],
) {
    use chrono::Local;
    for event in events {
        let calendar = calendars
            .iter()
            .find(|calendar| calendar.id == event.calendar_id)
            .map(|calendar| calendar.title.as_str())
            .unwrap_or("unknown calendar");
        let all_day_range = event
            .all_day_date_range()
            .map(|(start, end)| format!(" [{start}, {end})"))
            .unwrap_or_default();
        let recurrence = match (event.has_recurrence, event.is_detached) {
            (true, true) => " recurring detached",
            (true, false) => " recurring",
            (false, true) => " detached occurrence",
            (false, false) => "",
        };
        let title = event.title.replace(['\n', '\r'], " ");
        println!(
            "  id={} | provider_id={} | series_id={} | title={} | calendar={} ({}) | kind={}{} | start={} ({}) | end={} ({}){}",
            event.id,
            event.provider_id.as_deref().unwrap_or("unavailable"),
            event.series_id.as_deref().unwrap_or("unavailable"),
            title,
            event.calendar_id,
            calendar,
            event_kind(event),
            all_day_range,
            event.start.to_rfc3339(),
            event
                .start
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M %Z"),
            event.end.to_rfc3339(),
            event.end.with_timezone(&Local).format("%Y-%m-%d %H:%M %Z"),
            recurrence,
        );
    }
}

async fn run_debug_events(
    config: &Config,
    cache: &Cache,
    date: chrono::NaiveDate,
    provider_start_date: chrono::NaiveDate,
    provider_end_date: chrono::NaiveDate,
    use_mock: bool,
    refresh_cache: bool,
) -> Result<()> {
    let (start, end) = local_day_range(date);
    let (provider_start, _) = local_day_range(provider_start_date);
    let (_, provider_end) = local_day_range(provider_end_date);
    let next_date = date.succ_opt().context("debug date has no successor")?;
    let snapshot = cache.load_snapshot().context("loading cached snapshot")?;
    let cache_rows = cache.events_intersecting_diagnostic_day(start, end, date, next_date)?;
    let snapshot_day = snapshot
        .events
        .iter()
        .filter(|event| event_intersects_day(event, start, end, date))
        .count();
    let mut day_app = App::new(config.clone(), snapshot.clone());
    day_app.view = View::Day;
    day_app.active_date = date;
    let visible_day = day_app.visible_events();
    let day_items = visible_day
        .iter()
        .enumerate()
        .filter(|(_, event)| !event.all_day)
        .filter_map(|(index, event)| item_for_day(index, event.start, event.end, false, date))
        .collect::<Vec<_>>();
    let timeline_items = layout_overlaps(&day_items);
    let geometry = tui_calendar::ui::calendar_hit_geometry(&day_app, Rect::new(0, 0, 160, 60));
    let (timed_rectangles, all_day_rectangles, timeline_rectangles) = match geometry {
        Some(tui_calendar::hit_test::CalendarHitGeometry::Day(geometry)) => {
            let timed = geometry
                .event_regions
                .iter()
                .filter(|region| {
                    day_app
                        .snapshot
                        .events
                        .iter()
                        .find(|event| event.id == region.event_id)
                        .is_some_and(|event| !event.all_day)
                })
                .count();
            let rectangles = timeline_items
                .iter()
                .map(|positioned| {
                    let event = visible_day[positioned.event_index];
                    let rect = geometry
                        .event_regions
                        .iter()
                        .find(|region| region.event_id == event.id)
                        .map(|region| region.rect);
                    let in_bounds = rect.is_some_and(|rect| {
                        rect.width > 0
                            && rect.height > 0
                            && rect.x >= geometry.timed_area.x
                            && rect.x.saturating_add(rect.width)
                                <= geometry
                                    .timed_area
                                    .x
                                    .saturating_add(geometry.timed_area.width)
                            && rect.y >= geometry.timed_area.y
                            && rect.y.saturating_add(rect.height)
                                <= geometry
                                    .timed_area
                                    .y
                                    .saturating_add(geometry.timed_area.height)
                    });
                    (
                        event.title.replace(['\n', '\r'], " "),
                        positioned.column,
                        positioned.columns,
                        rect,
                        in_bounds,
                    )
                })
                .collect::<Vec<_>>();
            (
                timed,
                geometry.event_regions.len().saturating_sub(timed),
                rectangles,
            )
        }
        _ => (0, 0, Vec::new()),
    };
    let mut agenda_app = App::new(config.clone(), snapshot);
    agenda_app.view = View::Agenda;
    agenda_app.active_date = date;
    let agenda_day = agenda_app
        .visible_events()
        .into_iter()
        .filter(|event| event_intersects_day(event, start, end, date))
        .count();
    let enabled_before_filter = cache_rows.len();
    let enabled_after_filter = visible_day.len();
    let coverage = cache.range_is_fetched(start, end)?;
    let missing_ranges = cache.missing_ranges(start, end)?;

    println!("Event pipeline diagnostic: {date} (local day)");
    println!(
        "Local transport range: [{} , {})",
        start.to_rfc3339(),
        end.to_rfc3339()
    );
    println!("\nLayer                         Count");
    println!("-------------------------------------");
    println!("Cache rows (direct SQLite)     {}", cache_rows.len());
    println!("Snapshot events                {snapshot_day}");
    println!(
        "Calendar-enabled events        {enabled_after_filter} (before filter: {enabled_before_filter})"
    );
    println!("visible_events Day             {}", visible_day.len());
    println!("Agenda candidates for day      {agenda_day}");
    println!("Timed timeline items           {}", day_items.len());
    println!("Timed overlap placements       {}", timeline_items.len());
    println!("All-day items                  {all_day_rectangles}");
    println!("Rendered timed rectangles      {timed_rectangles}");
    println!(
        "Day cache coverage             {}",
        if coverage { "complete" } else { "incomplete" }
    );
    println!("Missing cache ranges           {}", missing_ranges.len());
    for range in &missing_ranges {
        println!(
            "  missing [{} , {})",
            range.start.to_rfc3339(),
            range.end.to_rfc3339()
        );
    }

    println!("\nRenderer-aligned timed rectangles:");
    if timeline_rectangles.is_empty() {
        println!("  unavailable (timeline geometry was not produced)");
    } else {
        for (title, lane, lanes, rect, in_bounds) in timeline_rectangles {
            match rect {
                Some(rect) => println!(
                    "  title={title} | lane={lane}/{lanes} | x={} y={} width={} height={} | in-bounds={in_bounds}",
                    rect.x, rect.y, rect.width, rect.height,
                ),
                None => println!(
                    "  title={title} | lane={lane}/{lanes} | rectangle=clipped | in-bounds=false"
                ),
            }
        }
    }

    println!("\nCached events touching {date}:");
    print_event_diagnostic(&cache_rows, &day_app.snapshot.calendars);

    let backend: Arc<dyn CalendarBackend> = if use_mock {
        Arc::new(MockBackend::seeded())
    } else {
        match EventKitBackend::connect(config.service_path.as_deref()).await {
            Ok(backend) => Arc::new(backend),
            Err(error) => {
                println!("\nEventKit response              unavailable ({error})");
                return Ok(());
            }
        }
    };
    let authorization = backend.authorization_status().await;
    match &authorization {
        Ok(status) => println!("\nEventKit authorization          {status:?}"),
        Err(error) => println!("\nEventKit authorization          unavailable ({error})"),
    }
    let calendars = match backend.calendars().await {
        Ok(calendars) => calendars,
        Err(error) => {
            println!("EventKit response              unavailable ({error})");
            return Ok(());
        }
    };
    let calendar_ids = calendars
        .iter()
        .map(|calendar| calendar.id.clone())
        .collect::<Vec<_>>();
    let provider_events = match backend
        .fetch_events(
            FetchRequest {
                // This intentionally mirrors startup synchronization: a broad
                // instant range without a local-day all-day predicate.
                instant_range: InstantRange {
                    start: provider_start,
                    end: provider_end,
                },
                all_day_range: None,
            },
            &calendar_ids,
        )
        .await
    {
        Ok(events) => events,
        Err(error) => {
            println!("EventKit response              unavailable ({error})");
            return Ok(());
        }
    };
    let provider_day = provider_events
        .iter()
        .filter(|event| event_intersects_day(event, start, end, date))
        .cloned()
        .collect::<Vec<_>>();
    let duplicates = duplicate_occurrence_ids(&provider_events);
    println!(
        "\nEventKit response              {} events in [{} , {})",
        provider_events.len(),
        provider_start_date,
        provider_end_date
    );
    println!(
        "EventKit unique IDs            {}",
        provider_events.len().saturating_sub(
            duplicates
                .values()
                .map(|events| events.len() - 1)
                .sum::<usize>(),
        )
    );
    if duplicates.is_empty() {
        println!("EventKit duplicate occurrence IDs none");
    } else {
        println!("EventKit duplicate occurrence identities:");
        for (id, occurrences) in duplicates {
            println!("  id={id}:");
            for event in occurrences {
                println!(
                    "    start={} end={} provider_id={} series_id={} calendar={} title={}",
                    event.start.to_rfc3339(),
                    event.end.to_rfc3339(),
                    event.provider_id.as_deref().unwrap_or("unavailable"),
                    event.series_id.as_deref().unwrap_or("unavailable"),
                    event.calendar_id,
                    event.title.replace(['\n', '\r'], " "),
                );
            }
        }
    }
    println!("\nEventKit events touching {date}:");
    print_event_diagnostic(&provider_day, &calendars);
    if refresh_cache {
        let snapshot = RangeLoader::new(backend, cache.clone())
            .refresh_range(RangeRequest {
                id: 0,
                start: provider_start,
                end: provider_end,
                all_day_range: None,
                reason: RangeReason::BackgroundRefresh,
                priority: RangePriority::Background,
            })
            .await
            .map_err(anyhow::Error::msg)?;
        let refreshed = cache.events_intersecting_diagnostic_day(start, end, date, next_date)?;
        println!(
            "\nAuthoritative cache refresh     completed for [{} , {})",
            provider_start_date, provider_end_date
        );
        println!("Cache rows after refresh        {}", refreshed.len());
        println!(
            "Snapshot events after refresh   {}",
            snapshot
                .events
                .iter()
                .filter(|event| event_intersects_day(event, start, end, date))
                .count()
        );
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
        resolve_service(config.service_path.as_deref()).or_else(|| {
            service_search_paths(config.service_path.as_deref())
                .into_iter()
                .next()
        })
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
        if doctor_is_healthy(backend, authorization, sqlite) {
            "healthy"
        } else {
            "needs attention"
        }
    );
}

fn doctor_is_healthy(backend: &str, authorization: &str, sqlite: &str) -> bool {
    backend == "OK" && authorization == "FullAccess" && !sqlite.starts_with("FAILED")
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            // `TerminalSession` does not exist yet, so its Drop implementation
            // cannot restore a partially-entered terminal. Make startup errors
            // leave the caller's terminal usable just like errors after entry.
            let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
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

#[cfg(test)]
mod tests {
    use super::doctor_is_healthy;

    #[test]
    fn doctor_requires_full_calendar_access_before_reporting_healthy() {
        assert!(!doctor_is_healthy("OK", "NotDetermined", "OK (schema v3)"));
        assert!(!doctor_is_healthy("OK", "Denied", "OK (schema v3)"));
        assert!(doctor_is_healthy("OK", "FullAccess", "OK (schema v3)"));
    }
}
