//! Shared EventKit-to-cache range loading.
//!
//! All intervals use half-open `[start, end)` UTC semantics. The loader fetches
//! all accessible calendars, then leaves calendar visibility as a local UI
//! filter; that makes `fetched_ranges` independent of sidebar state.

use crate::{
    backend::CalendarBackend,
    cache::{Cache, DateRange},
    model::{CalendarDateRange, Event, FetchRequest, InstantRange, Snapshot},
};
use chrono::{DateTime, Utc};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static PIPELINE_TRACE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeReason {
    VisibleDay,
    VisibleWeek,
    VisibleMonth,
    AgendaPage,
    SearchTarget,
    Preload,
    EventKitChange,
    BackgroundRefresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RangePriority {
    Critical,
    Interactive,
    Preload,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeRequest {
    pub id: u64,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Calendar-date inclusion required by a visible all-day view. Cache
    /// coverage remains keyed only by the UTC transport interval.
    pub all_day_range: Option<CalendarDateRange>,
    pub reason: RangeReason,
    pub priority: RangePriority,
}

pub struct RangeLoader {
    backend: Arc<dyn CalendarBackend>,
    cache: Cache,
}

impl RangeLoader {
    pub fn new(backend: Arc<dyn CalendarBackend>, cache: Cache) -> Self {
        Self { backend, cache }
    }

    /// Fetches only uncovered gaps. A successful empty response is committed as
    /// coverage by `Cache::replace_events`, preventing repeat loads.
    pub async fn ensure_range(&self, request: RangeRequest) -> Result<Snapshot, String> {
        let gaps = self
            .cache
            .missing_ranges(request.start, request.end)
            .map_err(|error| error.to_string())?;
        self.load_gaps(gaps, request.all_day_range).await
    }

    /// Reconciles an authoritative range even if it was fetched previously.
    /// Used by explicit/notification refreshes, while ordinary navigation uses
    /// `ensure_range` to avoid duplicate EventKit queries.
    pub async fn refresh_range(&self, request: RangeRequest) -> Result<Snapshot, String> {
        self.load_gaps(
            vec![DateRange {
                start: request.start,
                end: request.end,
            }],
            request.all_day_range,
        )
        .await
    }

    async fn load_gaps(
        &self,
        gaps: Vec<DateRange>,
        all_day_range: Option<CalendarDateRange>,
    ) -> Result<Snapshot, String> {
        if gaps.is_empty() {
            return self
                .cache
                .load_snapshot()
                .map_err(|error| error.to_string());
        }
        let calendars = self
            .backend
            .calendars()
            .await
            .map_err(|error| error.to_string())?;
        self.cache
            .save_calendars(&calendars)
            .map_err(|error| error.to_string())?;
        let ids = calendars
            .iter()
            .map(|calendar| calendar.id.clone())
            .collect::<Vec<_>>();
        for gap in gaps {
            let trace = PIPELINE_TRACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let pipeline_debug = std::env::var_os("TUI_CALENDAR_DEBUG_PIPELINE").is_some();
            let cache_before = if pipeline_debug {
                self.cache
                    .load_snapshot()
                    .ok()
                    .map(|snapshot| snapshot.events.len())
            } else {
                None
            };
            let events = self
                .backend
                .fetch_events(
                    FetchRequest {
                        instant_range: InstantRange {
                            start: gap.start,
                            end: gap.end,
                        },
                        all_day_range,
                    },
                    &ids,
                )
                .await
                .map_err(|error| error.to_string())?;
            pipeline_log(
                trace,
                "backend",
                gap.start,
                gap.end,
                &events,
                cache_before,
                None,
            );
            cache_identity_log("before", gap.start, gap.end, &events, None);
            self.cache
                .replace_events(gap.start, gap.end, &events)
                .map_err(|error| error.to_string())?;
            if std::env::var_os("TUI_CALENDAR_DEBUG_IDENTITY").is_some() || pipeline_debug {
                let snapshot = self
                    .cache
                    .load_snapshot()
                    .map_err(|error| error.to_string())?;
                cache_identity_log("after", gap.start, gap.end, &events, Some(&snapshot.events));
                pipeline_log(
                    trace,
                    "cache",
                    gap.start,
                    gap.end,
                    &events,
                    cache_before,
                    Some(&snapshot.events),
                );
            }
        }
        self.cache
            .load_snapshot()
            .map_err(|error| error.to_string())
    }
}

fn pipeline_log(
    trace: u64,
    phase: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    events: &[Event],
    cache_before: Option<usize>,
    persisted: Option<&[Event]>,
) {
    if std::env::var_os("TUI_CALENDAR_DEBUG_PIPELINE").is_none() {
        return;
    }
    let unique = events
        .iter()
        .map(|event| &event.id)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let recurring = events.iter().filter(|event| event.has_recurrence).count();
    eprintln!(
        "tui-calendar pipeline trace={trace} phase={phase} requested=[{}, {}) backend_events={} backend_unique_ids={} recurring_events={} cache_before={} cache_after={} cache_write_mode=replace fetched_range_recorded={}",
        start.to_rfc3339(),
        end.to_rfc3339(),
        events.len(),
        unique,
        recurring,
        cache_before.map_or_else(|| "unavailable".into(), |count| count.to_string()),
        persisted.map_or_else(|| "pending".into(), |items| items.len().to_string()),
        if phase == "cache" { "true" } else { "false" }
    );
    let date = std::env::var("TUI_CALENDAR_DEBUG_PIPELINE_DATE")
        .ok()
        .and_then(|value| value.parse::<chrono::NaiveDate>().ok());
    if let Some(date) = date {
        for event in events
            .iter()
            .filter(|event| event.start.date_naive() == date)
        {
            eprintln!(
                "tui-calendar pipeline trace={trace} phase={phase} event title={:?} id={} provider_id={} series_id={} start={} calendar_id={}",
                event.title,
                event.id,
                event.provider_id.as_deref().unwrap_or("unavailable"),
                event.series_id.as_deref().unwrap_or("unavailable"),
                event.start.to_rfc3339(),
                event.calendar_id
            );
        }
    }
}

/// Opt-in persistence diagnostic for recurring-occurrence identity audits.
/// It deliberately reports only provider-derived IDs and canonical transport
/// instants; it neither changes fetch coverage nor cache behavior.
fn cache_identity_log(
    phase: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    incoming: &[Event],
    persisted: Option<&[Event]>,
) {
    if std::env::var_os("TUI_CALENDAR_DEBUG_IDENTITY").is_none() {
        return;
    }
    let mut by_id = std::collections::BTreeMap::<&str, Vec<&Event>>::new();
    for event in incoming {
        by_id.entry(&event.id).or_default().push(event);
    }
    eprintln!(
        "tui-calendar identity phase={phase} range=[{}, {}) incoming_events={} unique_ids={}",
        start.to_rfc3339(),
        end.to_rfc3339(),
        incoming.len(),
        by_id.len(),
    );
    for (id, occurrences) in by_id.iter().filter(|(_, events)| events.len() > 1) {
        eprintln!(
            "tui-calendar identity duplicate id={id} occurrences={}",
            occurrences.len()
        );
        for event in occurrences {
            eprintln!(
                "  start={} end={} provider_id={} series_id={} calendar={} title={}",
                event.start.to_rfc3339(),
                event.end.to_rfc3339(),
                event.provider_id.as_deref().unwrap_or("unavailable"),
                event.series_id.as_deref().unwrap_or("unavailable"),
                event.calendar_id,
                event.title.replace(['\n', '\r'], " "),
            );
        }
    }
    let Some(persisted) = persisted else {
        return;
    };
    let surviving = incoming
        .iter()
        .filter(|candidate| {
            persisted.iter().any(|event| {
                event.id == candidate.id
                    && event.start == candidate.start
                    && event.end == candidate.end
            })
        })
        .count();
    eprintln!(
        "tui-calendar identity phase=after incoming_occurrences_surviving={} incoming_occurrences_missing={}",
        surviving,
        incoming.len().saturating_sub(surviving),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;

    #[tokio::test]
    async fn ensure_range_records_coverage_for_the_requested_interval() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::open(directory.path().join("cache.db")).unwrap();
        let backend = Arc::new(MockBackend::seeded());
        let loader = RangeLoader::new(backend, cache.clone());
        let request = RangeRequest {
            id: 1,
            start: "2030-01-01T00:00:00Z".parse().unwrap(),
            end: "2030-02-01T00:00:00Z".parse().unwrap(),
            all_day_range: None,
            reason: RangeReason::VisibleMonth,
            priority: RangePriority::Interactive,
        };
        loader.ensure_range(request).await.unwrap();
        assert!(cache.range_is_fetched(request.start, request.end).unwrap());
        assert_eq!(cache.stats().unwrap().fetched_range_count, 1);
    }

    #[tokio::test]
    async fn trusted_all_day_fixture_survives_fetch_and_cache_despite_shifted_instants() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::open(directory.path().join("cache.db")).unwrap();
        let backend = Arc::new(MockBackend::seeded());
        let loader = RangeLoader::new(backend, cache.clone());
        let request = RangeRequest {
            id: 2,
            start: "2026-09-10T00:00:00Z".parse().unwrap(),
            end: "2026-09-11T00:00:00Z".parse().unwrap(),
            all_day_range: Some(CalendarDateRange {
                start_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: chrono::NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            }),
            reason: RangeReason::VisibleDay,
            priority: RangePriority::Critical,
        };
        let snapshot = loader.ensure_range(request).await.unwrap();
        let event = snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-all-day-shifted")
            .unwrap();
        assert_eq!(
            event.all_day_date_range(),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            ))
        );
        assert!(cache.range_is_fetched(request.start, request.end).unwrap());
    }

    #[tokio::test]
    async fn authoritative_refresh_replaces_a_complete_but_stale_day_with_all_provider_events() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::open(directory.path().join("cache.db")).unwrap();
        let backend = Arc::new(MockBackend::seeded());
        let request = RangeRequest {
            id: 3,
            start: "2026-08-27T00:00:00Z".parse().unwrap(),
            end: "2026-08-28T00:00:00Z".parse().unwrap(),
            all_day_range: Some(CalendarDateRange {
                start_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap(),
                end_date_exclusive: chrono::NaiveDate::from_ymd_opt(2026, 8, 28).unwrap(),
            }),
            reason: RangeReason::VisibleDay,
            priority: RangePriority::Interactive,
        };
        let template = backend
            .events(
                chrono::Utc::now() - chrono::Duration::days(7),
                chrono::Utc::now() + chrono::Duration::days(7),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| !event.all_day)
            .unwrap();
        let provider_events = (0..8)
            .map(|index| {
                let mut event = template.clone();
                event.id = format!("refreshed-{index}");
                event.start = format!("2026-08-27T{:02}:00:00Z", 8 + index)
                    .parse()
                    .unwrap();
                event.end = format!("2026-08-27T{:02}:30:00Z", 8 + index)
                    .parse()
                    .unwrap();
                event
            })
            .collect::<Vec<_>>();
        cache
            .replace_events(request.start, request.end, &provider_events[..4])
            .unwrap();
        assert!(cache.range_is_fetched(request.start, request.end).unwrap());
        backend.set_events_for_test(provider_events);

        let snapshot = RangeLoader::new(backend, cache.clone())
            .refresh_range(request)
            .await
            .unwrap();
        assert_eq!(snapshot.events.len(), 8);
        assert_eq!(cache.load_snapshot().unwrap().events.len(), 8);
    }
}
