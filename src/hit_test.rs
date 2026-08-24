//! Pure calendar-grid hit testing for future pointer input.
//!
//! Hit testing only identifies a calendar target. It deliberately does not
//! select, move, or mutate an event; those operations must continue through
//! `UserAction` and the existing mutation safeguards.

use crate::layout::TimelineViewport;
use chrono::NaiveDate;

/// A terminal-space rectangle. Right and bottom edges are exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl ScreenRect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && x < self.x.saturating_add(self.width)
            && y >= self.y
            && y < self.y.saturating_add(self.height)
    }
}

/// A rendered calendar column with its local calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarDayColumn {
    pub date: NaiveDate,
    pub rect: ScreenRect,
}

/// A rendered month cell with its local calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarMonthCell {
    pub date: NaiveDate,
    pub rect: ScreenRect,
}

/// An event's rendered bounds. Event bounds take precedence over empty slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEventRegion {
    pub event_id: String,
    pub rect: ScreenRect,
}

/// A provider-neutral calendar target derived from a terminal coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarHitTarget {
    OutsideCalendar,
    EmptyCalendarCell { date: NaiveDate },
    ExistingEvent { event_id: String },
    AllDayRow { date: NaiveDate },
    TimedSlot { date: NaiveDate, minute: u16 },
}

/// The stable geometry emitted by a day or week timeline renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineHitGeometry {
    /// Actual column bounds from the renderer, rather than inferred widths.
    /// This preserves Ratatui's rounding at narrow terminal sizes.
    pub day_columns: Vec<CalendarDayColumn>,
    /// All-day event lanes, excluding the timeline label.
    pub all_day_area: Option<ScreenRect>,
    pub timed_area: ScreenRect,
    pub viewport: TimelineViewport,
    pub event_regions: Vec<CalendarEventRegion>,
}

/// The stable geometry emitted by a month renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthHitGeometry {
    pub cells: Vec<CalendarMonthCell>,
    pub event_regions: Vec<CalendarEventRegion>,
}

/// Calendar-view geometry that converts screen positions to logical targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarHitGeometry {
    Day(TimelineHitGeometry),
    Week(TimelineHitGeometry),
    Month(MonthHitGeometry),
}

impl CalendarHitGeometry {
    /// Returns a target only. Callers must route user intent through
    /// `UserAction`; this function never changes application state.
    pub fn hit_test(&self, x: u16, y: u16) -> CalendarHitTarget {
        match self {
            Self::Day(geometry) | Self::Week(geometry) => geometry.hit_test(x, y),
            Self::Month(geometry) => geometry.hit_test(x, y),
        }
    }
}

impl TimelineHitGeometry {
    pub fn hit_test(&self, x: u16, y: u16) -> CalendarHitTarget {
        if let Some(event) = self
            .event_regions
            .iter()
            .find(|event| event.rect.contains(x, y))
        {
            return CalendarHitTarget::ExistingEvent {
                event_id: event.event_id.clone(),
            };
        }

        let Some(column) = self
            .day_columns
            .iter()
            .find(|column| column.rect.contains(x, y))
        else {
            return CalendarHitTarget::OutsideCalendar;
        };

        if self.all_day_area.is_some_and(|area| area.contains(x, y)) {
            return CalendarHitTarget::AllDayRow { date: column.date };
        }
        if !self.timed_area.contains(x, y) {
            return CalendarHitTarget::OutsideCalendar;
        }

        let row = y.saturating_sub(self.timed_area.y);
        if row >= self.viewport.rows {
            return CalendarHitTarget::OutsideCalendar;
        }
        let minute = self
            .viewport
            .start_minute
            .saturating_add(row.saturating_mul(self.viewport.minutes_per_row));
        if minute >= crate::layout::MINUTES_PER_DAY {
            return CalendarHitTarget::OutsideCalendar;
        }
        CalendarHitTarget::TimedSlot {
            date: column.date,
            minute,
        }
    }
}

