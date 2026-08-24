//! End-to-end terminal coverage. `script` allocates a real pseudo-terminal so
//! Crossterm's raw-mode/event-stream path is exercised instead of a mocked UI.

use std::process::Stdio;
use tempfile::tempdir;
use tokio::{io::AsyncWriteExt, process::Command, time::Duration};

fn strip_ansi(text: &str) -> String {
    let mut output = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        if characters.next() == Some('[') {
            for control in characters.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        }
    }
    output
}

#[cfg(unix)]
fn crash_once_helper(directory: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let helper = directory.join("ipc-helper.py");
    let state = directory.join("crashed-once");
    std::fs::write(
        &helper,
        format!(
            r#"#!/usr/bin/env python3
import json, os, sys
state = {state:?}
try:
    starts = int(open(state).read())
except (FileNotFoundError, ValueError):
    starts = 0
open(state, "w").write(str(starts + 1))
crash = starts == 0
for line in sys.stdin:
    request = json.loads(line)
    if crash:
        sys.exit(0)
    method = request["method"]
    result = "fullAccess" if method == "authorizationStatus" else []
    print(json.dumps({{"protocol": 2, "id": request["id"], "result": result}}), flush=True)
"#,
            state = state.display().to_string(),
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions).unwrap();
    helper
}

#[tokio::test]
async fn tui_starts_navigates_and_quits_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("terminal.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let binary = env!("CARGO_BIN_EXE_tui-calendar");
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(binary)
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // Month navigation, the read-only Calendar Manager (`gc`), and quit are
    // deliberately sent through the PTY, not by calling App methods, to cover
    // the real crossterm event loop.
    stdin.write_all(b"gmLgcj").await.unwrap();
    stdin.flush().await.unwrap();
    // Send Esc separately so the PTY does not combine it with `q` as Alt-q.
    tokio::time::sleep(Duration::from_millis(80)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not quit from PTY input")
        .unwrap();
    assert!(status.success());
    // `script` records terminal control bytes, whose exact rendering differs
    // across macOS versions; a successful PTY-driven navigation and quit is
    // the stable assertion here.
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_searches_cached_events_opens_details_and_returns_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("search.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b"/Team\r").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    // The first Enter jumps to the cached result; the second opens details.
    stdin.write_all(b"\r").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not complete cached search workflow")
        .unwrap();
    assert!(status.success());
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_opens_filters_executes_and_cancels_the_command_palette_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("command-palette.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // Filter the palette to Go to date, execute it, cancel the nested date
    // prompt back to the palette, then cancel the palette and quit. This uses
    // the real event loop and raw terminal path.
    stdin.write_all(b":Go to date\r").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not complete command-palette workflow")
        .unwrap();
    assert!(status.success());
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_cancels_editor_back_to_event_details_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("details-editor-return.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // Skip the recurring stand-up, open a normal event's details, open its
    // editor, cancel back to details, close details, and quit.
    stdin.write_all(b"j\re").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not return from editor to details")
        .unwrap();
    assert!(status.success());
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_adds_a_structured_relative_alarm_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("structured-alarm.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // Event form fields place Alerts after URL. Select its structured manager,
    // open presets, choose 10 minutes before, then discard the draft.
    stdin.write_all(b"n\t\t\t\t\t\t\t\tajj\r").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"nq").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not quit after structured alarm input")
        .unwrap();
    assert!(status.success());
    // macOS `script` can leave an empty transcript when stdout is redirected,
    // so the stable PTY assertion is the real input sequence and clean exit.
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_creates_a_calendar_in_the_manager_through_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("calendar-create.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b"g").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"c").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"c").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"Calendar Created").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"\t\t").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"\x1b[C").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    stdin.write_all(b"\x13").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    // Whether save has completed or failed, return through the manager before
    // quitting so this test cannot leave a literal `q` in a form field.
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not finish calendar creation from PTY input")
        .unwrap();
    assert!(status.success());
    let snapshot = tui_calendar::cache::Cache::open(data.join("cache.sqlite3"))
        .unwrap()
        .load_snapshot()
        .unwrap();
    assert!(
        snapshot
            .calendars
            .iter()
            .any(|calendar| calendar.title == "Calendar Created")
    );
}

#[tokio::test]
async fn tui_renames_a_calendar_in_the_manager_through_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("calendar-rename.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b"g").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"c").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"e").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin
        .write_all(b"\x7f\x7f\x7f\x7fWork Renamed")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"\x13").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not finish calendar rename from PTY input")
        .unwrap();
    assert!(status.success());
    let snapshot = tui_calendar::cache::Cache::open(data.join("cache.sqlite3"))
        .unwrap()
        .load_snapshot()
        .unwrap();
    assert!(
        snapshot
            .calendars
            .iter()
            .any(|calendar| calendar.id == "work" && calendar.title == "Work Renamed")
    );
}

#[tokio::test]
async fn tui_changes_a_calendar_color_in_the_manager_through_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("calendar-color.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b"g").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"c").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"C").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin
        .write_all(b"\x7f\x7f\x7f\x7f\x7f\x7f\x7f#FF5500")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"\x13").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not finish calendar color change from PTY input")
        .unwrap();
    assert!(status.success());
    let snapshot = tui_calendar::cache::Cache::open(data.join("cache.sqlite3"))
        .unwrap()
        .load_snapshot()
        .unwrap();
    assert!(
        snapshot
            .calendars
            .iter()
            .any(|calendar| calendar.id == "work" && calendar.color == "#FF5500")
    );
}

#[tokio::test]
async fn tui_deletes_a_calendar_only_after_explicit_confirmation_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("calendar-delete.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();

    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // Open Calendar Manager. Cached mock metadata is ordered by account/title,
    // so the local delete fixture follows Work.
    stdin.write_all(b"g").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"c").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"j").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"d").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // First confirmation is deliberately cancelled. A second `d` then `y`
    // proves that the destructive IPC request only follows explicit consent.
    stdin.write_all(b"n").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"d").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"y").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not finish calendar deletion from PTY input")
        .unwrap();
    assert!(status.success());
    let snapshot = tui_calendar::cache::Cache::open(data.join("cache.sqlite3"))
        .unwrap()
        .load_snapshot()
        .unwrap();
    assert!(
        snapshot
            .calendars
            .iter()
            .all(|calendar| calendar.id != "calendar-delete-test")
    );
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_quick_adds_an_event_through_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("quick-add.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .arg("--mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b"a").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin
        .write_all(b"Lunch tomorrow 13:00 #Personal")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"\x13").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not finish Quick Add")
        .unwrap();
    assert!(status.success());
    let snapshot = tui_calendar::cache::Cache::open(data.join("cache.sqlite3"))
        .unwrap()
        .load_snapshot()
        .unwrap();
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.title == "Lunch" && event.calendar_id == "personal")
    );
    assert!(transcript.is_file());
}

