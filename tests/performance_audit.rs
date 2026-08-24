//! Opt-in deterministic performance audit for cache-backed UI paths.
//!
//! This deliberately uses local fixtures rather than an EventKit account, so
//! it can be run on CI or during release qualification without reading or
//! mutating a user's calendars:
//!
//! `cargo test --test performance_audit -- --ignored --nocapture`

use chrono::{Duration, NaiveDate, Utc};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use std::time::Instant;
use tempfile::tempdir;
use tui_calendar::{
    app::{App, View},
    cache::Cache,
    config::Config,
    hit_test::CalendarHitTarget,
    model::{
        AuthorizationStatus, Availability, CalendarInfo, CalendarPermissions, Event,
        InvitationStatus, Snapshot, TimeZoneProvenance,
    },
    ui,
};

fn calendar() -> CalendarInfo {
    CalendarInfo {
        id: "audit".into(),
        source_id: "audit-source".into(),
        permissions: CalendarPermissions {
            can_create_events: true,
            can_modify_events: true,
            can_modify_metadata: true,
            can_delete: true,
        },
        title: "Audit".into(),
        account: "Local".into(),
        provider: "Mock".into(),
        color: "#336699".into(),
        is_writable: true,
        enabled: true,
    }
}

fn events(count: usize) -> Vec<Event> {
    let start_date = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    (0..count)
        .map(|index| {
            let start = (start_date + Duration::days((index % 42) as i64))
                .and_hms_opt((index % 24) as u32, 0, 0)
                .unwrap()
                .and_utc();
            Event {
                id: format!("audit-{index}"),
                calendar_id: "audit".into(),
                title: format!("Audit event {index}"),
                start,
                end: start + Duration::minutes(45),
                all_day: false,
                all_day_start_date: None,
                all_day_end_date_exclusive: None,
                location: "Berlin".into(),
                notes: "audit fixture".into(),
                url: String::new(),
                time_zone: "Europe/Berlin".into(),
                time_zone_provenance: TimeZoneProvenance::ExplicitEvent,
                availability: Availability::Busy,
                organizer: None,
                attendees: vec![],
                alarms: vec![],
                recurrence: vec![],
                // These represent already-expanded provider instances. The UI
                // consumes those as ordinary cached events.
                has_recurrence: index % 3 == 0,
                is_detached: false,
                invitation_status: InvitationStatus::Unknown,
            }
        })
        .collect()
}

fn snapshot(events: Vec<Event>) -> Snapshot {
    Snapshot {
        calendars: vec![calendar()],
        events,
        authorization: AuthorizationStatus::FullAccess,
        updated_at: Some(Utc::now()),
    }
}

fn render(app: &App) {
    let backend = TestBackend::new(160, 48);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
}

fn elapsed(label: &str, f: impl FnOnce()) {
    let started = Instant::now();
    f();
    eprintln!("{label}: {:?}", started.elapsed());
}

#[test]
#[ignore = "release-audit measurement; run explicitly with --ignored --nocapture"]
fn measures_local_large_dataset_paths() {
    for count in [1_000, 10_000] {
        let events = events(count);
        let mut app = App::new(Config::default(), snapshot(events.clone()));
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();

        elapsed(&format!("{count} visible month"), || {
            assert!(!app.visible_events().is_empty());
        });
        for view in [View::Day, View::Week, View::Month, View::Agenda] {
            app.view = view;
            elapsed(&format!("{count} {view:?} render"), || render(&app));
        }

        app.view = View::Week;
        let geometry = ui::calendar_hit_geometry(&app, Rect::new(0, 0, 160, 48)).unwrap();
        elapsed(&format!("{count} 1,000 week hit tests"), || {
            for _ in 0..1_000 {
                let _ = geometry.hit_test(40, 20);
            }
        });
        let dragged_id = app.visible_events().first().unwrap().id.clone();
        assert!(app.start_drag_session(
            dragged_id.clone(),
            CalendarHitTarget::ExistingEvent {
                event_id: dragged_id,
            },
        ));
        assert!(app.update_drag_preview(CalendarHitTarget::TimedSlot {
            date: app.active_date,
            minute: 12 * 60,
        }));
        elapsed(&format!("{count} week render with drag preview"), || {
            render(&app)
        });
        app.cancel_drag_session();

        app.search_query = "audit /location:berlin /recurring:true".into();
        elapsed(&format!("{count} structured search"), || {
            assert!(!app.search_results().is_empty());
        });
        elapsed(&format!("{count} repeated structured search"), || {
            assert!(!app.search_results().is_empty());
        });

        let directory = tempdir().unwrap();
        elapsed(&format!("{count} cache open and write"), || {
            let cache = Cache::open(directory.path().join("cache.sqlite3")).unwrap();
            cache.save_calendars(&[calendar()]).unwrap();
            cache
                .replace_events(
                    events.first().unwrap().start - Duration::days(1),
                    events.last().unwrap().end + Duration::days(1),
                    &events,
                )
                .unwrap();
        });
        let cache = Cache::open(directory.path().join("cache.sqlite3")).unwrap();
        elapsed(&format!("{count} cache snapshot"), || {
            assert_eq!(cache.load_snapshot().unwrap().events.len(), count);
        });
        elapsed(&format!("{count} cache text query"), || {
            assert!(!cache.search("audit berlin", 20).unwrap().is_empty());
        });
    }
}
