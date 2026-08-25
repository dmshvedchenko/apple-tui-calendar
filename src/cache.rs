use crate::model::{AuthorizationStatus, CalendarInfo, Event, Snapshot};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

const CACHE_SCHEMA_VERSION: u32 = 3;

/// Files preserved after a positively identified local SQLite corruption.
/// EventKit is never involved in this recovery; a replacement cache starts
/// empty and is repopulated through the ordinary synchronization flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRecovery {
    pub quarantined_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Cache {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub bytes: u64,
    pub event_count: u64,
    pub calendar_count: u64,
    pub oldest_event: Option<String>,
    pub newest_event: Option<String>,
    pub schema_version: u32,
    pub fetched_range_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

fn is_sqlite_corruption(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    [
        "file is not a database",
        "database disk image is malformed",
        "database corruption",
        "database malformed",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn preserve_companion_files(path: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string();
    ["-wal", "-shm"]
        .into_iter()
        .filter_map(|suffix| {
            let original = PathBuf::from(format!("{}{}", path.display(), suffix));
            original.exists().then_some(original)
        })
        .map(|original| {
            let file_name = original
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("cache path has no UTF-8 file name"))?;
            let parent = original
                .parent()
                .ok_or_else(|| anyhow::anyhow!("cache path has no parent directory"))?;
            let mut backup = parent.join(format!(".{file_name}.recovery-probe.{timestamp}"));
            let mut duplicate = 1_u32;
            while backup.exists() {
                backup = parent.join(format!(
                    ".{file_name}.recovery-probe.{timestamp}.{duplicate}"
                ));
                duplicate = duplicate.saturating_add(1);
            }
            std::fs::copy(&original, &backup).with_context(|| {
                format!(
                    "preserving cache companion {} before validation",
                    original.display()
                )
            })?;
            Ok((original, backup))
        })
        .collect()
}

fn discard_companion_backups(backups: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (_, backup) in backups {
        if backup.exists() {
            std::fs::remove_file(backup).with_context(|| {
                format!("discarding temporary cache backup {}", backup.display())
            })?;
        }
    }
    Ok(())
}

impl Cache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_checked(path)
    }

    /// Opens and validates a cache. Only SQLite-corruption failures are
    /// recovered by moving the cache aside; permissions and other filesystem
    /// failures remain actionable startup errors rather than risking data.
    pub fn open_with_recovery(path: impl AsRef<Path>) -> Result<(Self, Option<CacheRecovery>)> {
        let path = path.as_ref().to_path_buf();
        // Opening a corrupt database can cause SQLite to remove an invalid WAL
        // pair. Preserve copies before probing so quarantine retains every
        // user-visible cache artifact.
        let companion_backups = preserve_companion_files(&path)?;
        match Self::open_checked(&path) {
            Ok(cache) => {
                discard_companion_backups(&companion_backups)?;
                Ok((cache, None))
            }
            Err(error) if is_sqlite_corruption(&error) => {
                let recovery = Self::quarantine(&path, &companion_backups)?;
                let cache = Self::open_checked(&path)?;
                Ok((cache, Some(recovery)))
            }
            Err(error) => {
                discard_companion_backups(&companion_backups)?;
                Err(error)
            }
        }
    }

    fn open_checked(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cache = Self { path };
        // Validate before enabling WAL or applying migrations. SQLite may
        // otherwise clean up an invalid `-wal`/`-shm` pair before recovery
        // gets the chance to preserve it for inspection.
        cache.with_connection(|conn| {
            Self::integrity_check(conn)?;
            Self::migrate(conn)
        })?;
        Ok(cache)
    }

    fn quarantine(path: &Path, companion_backups: &[(PathBuf, PathBuf)]) -> Result<CacheRecovery> {
        let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string();
        let mut quarantined_paths = Vec::new();
        let mut move_to_quarantine = |source: &Path, file_name: &str| -> Result<()> {
            let parent = source
                .parent()
                .ok_or_else(|| anyhow::anyhow!("cache path has no parent directory"))?;
            let mut candidate = parent.join(format!("{file_name}.corrupt.{timestamp}"));
            let mut duplicate = 1_u32;
            while candidate.exists() {
                candidate = parent.join(format!("{file_name}.corrupt.{timestamp}.{duplicate}"));
                duplicate = duplicate.saturating_add(1);
            }
            std::fs::rename(source, &candidate)
                .with_context(|| format!("quarantining corrupted cache {}", source.display()))?;
            quarantined_paths.push(candidate);
            Ok(())
        };
        for suffix in [""] {
            let source = PathBuf::from(format!("{}{}", path.display(), suffix));
            if !source.exists() {
                continue;
            }
            let file_name = source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("cache path has no UTF-8 file name"))?;
            move_to_quarantine(&source, file_name)?;
        }
        for (original, backup) in companion_backups {
            let file_name = original
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("cache path has no UTF-8 file name"))?;
            if original.exists() {
                move_to_quarantine(original, file_name)?;
                std::fs::remove_file(backup).with_context(|| {
                    format!("discarding temporary cache backup {}", backup.display())
                })?;
            } else if backup.exists() {
                move_to_quarantine(backup, file_name)?;
            }
        }
        anyhow::ensure!(
            !quarantined_paths.is_empty(),
            "corrupted cache disappeared before it could be quarantined"
        );
        Ok(CacheRecovery { quarantined_paths })
    }

    pub fn schema_version(&self) -> Result<u32> {
        self.with_connection(
            |conn| Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?),
        )
    }

    pub fn health_check(&self) -> Result<()> {
        self.with_connection(Self::integrity_check)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        self.with_connection(|conn| {
            let event_count =
                conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            let calendar_count =
                conn.query_row("SELECT COUNT(*) FROM calendars", [], |row| row.get(0))?;
            let (oldest_event, newest_event) = conn.query_row(
                "SELECT MIN(starts_at), MAX(ends_at) FROM events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let fetched_range_count =
                conn.query_row("SELECT COUNT(*) FROM fetched_ranges", [], |row| row.get(0))?;
            Ok(CacheStats {
                bytes: std::fs::metadata(&self.path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                event_count,
                calendar_count,
                oldest_event,
                newest_event,
                schema_version: conn.query_row("PRAGMA user_version", [], |row| row.get(0))?,
                fetched_range_count,
            })
        })
    }

    pub fn vacuum(&self) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute_batch("VACUUM")?;
            Ok(())
        })
    }

    pub fn prune_outside(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<usize> {
        self.with_connection(|conn| {
            let transaction = conn.transaction()?;
            let removed = transaction.execute(
                "DELETE FROM events WHERE ends_at <= ?1 OR starts_at >= ?2",
                params![start.to_rfc3339(), end.to_rfc3339()],
            )?;
            transaction.execute(
                "DELETE FROM fetched_ranges WHERE ends_at <= ?1 OR starts_at >= ?2",
                params![start.to_rfc3339(), end.to_rfc3339()],
            )?;
            transaction.commit()?;
            Ok(removed)
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_connection<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = Connection::open(&self.path)
            .with_context(|| format!("opening cache {}", self.path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        f(&mut conn)
    }

    fn integrity_check(conn: &mut Connection) -> Result<()> {
        let status: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        anyhow::ensure!(status == "ok", "SQLite integrity check: {status}");
        Ok(())
    }

    fn migrate(conn: &mut Connection) -> Result<()> {
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            version <= CACHE_SCHEMA_VERSION,
            "cache schema version {version} is newer than this tui-calendar release (supports up to {CACHE_SCHEMA_VERSION}); update tui-calendar before opening this cache"
        );
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        if version < 1 {
            let transaction = conn.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS calendars (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    account TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    color TEXT NOT NULL,
                    is_writable INTEGER NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    raw_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS events (
                    id TEXT PRIMARY KEY,
                    calendar_id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    starts_at TEXT NOT NULL,
                    ends_at TEXT NOT NULL,
                    search_text TEXT NOT NULL,
                    raw_json TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS events_range_idx ON events(starts_at, ends_at);
                 CREATE INDEX IF NOT EXISTS events_calendar_idx ON events(calendar_id);
                 CREATE TABLE IF NOT EXISTS metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                 );
                 PRAGMA user_version=1;",
            )?;
            transaction.commit()?;
        }
        if version < 2 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS fetched_ranges (
                    starts_at TEXT NOT NULL,
                    ends_at TEXT NOT NULL,
                    fetched_at TEXT NOT NULL,
                    PRIMARY KEY(starts_at, ends_at)
                 );
                 CREATE INDEX IF NOT EXISTS fetched_ranges_cover_idx ON fetched_ranges(starts_at, ends_at);
                 PRAGMA user_version=2;",
            )?;
        }
        if version < 3 {
            // v2 stored EventKit's provider identifier as the primary key.
            // Recurring providers may reuse it for many concrete occurrences,
            // so those rows cannot be migrated without guessing an occurrence
            // identity. EventKit is authoritative: clear only disposable event
            // rows and coverage, retaining calendar metadata and user-enabled
            // visibility settings for the ordinary startup refresh.
            let transaction = conn.transaction()?;
            transaction.execute("DELETE FROM events", [])?;
            transaction.execute("DELETE FROM fetched_ranges", [])?;
            transaction.execute_batch("PRAGMA user_version=3;")?;
            transaction.commit()?;
        }
        Ok(())
    }

    pub fn load_snapshot(&self) -> Result<Snapshot> {
        self.with_connection(|conn| {
            let calendars = {
                let mut stmt = conn
                    .prepare("SELECT raw_json, enabled FROM calendars ORDER BY account, title")?;
                stmt.query_map([], |row| {
                    let raw: String = row.get(0)?;
                    let enabled: bool = row.get(1)?;
                    Ok((raw, enabled))
                })?
                .map(|row| -> Result<CalendarInfo> {
                    let (raw, enabled) = row?;
                    let mut calendar: CalendarInfo = serde_json::from_str(&raw)?;
                    calendar.enabled = enabled;
                    Ok(calendar)
                })
                .collect::<Result<Vec<_>>>()?
            };
            let events = {
                let mut stmt =
                    conn.prepare("SELECT raw_json FROM events ORDER BY starts_at, title")?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .map(|row| -> Result<Event> { Ok(serde_json::from_str(&row?)?) })
                    .collect::<Result<Vec<_>>>()?
            };
            let auth = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='authorization'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let updated_at = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='updated_at'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&Utc));
            Ok(Snapshot {
                calendars,
                events,
                authorization: auth,
                updated_at,
            })
        })
    }

    /// Reads the event rows currently persisted in SQLite that intersect a
    /// diagnostic day. This is intentionally read-only and evaluates trusted
    /// all-day membership from the serialized provider date range rather than
    /// inferring it from compatibility instants.
    pub fn events_intersecting_diagnostic_day(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        start_date: NaiveDate,
        end_date_exclusive: NaiveDate,
    ) -> Result<Vec<Event>> {
        self.with_connection(|conn| {
            let mut statement =
                conn.prepare("SELECT raw_json FROM events ORDER BY starts_at, title")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|row| -> Result<Option<Event>> {
                    let event: Event = serde_json::from_str(&row?)?;
                    let intersects = if event.all_day_date_range().is_some() {
                        event.all_day_intersects_dates(start_date, end_date_exclusive)
                    } else {
                        event.start < end && event.end > start
                    };
                    Ok(intersects.then_some(event))
                })
                .filter_map(|row| row.transpose())
                .collect()
        })
    }

    pub fn save_calendars(&self, calendars: &[CalendarInfo]) -> Result<()> {
        self.with_connection(|conn| {
            let transaction = conn.transaction()?;
            for calendar in calendars {
                transaction.execute(
                    "INSERT INTO calendars(id,title,account,provider,color,is_writable,enabled,raw_json)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(id) DO UPDATE SET title=excluded.title, account=excluded.account,
                       provider=excluded.provider, color=excluded.color,
                       is_writable=excluded.is_writable, raw_json=excluded.raw_json",
                    params![calendar.id, calendar.title, calendar.account, calendar.provider,
                        calendar.color, calendar.is_writable, calendar.enabled,
                        serde_json::to_string(calendar)?]
                )?;
            }
            let ids = calendars
                .iter()
                .map(|calendar| calendar.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            let stale = {
                let mut stmt = transaction.prepare("SELECT id FROM calendars")?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in stale.into_iter().filter(|id| !ids.contains(id.as_str())) {
                transaction.execute("DELETE FROM calendars WHERE id=?1", [&id])?;
                transaction.execute("DELETE FROM events WHERE calendar_id=?1", [&id])?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn set_calendar_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE calendars SET enabled=?2 WHERE id=?1",
                params![id, enabled],
            )?;
            Ok(())
        })
    }

    pub fn replace_events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        events: &[Event],
    ) -> Result<()> {
        self.with_connection(|conn| {
            let transaction = conn.transaction()?;
            transaction.execute(
                "DELETE FROM events WHERE starts_at < ?2 AND ends_at > ?1",
                params![start.to_rfc3339(), end.to_rfc3339()]
            )?;
            for event in events {
                let search_text = format!("{} {} {} {}", event.title, event.location, event.notes,
                    event.attendees.iter().map(|a| format!("{} {}", a.name, a.email)).collect::<Vec<_>>().join(" "))
                    .to_lowercase();
                transaction.execute(
                    "INSERT OR REPLACE INTO events(id,calendar_id,title,starts_at,ends_at,search_text,raw_json)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![event.id, event.calendar_id, event.title, event.start.to_rfc3339(),
                        event.end.to_rfc3339(), search_text, serde_json::to_string(event)?]
                )?;
            }
            transaction.execute("INSERT OR REPLACE INTO metadata(key,value) VALUES('updated_at',?1)",
                [Utc::now().to_rfc3339()])?;
            Self::record_fetched_range(&transaction, start, end)?;
            transaction.commit()?;
            Ok(())
        })
    }

    fn record_fetched_range(
        transaction: &rusqlite::Transaction<'_>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<()> {
        let overlapping = {
            let mut statement = transaction.prepare(
                "SELECT starts_at, ends_at FROM fetched_ranges WHERE starts_at <= ?1 AND ends_at >= ?2",
            )?;
            statement
                .query_map(params![end.to_rfc3339(), start.to_rfc3339()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut merged_start = start;
        let mut merged_end = end;
        for (range_start, range_end) in &overlapping {
            merged_start =
                merged_start.min(DateTime::parse_from_rfc3339(range_start)?.with_timezone(&Utc));
            merged_end =
                merged_end.max(DateTime::parse_from_rfc3339(range_end)?.with_timezone(&Utc));
        }
        transaction.execute(
            "DELETE FROM fetched_ranges WHERE starts_at <= ?1 AND ends_at >= ?2",
            params![end.to_rfc3339(), start.to_rfc3339()],
        )?;
        transaction.execute(
            "INSERT INTO fetched_ranges(starts_at, ends_at, fetched_at) VALUES(?1, ?2, ?3)",
            params![
                merged_start.to_rfc3339(),
                merged_end.to_rfc3339(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Returns true only after EventKit successfully fetched every part of a
    /// half-open `[start, end)` range. Empty fetched periods count as loaded.
    pub fn range_is_fetched(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<bool> {
        if end <= start {
            return Ok(false);
        }
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT starts_at, ends_at FROM fetched_ranges WHERE ends_at > ?1 AND starts_at < ?2 ORDER BY starts_at",
            )?;
            let ranges = statement
                .query_map(params![start.to_rfc3339(), end.to_rfc3339()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut covered_until = start;
            for (range_start, range_end) in ranges {
                let range_start = DateTime::parse_from_rfc3339(&range_start)?.with_timezone(&Utc);
                let range_end = DateTime::parse_from_rfc3339(&range_end)?.with_timezone(&Utc);
                if range_start > covered_until {
                    return Ok(false);
                }
                covered_until = covered_until.max(range_end);
                if covered_until >= end {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    /// Returns uncovered half-open intervals within `[start, end)`. The range
    /// loader can issue EventKit queries only for these gaps, including when a
    /// successfully fetched interval contained no events.
    pub fn missing_ranges(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<DateRange>> {
        if end <= start {
            return Ok(vec![]);
        }
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT starts_at, ends_at FROM fetched_ranges WHERE ends_at > ?1 AND starts_at < ?2 ORDER BY starts_at",
            )?;
            let ranges = statement
                .query_map(params![start.to_rfc3339(), end.to_rfc3339()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut cursor = start;
            let mut missing = Vec::new();
            for (range_start, range_end) in ranges {
                let range_start = DateTime::parse_from_rfc3339(&range_start)?.with_timezone(&Utc);
                let range_end = DateTime::parse_from_rfc3339(&range_end)?.with_timezone(&Utc);
                if range_start > cursor {
                    missing.push(DateRange { start: cursor, end: range_start.min(end) });
                }
                cursor = cursor.max(range_end);
                if cursor >= end {
                    break;
                }
            }
            if cursor < end {
                missing.push(DateRange { start: cursor, end });
            }
            Ok(missing.into_iter().filter(|range| range.end > range.start).collect())
        })
    }

    /// Returns the compact, successfully fetched coverage that intersects the
    /// future suffix beginning at `start`. Each returned interval is clipped
    /// to `start`; unloaded gaps are deliberately absent.
    pub fn fetched_ranges_from(&self, start: DateTime<Utc>) -> Result<Vec<DateRange>> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT starts_at, ends_at FROM fetched_ranges WHERE ends_at > ?1 ORDER BY starts_at, ends_at",
            )?;
            statement
                .query_map([start.to_rfc3339()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .map(|row| {
                    let (range_start, range_end) = row?;
                    Ok(DateRange {
                        start: DateTime::parse_from_rfc3339(&range_start)?
                            .with_timezone(&Utc)
                            .max(start),
                        end: DateTime::parse_from_rfc3339(&range_end)?.with_timezone(&Utc),
                    })
                })
                .collect()
        })
    }

    pub fn save_authorization(&self, status: AuthorizationStatus) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES('authorization',?1)",
                [serde_json::to_string(&status)?],
            )?;
            Ok(())
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Event>> {
        let words = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        if words.is_empty() {
            return Ok(vec![]);
        }
        self.with_connection(|conn| {
            let mut stmt =
                conn.prepare("SELECT raw_json, search_text FROM events ORDER BY starts_at")?;
            let mut found = Vec::new();
            for row in stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (raw, text) = row?;
                if words.iter().all(|word| text.contains(word)) {
                    found.push(serde_json::from_str(&raw)?);
                    if found.len() >= limit {
                        break;
                    }
                }
            }
            Ok(found)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Availability, InvitationStatus, RecurrenceFrequency, RecurrenceRule, TimeZoneProvenance,
    };

    fn event(title: &str) -> Event {
        Event {
            id: "event-1".into(),
            provider_id: Some("event-1".into()),
            series_id: None,
            calendar_id: "cal-1".into(),
            title: title.into(),
            start: "2026-08-21T09:00:00Z".parse().unwrap(),
            end: "2026-08-21T10:00:00Z".parse().unwrap(),
            all_day: false,
            all_day_start_date: None,
            all_day_end_date_exclusive: None,
            location: "Berlin".into(),
            notes: "migration plan".into(),
            url: String::new(),
            time_zone: "Europe/Berlin".into(),
            time_zone_provenance: TimeZoneProvenance::Unknown,
            availability: Availability::Busy,
            organizer: None,
            attendees: vec![],
            alarms: vec![],
            recurrence: vec![],
            has_recurrence: false,
            is_detached: false,
            invitation_status: InvitationStatus::Unknown,
        }
    }

    #[test]
    fn snapshot_round_trip_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let calendar = CalendarInfo {
            id: "cal-1".into(),
            source_id: "source-1".into(),
            permissions: crate::model::CalendarPermissions::default(),
            title: "Work".into(),
            account: "iCloud".into(),
            provider: "iCloud".into(),
            color: "#3366FF".into(),
            is_writable: true,
            enabled: true,
        };
        cache.save_calendars(&[calendar]).unwrap();
        let events = (0..10)
            .map(|index| {
                let mut event = event(&format!("Architecture review {index}"));
                event.id = format!("event-{index}");
                event
            })
            .collect::<Vec<_>>();
        cache
            .replace_events(
                "2026-01-01T00:00:00Z".parse().unwrap(),
                "2027-01-01T00:00:00Z".parse().unwrap(),
                &events,
            )
            .unwrap();
        let snapshot = cache.load_snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 10);
        assert_eq!(cache.search("migration berlin", 10).unwrap().len(), 10);
    }

    #[test]
    fn diagnostic_day_query_reads_timed_and_trusted_all_day_rows_without_guessing_dates() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let mut timed = event("Timed");
        timed.id = "timed".into();
        timed.start = "2026-08-27T08:00:00Z".parse().unwrap();
        timed.end = "2026-08-27T09:00:00Z".parse().unwrap();
        let mut all_day = event("All day");
        all_day.id = "all-day".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(NaiveDate::from_ymd_opt(2026, 8, 27).unwrap());
        all_day.all_day_end_date_exclusive = Some(NaiveDate::from_ymd_opt(2026, 8, 28).unwrap());
        // Compatibility instants intentionally fall outside the diagnostic
        // transport day, so this proves the query uses trusted date identity.
        all_day.start = "2026-08-26T00:00:00Z".parse().unwrap();
        all_day.end = "2026-08-26T23:59:59Z".parse().unwrap();
        cache
            .replace_events(
                "2026-08-01T00:00:00Z".parse().unwrap(),
                "2026-09-01T00:00:00Z".parse().unwrap(),
                &[timed, all_day],
            )
            .unwrap();

        let events = cache
            .events_intersecting_diagnostic_day(
                "2026-08-27T00:00:00Z".parse().unwrap(),
                "2026-08-28T00:00:00Z".parse().unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 27).unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 28).unwrap(),
            )
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            ["all-day", "timed"]
        );
    }

    #[test]
    fn all_day_date_metadata_survives_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let mut all_day = event("All-day metadata");
        all_day.all_day = true;
        // These intentionally do not identify the trusted calendar dates.
        // The raw payload must preserve provider-normalized date identity
        // through cache reload without consulting compatibility instants.
        all_day.start = "2026-09-09T00:00:00Z".parse().unwrap();
        all_day.end = "2026-09-10T00:00:00Z".parse().unwrap();
        all_day.all_day_start_date = Some(chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap());
        all_day.all_day_end_date_exclusive =
            Some(chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap());
        all_day.has_recurrence = true;
        all_day.recurrence = vec![RecurrenceRule {
            frequency: RecurrenceFrequency::Daily,
            interval: 1,
            days_of_week: vec![],
            occurrence_count: None,
            end_date: None,
        }];
        cache
            .replace_events(
                "2026-09-01T00:00:00Z".parse().unwrap(),
                "2026-10-01T00:00:00Z".parse().unwrap(),
                &[all_day],
            )
            .unwrap();
        let event = cache.load_snapshot().unwrap().events.remove(0);
        assert_eq!(
            event.all_day_date_range(),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            ))
        );
        assert!(event.has_recurrence);
        assert_eq!(event.recurrence.len(), 1);
        assert_eq!(event.recurrence[0].frequency, RecurrenceFrequency::Daily);
    }

    #[test]
    fn initializes_a_versioned_schema_and_passes_integrity_check() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        assert_eq!(cache.schema_version().unwrap(), CACHE_SCHEMA_VERSION);
        cache.health_check().unwrap();
    }

    #[test]
    fn migrates_a_v1_cache_without_losing_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        Cache::open(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE fetched_ranges; PRAGMA user_version=1;")
            .unwrap();

        let cache = Cache::open(&path).unwrap();
        assert_eq!(cache.schema_version().unwrap(), CACHE_SCHEMA_VERSION);
        cache
            .replace_events(
                "2026-01-01T00:00:00Z".parse().unwrap(),
                "2026-02-01T00:00:00Z".parse().unwrap(),
                &[],
            )
            .unwrap();
        assert!(
            cache
                .range_is_fetched(
                    "2026-01-01T00:00:00Z".parse().unwrap(),
                    "2026-02-01T00:00:00Z".parse().unwrap(),
                )
                .unwrap()
        );
    }

    #[test]
    fn refuses_a_future_schema_without_quarantining_or_replacing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        Cache::open(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version=4;")
            .unwrap();

        let error = Cache::open_with_recovery(&path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("newer than this tui-calendar release")
        );
        assert!(path.is_file());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".corrupt.")
        }));
    }

    #[test]
    fn quarantines_corrupted_database_and_companion_files_before_recreating_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite3");
        std::fs::write(&path, "not a sqlite database").unwrap();
        std::fs::write(format!("{}-wal", path.display()), "bad wal").unwrap();
        std::fs::write(format!("{}-shm", path.display()), "bad shm").unwrap();

        let (cache, recovery) = Cache::open_with_recovery(&path).unwrap();
        cache.health_check().unwrap();
        assert!(
            path.is_file(),
            "a fresh cache replaces the quarantined file"
        );
        let recovery = recovery.expect("corruption must be reported to the caller");
        assert_eq!(recovery.quarantined_paths.len(), 3);
        for path in recovery.quarantined_paths {
            assert!(path.is_file());
            assert!(path.to_string_lossy().contains(".corrupt."));
        }
        assert!(!std::path::PathBuf::from(format!("{}-wal", path.display())).exists());
        assert!(!std::path::PathBuf::from(format!("{}-shm", path.display())).exists());
    }

    #[test]
    fn prune_removes_events_outside_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let mut old = event("old");
        old.id = "old".into();
        old.start = "2020-01-01T09:00:00Z".parse().unwrap();
        old.end = "2020-01-01T10:00:00Z".parse().unwrap();
        let mut current = event("current");
        current.id = "current".into();
        cache
            .replace_events(
                "2019-01-01T00:00:00Z".parse().unwrap(),
                "2027-01-01T00:00:00Z".parse().unwrap(),
                &[old, current],
            )
            .unwrap();
        let removed = cache
            .prune_outside(
                "2026-01-01T00:00:00Z".parse().unwrap(),
                "2027-01-01T00:00:00Z".parse().unwrap(),
            )
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(cache.load_snapshot().unwrap().events.len(), 1);
    }

    #[test]
    fn records_successfully_fetched_empty_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let start: DateTime<Utc> = "2028-09-01T00:00:00Z".parse().unwrap();
        let end: DateTime<Utc> = "2028-10-01T00:00:00Z".parse().unwrap();
        cache.replace_events(start, end, &[]).unwrap();
        assert!(cache.range_is_fetched(start, end).unwrap());
        assert_eq!(cache.stats().unwrap().fetched_range_count, 1);
    }

    #[test]
    fn authoritative_refresh_preserves_an_overlapping_multiday_event() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let mut overnight = event("overnight");
        overnight.start = "2026-08-23T23:00:00Z".parse().unwrap();
        overnight.end = "2026-08-24T02:00:00Z".parse().unwrap();
        let start = "2026-08-24T00:00:00Z".parse().unwrap();
        let end = "2026-08-25T00:00:00Z".parse().unwrap();

        // EventKit returns events that overlap the predicate interval, even
        // where their start falls before the interval being reconciled.
        cache
            .replace_events(start, end, &[overnight.clone()])
            .unwrap();
        let snapshot = cache.load_snapshot().unwrap();
        assert_eq!(snapshot.events, vec![overnight]);
    }

    #[test]
    fn coverage_detects_gaps_and_merges_adjacent_fetches() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let first_start: DateTime<Utc> = "2028-09-01T00:00:00Z".parse().unwrap();
        let first_end: DateTime<Utc> = "2028-09-10T00:00:00Z".parse().unwrap();
        let second_start: DateTime<Utc> = "2028-09-20T00:00:00Z".parse().unwrap();
        let second_end: DateTime<Utc> = "2028-10-01T00:00:00Z".parse().unwrap();
        cache.replace_events(first_start, first_end, &[]).unwrap();
        cache.replace_events(second_start, second_end, &[]).unwrap();
        assert!(!cache.range_is_fetched(first_start, second_end).unwrap());
        cache.replace_events(first_end, second_start, &[]).unwrap();
        assert!(cache.range_is_fetched(first_start, second_end).unwrap());
        assert_eq!(cache.stats().unwrap().fetched_range_count, 1);
    }

    #[test]
    fn fetched_ranges_from_clips_loaded_coverage_without_filling_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let first_start: DateTime<Utc> = "2026-09-01T00:00:00Z".parse().unwrap();
        let first_end: DateTime<Utc> = "2026-09-10T00:00:00Z".parse().unwrap();
        let second_start: DateTime<Utc> = "2026-09-20T00:00:00Z".parse().unwrap();
        let second_end: DateTime<Utc> = "2026-10-01T00:00:00Z".parse().unwrap();
        cache.replace_events(first_start, first_end, &[]).unwrap();
        cache.replace_events(second_start, second_end, &[]).unwrap();
        let affected: DateTime<Utc> = "2026-09-05T00:00:00Z".parse().unwrap();
        assert_eq!(
            cache.fetched_ranges_from(affected).unwrap(),
            vec![
                DateRange {
                    start: affected,
                    end: first_end
                },
                DateRange {
                    start: second_start,
                    end: second_end
                },
            ]
        );
        assert!(!cache.range_is_fetched(first_end, second_start).unwrap());
    }

    #[test]
    fn returns_only_the_missing_gap_for_partially_covered_range() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let requested_start: DateTime<Utc> = "2028-09-01T00:00:00Z".parse().unwrap();
        let requested_end: DateTime<Utc> = "2028-10-01T00:00:00Z".parse().unwrap();
        cache
            .replace_events(
                requested_start,
                "2028-09-10T00:00:00Z".parse().unwrap(),
                &[],
            )
            .unwrap();
        cache
            .replace_events("2028-09-20T00:00:00Z".parse().unwrap(), requested_end, &[])
            .unwrap();
        assert_eq!(
            cache
                .missing_ranges(requested_start, requested_end)
                .unwrap(),
            vec![DateRange {
                start: "2028-09-10T00:00:00Z".parse().unwrap(),
                end: "2028-09-20T00:00:00Z".parse().unwrap(),
            }]
        );
    }

    #[test]
    fn keeps_user_calendar_visibility_during_metadata_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let mut calendar = CalendarInfo {
            id: "cal-1".into(),
            source_id: "source-1".into(),
            permissions: crate::model::CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: "Local".into(),
            color: "#fff".into(),
            is_writable: true,
            enabled: true,
        };
        cache.save_calendars(&[calendar.clone()]).unwrap();
        cache.set_calendar_enabled("cal-1", false).unwrap();
        calendar.title = "Renamed".into();
        cache.save_calendars(&[calendar]).unwrap();
        let snapshot = cache.load_snapshot().unwrap();
        assert!(!snapshot.calendars[0].enabled);
        assert_eq!(snapshot.calendars[0].title, "Renamed");
    }

    #[test]
    fn metadata_refresh_prunes_deleted_calendar_visibility_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let calendar = CalendarInfo {
            id: "cal-delete".into(),
            source_id: "source-1".into(),
            permissions: crate::model::CalendarPermissions::default(),
            title: "Delete Me".into(),
            account: String::new(),
            provider: "Local".into(),
            color: "#fff".into(),
            is_writable: true,
            enabled: true,
        };
        cache.save_calendars(&[calendar]).unwrap();
        cache.set_calendar_enabled("cal-delete", false).unwrap();
        let mut deleted_event = event("orphan");
        deleted_event.calendar_id = "cal-delete".into();
        cache
            .replace_events(
                "2026-01-01T00:00:00Z".parse().unwrap(),
                "2027-01-01T00:00:00Z".parse().unwrap(),
                &[deleted_event],
            )
            .unwrap();

        // A non-empty authoritative calendar list means the missing ID was
        // removed by EventKit, so both local visibility and orphaned events
        // are reconciled by the shared metadata refresh path.
        let replacement = CalendarInfo {
            id: "cal-remaining".into(),
            source_id: "source-1".into(),
            permissions: crate::model::CalendarPermissions::default(),
            title: "Remaining".into(),
            account: String::new(),
            provider: "Local".into(),
            color: "#fff".into(),
            is_writable: true,
            enabled: true,
        };
        cache.save_calendars(&[replacement]).unwrap();
        let snapshot = cache.load_snapshot().unwrap();
        assert_eq!(snapshot.calendars.len(), 1);
        assert_eq!(snapshot.calendars[0].id, "cal-remaining");
        assert!(snapshot.events.is_empty());
    }

    #[test]
    fn broad_refresh_keeps_each_recurring_occurrence_with_a_shared_provider_id() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        let start: DateTime<Utc> = "2026-08-20T00:00:00Z".parse().unwrap();
        let end: DateTime<Utc> = "2026-09-11T00:00:00Z".parse().unwrap();
        let events = [0_i64, 1, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15]
            .into_iter()
            .map(|offset| {
                let mut occurrence = event("Dev Stand-up");
                occurrence.provider_id = Some("dev-provider".into());
                occurrence.series_id = Some("dev-series".into());
                occurrence.has_recurrence = true;
                occurrence.start =
                    start + chrono::Duration::days(offset) + chrono::Duration::hours(7);
                occurrence.end = occurrence.start + chrono::Duration::minutes(15);
                occurrence.id = format!("occ-v1:dev-provider:{:020}", occurrence.start.timestamp());
                occurrence
            })
            .collect::<Vec<_>>();
        cache.replace_events(start, end, &events).unwrap();
        let snapshot = cache.load_snapshot().unwrap();
        assert_eq!(snapshot.events.len(), events.len());
        assert_eq!(cache.stats().unwrap().event_count as usize, events.len());
        assert!(snapshot.events.iter().any(|event| {
            event.start == "2026-08-27T07:00:00Z".parse::<DateTime<Utc>>().unwrap()
                && event.provider_id.as_deref() == Some("dev-provider")
        }));
        // Replaying the same broad authoritative response is idempotent.
        cache.replace_events(start, end, &events).unwrap();
        assert_eq!(cache.stats().unwrap().event_count as usize, events.len());
    }
}