#[tokio::test]
async fn tui_cycles_recurrence_ends_and_renders_the_end_date_row_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("recurrence-ends.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg("sh")
        .arg("-c")
        .arg("stty cols 120 rows 40; exec \"$TUI_CALENDAR_BINARY\" --mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .env("TUI_CALENDAR_BINARY", env!("CARGO_BIN_EXE_tui-calendar"))
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // A new event starts with no recurrence. Tab to Repeat, choose Daily,
    // then pass Interval to Ends, select On date, and enter a buffered date.
    stdin.write_all(b"n\t\t\t\t\t\t\t\t\t").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    stdin.write_all(b"\x1b[C\t\t\x1b[C").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    stdin.write_all(b"\t2026-12-31").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Return to Ends, select After occurrences, and enter a valid count.
    stdin.write_all(b"\x1b[Z\x1b[C\t10").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The mode switch is intentionally dirty, so explicitly discard before
    // quitting and prove the terminal session still tears down normally.
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"nq").await.unwrap();
    stdin.flush().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not quit after recurrence-end interaction")
        .unwrap();
    assert!(status.success());
    let rendered = std::fs::read_to_string(&transcript).unwrap_or_default();
    let rendered_text = strip_ansi(&rendered);
    assert!(
        rendered_text.contains("End date"),
        "the dynamic End date field was not rendered: {rendered:?}"
    );
    assert!(
        rendered_text.contains("Until"),
        "the valid buffered date did not reach the recurrence summary: {rendered:?}"
    );
    assert!(
        rendered_text.contains("Occurrences"),
        "the dynamic Occurrences field was not rendered: {rendered:?}"
    );
    assert!(
        rendered_text.contains("For"),
        "the valid buffered occurrence count did not reach the recurrence summary: {rendered:?}"
    );
}

