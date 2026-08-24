//! Process-level release regression for disposable-cache recovery.

use std::process::Command;
use tempfile::tempdir;

#[test]
fn mock_doctor_recovers_from_a_corrupted_cache_without_touching_a_provider() {
    let directory = tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let data = directory.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(&config, "backend = \"mock\"\n").unwrap();
    let cache = data.join("cache.sqlite3");
    std::fs::write(&cache, "not a sqlite database").unwrap();
    std::fs::write(format!("{}-wal", cache.display()), "bad wal").unwrap();
    std::fs::write(format!("{}-shm", cache.display()), "bad shm").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tui-calendar"))
        .args(["doctor", "--mock"])
        .env("TUI_CALENDAR_CONFIG", &config)
        .env("TUI_CALENDAR_DATA_DIR", &data)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Status: healthy"));
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "Cache was corrupted and has been quarantined. Calendar data will be reloaded from EventKit."
    ));
    assert!(cache.is_file());
    assert!(data.read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".corrupt.")
    }));
}
