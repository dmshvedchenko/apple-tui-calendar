//! Pure calendar geometry used by the Day and Week renderers.
//!
//! The module deliberately contains no Ratatui code. That keeps overlap and
//! clipping behaviour deterministic, cheap to test, and independent of a
//! terminal's current size.

use chrono::{DateTime, Duration, Local, NaiveDate, Timelike, Utc};

pub const MINUTES_PER_DAY: u16 = 24 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineItem {
    pub event_index: usize,
    pub start_minute: u16,
    pub end_minute: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionedItem {
    pub event_index: usize,
    pub start_minute: u16,
    pub end_minute: u16,
    pub column: u16,
    pub columns: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineViewport {
    pub start_minute: u16,
    pub minutes_per_row: u16,
    pub rows: u16,
}

impl TimelineViewport {
    pub fn end_minute(self) -> u16 {
        self.start_minute
            .saturating_add(self.minutes_per_row.saturating_mul(self.rows))
            .min(MINUTES_PER_DAY)
    }

    pub fn row_for(self, minute: u16) -> Option<u16> {
        if minute < self.start_minute || minute >= self.end_minute() {
            return None;
        }
        Some((minute - self.start_minute) / self.minutes_per_row)
    }

    pub fn rows_for_range(self, start: u16, end: u16) -> Option<(u16, u16)> {
        let visible_start = start.max(self.start_minute);
        let visible_end = end.min(self.end_minute());
        if visible_end <= visible_start {
            return None;
        }
        let row = (visible_start - self.start_minute) / self.minutes_per_row;
        let last_row = (visible_end - 1 - self.start_minute) / self.minutes_per_row;
        Some((row, last_row.saturating_sub(row).saturating_add(1)))
    }
}

/// Clips a timed event to one local calendar day. All-day events are handled
/// separately by the renderer and return `None` here.
pub fn item_for_day(
    event_index: usize,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    all_day: bool,
    day: NaiveDate,
) -> Option<TimelineItem> {
    if all_day || end <= start {
        return None;
    }
    let day_start = day
        .and_hms_opt(0, 0, 0)?
        .and_local_timezone(Local)
        .earliest()?
        .with_timezone(&Utc);
    let next_day = day + Duration::days(1);
    let day_end = next_day
        .and_hms_opt(0, 0, 0)?
        .and_local_timezone(Local)
        .earliest()?
        .with_timezone(&Utc);
    if end <= day_start || start >= day_end {
        return None;
    }
    let local_start = start.max(day_start).with_timezone(&Local);
    let local_end = end.min(day_end).with_timezone(&Local);
    let start_minute = if start <= day_start {
        0
    } else {
        (local_start.hour() * 60 + local_start.minute()) as u16
    };
    let end_minute = if end >= day_end {
        MINUTES_PER_DAY
    } else {
        (local_end.hour() * 60 + local_end.minute()).min(MINUTES_PER_DAY as u32) as u16
    };
    (end_minute > start_minute).then_some(TimelineItem {
        event_index,
        start_minute,
        end_minute,
    })
}

/// Assigns each intersecting event a column. Events that only touch at their
/// endpoints share a column; intersecting connected components use the number
/// of columns required by their densest overlap.
pub fn layout_overlaps(items: &[TimelineItem]) -> Vec<PositionedItem> {
    let mut sorted = items.to_vec();
    sorted.sort_by_key(|item| (item.start_minute, item.end_minute, item.event_index));
    let mut output = Vec::with_capacity(sorted.len());
    let mut component_start = 0;
    while component_start < sorted.len() {
        let mut component_end = component_start + 1;
        let mut furthest_end = sorted[component_start].end_minute;
        while component_end < sorted.len() && sorted[component_end].start_minute < furthest_end {
            furthest_end = furthest_end.max(sorted[component_end].end_minute);
            component_end += 1;
        }

        let mut column_ends: Vec<u16> = Vec::new();
        let position_start = output.len();
        for item in &sorted[component_start..component_end] {
            let column = column_ends
                .iter()
                .position(|end| *end <= item.start_minute)
                .unwrap_or_else(|| {
                    column_ends.push(0);
                    column_ends.len() - 1
                });
            column_ends[column] = item.end_minute;
            output.push(PositionedItem {
                event_index: item.event_index,
                start_minute: item.start_minute,
                end_minute: item.end_minute,
                column: column as u16,
                columns: 0,
            });
        }
        let columns = column_ends.len() as u16;
        for positioned in &mut output[position_start..] {
            positioned.columns = columns;
        }
        component_start = component_end;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize, start: u16, end: u16) -> TimelineItem {
        TimelineItem {
            event_index: index,
            start_minute: start,
            end_minute: end,
        }
    }

    #[test]
    fn lays_overlapping_events_side_by_side() {
        let layout = layout_overlaps(&[item(0, 9 * 60, 10 * 60), item(1, 9 * 60 + 30, 11 * 60)]);
        assert_eq!(layout.len(), 2);
        assert_eq!(layout[0].columns, 2);
        assert_eq!(layout[1].columns, 2);
        assert_ne!(layout[0].column, layout[1].column);
    }

    #[test]
    fn lets_adjacent_events_reuse_a_column() {
        let layout = layout_overlaps(&[item(0, 9 * 60, 10 * 60), item(1, 10 * 60, 11 * 60)]);
        assert_eq!(layout[0].columns, 1);
        assert_eq!(layout[1].columns, 1);
        assert_eq!(layout[0].column, 0);
        assert_eq!(layout[1].column, 0);
    }

    #[test]
    fn viewport_clips_event_to_visible_rows() {
        let viewport = TimelineViewport {
            start_minute: 8 * 60,
            minutes_per_row: 30,
            rows: 8,
        };
        assert_eq!(
            viewport.rows_for_range(7 * 60 + 30, 9 * 60 + 15),
            Some((0, 3))
        );
        assert_eq!(viewport.rows_for_range(13 * 60, 14 * 60), None);
    }

    #[test]
    fn clips_timed_events_across_midnight_for_each_day() {
        let day = Local::now().date_naive();
        let start = day
            .and_hms_opt(22, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let end = (day + Duration::days(1))
            .and_hms_opt(2, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            item_for_day(1, start, end, false, day)
                .unwrap()
                .start_minute,
            22 * 60
        );
        assert_eq!(
            item_for_day(1, start, end, false, day).unwrap().end_minute,
            MINUTES_PER_DAY
        );
        assert_eq!(
            item_for_day(1, start, end, false, day + Duration::days(1))
                .unwrap()
                .start_minute,
            0
        );
        assert_eq!(
            item_for_day(1, start, end, false, day + Duration::days(1))
                .unwrap()
                .end_minute,
            2 * 60
        );
    }

    #[test]
    fn handles_nested_and_chained_overlaps() {
        let layout = layout_overlaps(&[
            item(0, 9 * 60, 10 * 60),
            item(1, 9 * 60 + 15, 9 * 60 + 45),
            item(2, 9 * 60 + 30, 11 * 60),
            item(3, 10 * 60, 10 * 60 + 30),
        ]);
        assert_eq!(layout.len(), 4);
        assert_eq!(layout.iter().map(|item| item.columns).max(), Some(3));
        assert_eq!(layout[3].columns, 3);
    }
}