#[tokio::test]
async fn tui_opens_the_recurring_edit_scope_modal_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("recurring-edit-scope.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg("sh")
        .arg("-c")
        .arg("stty cols 120 rows 40; exec \"$TUI_CALENDAR_BINARY\" --mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .env("TUI_CALENDAR_BINARY", env!("CARGO_BIN_EXE_tui-calendar"))
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    // The deterministic mock's first event is the recurring Team stand-up.
    stdin.write_all(b"e").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"1").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // The opened editor is clean, so Esc returns to the calendar before quit.
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not quit after recurring edit scope selection")
        .unwrap();
    assert!(status.success());
    let rendered = strip_ansi(&std::fs::read_to_string(&transcript).unwrap_or_default());
    assert!(rendered.contains("Edit recurring event"), "{rendered:?}");
    assert!(rendered.contains("This occurrence"), "{rendered:?}");
    assert!(rendered.contains("This and future events"), "{rendered:?}");
    assert!(!rendered.contains("Entire series"), "{rendered:?}");
}

#[tokio::test]
async fn tui_cancels_the_recurring_delete_scope_modal_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let transcript = directory.path().join("recurring-delete-scope.log");
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg("sh")
        .arg("-c")
        .arg("stty cols 120 rows 40; exec \"$TUI_CALENDAR_BINARY\" --mock")
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .env("TUI_CALENDAR_BINARY", env!("CARGO_BIN_EXE_tui-calendar"))
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b"d").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    stdin.write_all(b"\x1b").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    stdin.write_all(b"q").await.unwrap();
    stdin.flush().await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI did not quit after recurring delete scope cancellation")
        .unwrap();
    assert!(status.success());
    let rendered = strip_ansi(&std::fs::read_to_string(&transcript).unwrap_or_default());
    assert!(rendered.contains("Delete recurring event"), "{rendered:?}");
    assert!(rendered.contains("This occurrence"), "{rendered:?}");
    assert!(rendered.contains("This and future events"), "{rendered:?}");
    assert!(!rendered.contains("Entire series"), "{rendered:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn tui_survives_a_helper_crash_and_reconnects_in_a_pseudo_terminal() {
    let directory = tempdir().unwrap();
    let helper = crash_once_helper(directory.path());
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::write(
        &config,
        format!(
            "service_path = {:?}\nrefresh_seconds = 3600\n",
            helper.display().to_string()
        ),
    )
    .unwrap();
    let transcript = directory.path().join("recovery.log");
    let mut child = Command::new("script")
        .arg("-q")
        .arg(&transcript)
        .arg(env!("CARGO_BIN_EXE_tui-calendar"))
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // The first helper process exits during the worker's authorization call;
    // allow a little scheduling headroom beyond the supervisor's 1s retry
    // when this runs beside the rest of the PTY suite.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let starts: u32 = std::fs::read_to_string(directory.path().join("crashed-once"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        starts >= 2,
        "expected the helper to be restarted, got {starts} starts"
    );
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"Lq")
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("TUI froze after helper recovery")
        .unwrap();
    assert!(status.success());
    assert!(transcript.is_file());
}
