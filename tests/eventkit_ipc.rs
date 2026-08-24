use std::path::PathBuf;
use tui_calendar::backend::{CalendarBackend, EventKitBackend};

#[tokio::test]
async fn native_service_answers_the_versioned_json_protocol() {
    let service = std::env::var_os("TUI_CALENDAR_SERVICE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("macos-calendar-service/.build/debug/tui-calendar-service")
        });
    if !service.is_file() {
        eprintln!("skipping EventKit IPC smoke test; build macos-calendar-service first");
        return;
    }
    let backend = EventKitBackend::connect(Some(&service)).await.unwrap();
    let _status = backend.authorization_status().await.unwrap();
}