impl MonthHitGeometry {
    pub fn hit_test(&self, x: u16, y: u16) -> CalendarHitTarget {
        if let Some(event) = self
            .event_regions
            .iter()
            .find(|event| event.rect.contains(x, y))
        {
            return CalendarHitTarget::ExistingEvent {
                event_id: event.event_id.clone(),
            };
        }
        self.cells
            .iter()
            .find(|cell| cell.rect.contains(x, y))
            .map(|cell| CalendarHitTarget::EmptyCalendarCell { date: cell.date })
            .unwrap_or(CalendarHitTarget::OutsideCalendar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, day).unwrap()
    }

    fn timeline(columns: Vec<CalendarDayColumn>) -> TimelineHitGeometry {
        TimelineHitGeometry {
            day_columns: columns,
            all_day_area: Some(ScreenRect::new(10, 2, 20, 2)),
            timed_area: ScreenRect::new(10, 4, 20, 8),
            viewport: TimelineViewport {
                start_minute: 8 * 60,
                minutes_per_row: 30,
                rows: 8,
            },
            event_regions: Vec::new(),
        }
    }

    #[test]
    fn maps_a_day_timeline_coordinate_to_a_local_time_slot() {
        let geometry = CalendarHitGeometry::Day(timeline(vec![CalendarDayColumn {
            date: date(10),
            rect: ScreenRect::new(10, 4, 20, 8),
        }]));

        assert_eq!(
            geometry.hit_test(12, 6),
            CalendarHitTarget::TimedSlot {
                date: date(10),
                minute: 9 * 60,
            }
        );
    }

    #[test]
    fn maps_week_columns_without_using_elapsed_time() {
        let geometry = CalendarHitGeometry::Week(timeline(vec![
            CalendarDayColumn {
                date: date(10),
                rect: ScreenRect::new(10, 4, 10, 8),
            },
            CalendarDayColumn {
                date: date(11),
                rect: ScreenRect::new(20, 4, 10, 8),
            },
        ]));

        assert_eq!(
            geometry.hit_test(22, 5),
            CalendarHitTarget::TimedSlot {
                date: date(11),
                minute: 8 * 60 + 30,
            }
        );
    }

    #[test]
    fn maps_all_day_rows_to_calendar_dates() {
        let geometry = CalendarHitGeometry::Week(timeline(vec![CalendarDayColumn {
            date: date(10),
            rect: ScreenRect::new(10, 2, 20, 10),
        }]));

        assert_eq!(
            geometry.hit_test(15, 3),
            CalendarHitTarget::AllDayRow { date: date(10) }
        );
    }

    #[test]
    fn maps_month_cells_to_their_calendar_dates() {
        let geometry = CalendarHitGeometry::Month(MonthHitGeometry {
            cells: vec![
                CalendarMonthCell {
                    date: date(1),
                    rect: ScreenRect::new(0, 1, 10, 3),
                },
                CalendarMonthCell {
                    date: date(2),
                    rect: ScreenRect::new(10, 1, 10, 3),
                },
            ],
            event_regions: Vec::new(),
        });

        assert_eq!(
            geometry.hit_test(12, 2),
            CalendarHitTarget::EmptyCalendarCell { date: date(2) }
        );
    }

    #[test]
    fn event_regions_win_over_empty_grid_targets() {
        let geometry = CalendarHitGeometry::Month(MonthHitGeometry {
            cells: vec![CalendarMonthCell {
                date: date(10),
                rect: ScreenRect::new(0, 1, 10, 3),
            }],
            event_regions: vec![CalendarEventRegion {
                event_id: "event-42".into(),
                rect: ScreenRect::new(1, 2, 8, 1),
            }],
        });

        assert_eq!(
            geometry.hit_test(3, 2),
            CalendarHitTarget::ExistingEvent {
                event_id: "event-42".into(),
            }
        );
    }

    #[test]
    fn coordinates_outside_calendar_geometry_are_rejected() {
        let geometry = CalendarHitGeometry::Day(timeline(vec![CalendarDayColumn {
            date: date(10),
            rect: ScreenRect::new(10, 4, 20, 8),
        }]));

        assert_eq!(geometry.hit_test(1, 1), CalendarHitTarget::OutsideCalendar);
        assert_eq!(
            geometry.hit_test(12, 20),
            CalendarHitTarget::OutsideCalendar
        );
    }
}
