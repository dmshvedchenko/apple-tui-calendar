use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub theme: String,
    pub week_start: String,
    pub time_format: String,
    pub default_view: String,
    pub show_week_numbers: bool,
    pub refresh_seconds: u64,
    pub backend: String,
    pub service_path: Option<PathBuf>,
    pub event: EventConfig,
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventConfig {
    pub default_duration_minutes: u16,
    pub default_start_time: String,
    pub time_rounding_minutes: u16,
    pub move_step_minutes: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub past_days: u32,
    pub future_days: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            past_days: 365,
            future_days: 730,
        }
    }
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            default_duration_minutes: 60,
            default_start_time: "09:00".into(),
            time_rounding_minutes: 15,
            move_step_minutes: 15,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            week_start: "monday".into(),
            time_format: "24h".into(),
            default_view: "week".into(),
            show_week_numbers: true,
            refresh_seconds: 60,
            backend: "eventkit".into(),
            service_path: None,
            event: EventConfig::default(),
            cache: CacheConfig::default(),
        }
    }
}

impl Config {
    pub fn directory() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tui-calendar")
    }

    pub fn path() -> PathBuf {
        std::env::var_os("TUI_CALENDAR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| Self::directory().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))?;
        config
            .validate()
            .map_err(|error| anyhow::anyhow!("validating {}: {error:#}", path.display()))?;
        Ok(config)
    }

    pub fn write_example_if_missing(&self) -> Result<()> {
        let path = Self::path();
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn data_directory() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(Self::directory)
            .join("tui-calendar")
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            matches!(self.theme.to_ascii_lowercase().as_str(), "dark" | "light"),
            "theme must be either \"dark\" or \"light\""
        );
        anyhow::ensure!(
            matches!(
                self.week_start.to_ascii_lowercase().as_str(),
                "monday" | "sunday"
            ),
            "week_start must be either \"monday\" or \"sunday\""
        );
        anyhow::ensure!(
            matches!(
                self.time_format.to_ascii_lowercase().as_str(),
                "12h" | "24h"
            ),
            "time_format must be either \"12h\" or \"24h\""
        );
        anyhow::ensure!(
            matches!(
                self.default_view.to_ascii_lowercase().as_str(),
                "day" | "week" | "month" | "agenda"
            ),
            "default_view must be one of \"day\", \"week\", \"month\", or \"agenda\""
        );
        anyhow::ensure!(
            matches!(
                self.backend.to_ascii_lowercase().as_str(),
                "eventkit" | "mock"
            ),
            "backend must be either \"eventkit\" or \"mock\""
        );
        anyhow::ensure!(
            self.refresh_seconds > 0,
            "refresh_seconds must be greater than zero"
        );
        anyhow::ensure!(
            self.event.default_duration_minutes > 0,
            "event.default_duration_minutes must be greater than zero"
        );
        anyhow::ensure!(
            self.event.time_rounding_minutes > 0,
            "event.time_rounding_minutes must be greater than zero"
        );
        anyhow::ensure!(
            self.event.move_step_minutes > 0,
            "event.move_step_minutes must be greater than zero"
        );
        let valid_time =
            chrono::NaiveTime::parse_from_str(&self.event.default_start_time, "%H:%M").is_ok();
        anyhow::ensure!(valid_time, "event.default_start_time must use HH:MM");
        anyhow::ensure!(
            self.cache.past_days <= 3650,
            "cache.past_days must not exceed 3650"
        );
        anyhow::ensure!(
            self.cache.future_days <= 3650,
            "cache.future_days must not exceed 3650"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_partial_configuration_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "default_view = \"agenda\"\nrefresh_seconds = 10\n").unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.default_view, "agenda");
        assert_eq!(config.refresh_seconds, 10);
        assert_eq!(config.time_format, "24h");
    }

    #[test]
    fn rejects_invalid_event_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[event]\ndefault_duration_minutes = 0\n").unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains("validating")
        );
    }

    #[test]
    fn rejects_unknown_and_invalid_top_level_settings_with_field_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "week_start = \"friday\"\n").unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains("week_start")
        );

        fs::write(&path, "unknown_release_setting = true\n").unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn rejects_invalid_runtime_settings_instead_of_silently_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "default_view = \"timeline\"\n").unwrap();
        let error = format!("{:#}", Config::load_from(&path).unwrap_err());
        assert!(error.contains("default_view"));

        fs::write(&path, "time_format = \"military\"\n").unwrap();
        assert!(format!("{:#}", Config::load_from(&path).unwrap_err()).contains("time_format"));

        fs::write(&path, "refresh_seconds = 0\n").unwrap();
        assert!(format!("{:#}", Config::load_from(&path).unwrap_err()).contains("refresh_seconds"));

        fs::write(&path, "[event]\nunknown_event_setting = 1\n").unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }
}
