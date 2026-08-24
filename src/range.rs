//! Shared EventKit-to-cache range loading.
//!
//! All intervals use half-open `[start, end)` UTC semantics. The loader fetches
//! all accessible calendars, then leaves calendar visibility as a local UI
//! filter; that makes `fetched_ranges` independent of sidebar state.

use crate::{
    backend::CalendarBackend,
    cache::{Cache, DateRange},
    model::{CalendarDateRange, FetchRequest, InstantRange, Snapshot},
};
use chrono::{DateTime, Utc};
use std::sync::Arc;

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
            self.cache
                .replace_events(gap.start, gap.end, &events)
                .map_err(|error| error.to_string())?;
        }
        self.cache
            .load_snapshot()
            .map_err(|error| error.to_string())
    }
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
}
