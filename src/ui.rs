use crate::{
    app::{
        App, CalendarFormField, DragPreview, EditorMode, Mode, PaletteCommand, View,
        humanize_recurrence,
    },
    hit_test::{
        CalendarDayColumn, CalendarEventRegion, CalendarHitGeometry, CalendarMonthCell,
        MonthHitGeometry, ScreenRect, TimelineHitGeometry,
    },
    layout::{TimelineItem, TimelineViewport, item_for_day},
    model::{AuthorizationStatus, CalendarInfo, Event, EventSpan},
};
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

const BG: Color = Color::Rgb(15, 17, 22);
const PANEL: Color = Color::Rgb(24, 27, 34);
const MUTED: Color = Color::Rgb(128, 137, 154);
const TEXT: Color = Color::Rgb(231, 234, 240);
const ACCENT: Color = Color::Rgb(94, 158, 255);
const RED: Color = Color::Rgb(255, 91, 86);

const BACKGROUND_EVENT_MINUTES: u16 = 6 * 60;
const TIMELINE_MINUTES_PER_ROW: u16 = 30;

#[derive(Debug, Clone, Copy)]
struct TimelineSegment {
    event_index: usize,
    start_minute: u16,
    end_minute: u16,
    rect: Rect,
    z_order: u8,
    is_long: bool,
    continues_from_above: bool,
    continues_below: bool,
}

/// Presentation-only coalescing of consecutive long-event segments.  The
/// source segments remain the authority for hit testing; a run only controls
/// how quietly a long duration is painted.
#[derive(Debug, Clone, Copy)]
struct LongEventVisualRun {
    event_index: usize,
    rect: Rect,
    clipped_top: bool,
    clipped_bottom: bool,
}

/// The sole timed-event geometry authority for paint and hit testing.
///
/// A logical event can yield multiple vertical segments: every segment covers
/// one 30-minute display row with one active event set. This lets long events
/// reclaim width immediately when an overlap ends without overlapping another
/// segment inside the same terminal row.
fn timeline_segments(
    events: &[&Event],
    items: &[TimelineItem],
    cell: Rect,
    viewport: TimelineViewport,
) -> Vec<TimelineSegment> {
    let mut segments = Vec::new();
    for row in 0..viewport.rows {
        let row_start = viewport
            .start_minute
            .saturating_add(row.saturating_mul(viewport.minutes_per_row));
        let row_end = row_start
            .saturating_add(viewport.minutes_per_row)
            .min(viewport.end_minute());
        let mut backgrounds = items
            .iter()
            .copied()
            .filter(|item| item.start_minute < row_end && item.end_minute > row_start)
            .filter(|item| {
                item.end_minute.saturating_sub(item.start_minute) >= BACKGROUND_EVENT_MINUTES
            })
            .collect::<Vec<_>>();
        let mut foreground = items
            .iter()
            .copied()
            .filter(|item| item.start_minute < row_end && item.end_minute > row_start)
            .filter(|item| {
                item.end_minute.saturating_sub(item.start_minute) < BACKGROUND_EVENT_MINUTES
            })
            .collect::<Vec<_>>();
        if backgrounds.is_empty() && foreground.is_empty() {
            continue;
        }
        let stable_order = |left: &TimelineItem, right: &TimelineItem| {
            left.start_minute
                .cmp(&right.start_minute)
                .then_with(|| left.end_minute.cmp(&right.end_minute))
                .then_with(|| {
                    events[left.event_index]
                        .id
                        .cmp(&events[right.event_index].id)
                })
        };
        backgrounds.sort_by(stable_order);
        foreground.sort_by(stable_order);

        // This is deliberately row-local: two 15-minute changes inside one
        // 30-minute terminal row must use the same horizontal partition or
        // their distinct painted segments could overlap each other.
        // A short appointment gets the majority of the cell during a real
        // overlap. Without one, long events share all available width.
        let background_width = if foreground.is_empty() {
            cell.width
        } else if backgrounds.is_empty() {
            0
        } else {
            (cell.width / 3)
                .max(backgrounds.len() as u16)
                .min(cell.width.saturating_sub(1))
        };
        append_partitioned_segments(
            &mut segments,
            &backgrounds,
            cell,
            cell.x,
            background_width,
            row,
            1,
            row_start,
            row_end,
            0,
            row == 0,
            row + 1 == viewport.rows,
        );
        append_partitioned_segments(
            &mut segments,
            &foreground,
            cell,
            cell.x.saturating_add(background_width),
            cell.width.saturating_sub(background_width),
            row,
            1,
            row_start,
            row_end,
            1,
            row == 0,
            row + 1 == viewport.rows,
        );
    }
    // Painter and hit-test both consume this order. Foreground segments are
    // topmost when an intentionally overlapping terminal rectangle exists.
    segments.sort_by(|left, right| {
        left.z_order
            .cmp(&right.z_order)
            .then_with(|| left.start_minute.cmp(&right.start_minute))
            .then_with(|| left.event_index.cmp(&right.event_index))
    });
    segments
}

fn long_event_visual_runs(segments: &[TimelineSegment]) -> Vec<LongEventVisualRun> {
    let mut by_event = std::collections::BTreeMap::<usize, Vec<TimelineSegment>>::new();
    for segment in segments.iter().copied().filter(|segment| segment.is_long) {
        by_event
            .entry(segment.event_index)
            .or_default()
            .push(segment);
    }
    let mut runs = Vec::new();
    for (event_index, mut event_segments) in by_event {
        event_segments.sort_by_key(|segment| (segment.rect.y, segment.rect.x));
        let mut current = None::<LongEventVisualRun>;
        for segment in event_segments {
            if let Some(run) = current.as_mut()
                && run.rect.y.saturating_add(run.rect.height) == segment.rect.y
            {
                run.rect.height = run.rect.height.saturating_add(segment.rect.height);
                run.clipped_bottom = segment.continues_below;
            } else {
                if let Some(run) = current.take() {
                    runs.push(run);
                }
                current = Some(LongEventVisualRun {
                    event_index,
                    rect: segment.rect,
                    clipped_top: segment.continues_from_above,
                    clipped_bottom: segment.continues_below,
                });
            }
        }
        if let Some(run) = current {
            runs.push(run);
        }
    }
    runs.sort_by_key(|run| (run.rect.y, run.rect.x, run.event_index));
    runs
}

#[allow(clippy::too_many_arguments)]
fn append_partitioned_segments(
    output: &mut Vec<TimelineSegment>,
    items: &[TimelineItem],
    cell: Rect,
    x: u16,
    width: u16,
    row: u16,
    height: u16,
    start_minute: u16,
    end_minute: u16,
    z_order: u8,
    first_visible_row: bool,
    last_visible_row: bool,
) {
    if items.is_empty() || width == 0 {
        return;
    }
    let columns = (items.len() as u16).min(width).max(1);
    let column_width = (width / columns).max(1);
    for (column, item) in items.iter().enumerate() {
        let column = column as u16;
        if column >= columns {
            break;
        }
        let left = x.saturating_add(column.saturating_mul(column_width));
        let right = if column + 1 == columns {
            x.saturating_add(width)
        } else {
            left.saturating_add(column_width)
        };
        output.push(TimelineSegment {
            event_index: item.event_index,
            start_minute,
            end_minute,
            rect: Rect::new(
                left,
                cell.y.saturating_add(row),
                right.saturating_sub(left).max(1),
                height.max(1),
            ),
            z_order,
            is_long: z_order == 0,
            continues_from_above: first_visible_row && item.start_minute < start_minute,
            continues_below: last_visible_row && item.end_minute > end_minute,
        });
    }
}

fn timeline_viewport(app: &App, grid_height: u16) -> TimelineViewport {
    // A 60-minute row collapses 09:00–09:30 and 09:45–10:00 into one paint
    // row. Keep half-hour precision on compact terminals; vertical scrolling
    // reveals the rest of the local day instead of overwriting appointments.
    let minutes_per_row = TIMELINE_MINUTES_PER_ROW;
    let start_minute = (app.timeline_start_minute / minutes_per_row) * minutes_per_row;
    TimelineViewport {
        start_minute,
        minutes_per_row,
        rows: grid_height
            .min((crate::layout::MINUTES_PER_DAY.saturating_sub(start_minute)) / minutes_per_row),
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .split(frame.area());
    draw_header(frame, app, rows[0]);
    let content = if app.sidebar_visible && rows[1].width >= 64 {
        let panes =
            Layout::horizontal([Constraint::Length(28), Constraint::Min(24)]).split(rows[1]);
        draw_sidebar(frame, app, panes[0]);
        panes[1]
    } else {
        rows[1]
    };
    if matches!(
        app.mode,
        Mode::CalendarManager
            | Mode::CalendarManagerDetails
            | Mode::CalendarCreate
            | Mode::CalendarRename
            | Mode::CalendarColor
            | Mode::CalendarDeleteConfirm
    ) {
        draw_calendar_manager(frame, app, content);
    } else {
        match app.view {
            View::Day => draw_day(frame, app, content),
            View::Week => draw_week(frame, app, content),
            View::Month => draw_month(frame, app, content),
            View::Agenda => draw_agenda(frame, app, content),
        }
    }
    draw_footer(frame, app, rows[2]);
    match app.mode {
        Mode::Calendars => {}
        Mode::CalendarManager => {}
        Mode::CalendarManagerDetails => draw_calendar_manager_details(frame, app),
        Mode::CalendarCreate => draw_calendar_create_form(frame, app),
        Mode::CalendarRename => draw_calendar_rename_form(frame, app),
        Mode::CalendarColor => draw_calendar_color_form(frame, app),
        Mode::CalendarDeleteConfirm => draw_calendar_delete_confirm(frame, app),
        Mode::QuickAdd => draw_quick_add(frame, app),
        Mode::Details => draw_details(frame, app),
        Mode::Search => draw_search(frame, app),
        Mode::Palette => draw_palette(frame, app),
        Mode::DateJump => draw_date_jump(frame, app),
        Mode::Form => draw_form(frame, app),
        Mode::DiscardConfirm => draw_discard_confirm(frame),
        Mode::Delete => draw_delete(frame, app),
        Mode::RecurringEditScope => draw_recurring_scope(frame, "Edit recurring event"),
        Mode::RecurringDeleteScope => draw_recurring_scope(frame, "Delete recurring event"),
        Mode::Help => draw_help(frame, app),
        Mode::Normal => {}
    }
}

/// Gives the application the actual Day/Week grid height before rendering.
/// This is intentionally the only layout-to-state bridge: it selects which
/// rows are initially visible, while timeline rectangles remain pure geometry.
pub fn sync_timeline_viewport(app: &mut App, frame_area: Rect) {
    if !matches!(app.view, View::Day | View::Week)
        || matches!(
            app.mode,
            Mode::CalendarManager
                | Mode::CalendarManagerDetails
                | Mode::CalendarCreate
                | Mode::CalendarRename
                | Mode::CalendarColor
                | Mode::CalendarDeleteConfirm
        )
    {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .split(frame_area);
    let content = if app.sidebar_visible && rows[1].width >= 64 {
        Layout::horizontal([Constraint::Length(28), Constraint::Min(24)]).split(rows[1])[1]
    } else {
        rows[1]
    };
    let inner = content_block(Line::default()).inner(content);
    if inner.height < 5 {
        return;
    }
    let days = match app.view {
        View::Day => vec![app.active_date],
        View::Week => {
            let (start, _) = app.view_range();
            let first = start.with_timezone(&Local).date_naive();
            (0..7)
                .map(|offset| first + Duration::days(offset))
                .collect()
        }
        _ => return,
    };
    let events = app.visible_events();
    let total_all_day_lanes = days
        .iter()
        .map(|day| {
            events
                .iter()
                .filter(|event| event.all_day && event.occurs_on(*day))
                .count()
        })
        .max()
        .unwrap_or(0);
    let (all_day_lanes, all_day_overflow) = all_day_lane_layout(total_all_day_lanes, inner.height);
    let all_day_rows = if all_day_lanes > 0 {
        all_day_lanes + u16::from(all_day_overflow > 0) + 1
    } else if all_day_overflow > 0 {
        2
    } else {
        0
    };
    let grid_rows = inner.height.saturating_sub(1 + all_day_rows);
    if grid_rows >= 2 {
        app.refresh_auto_timeline_viewport(grid_rows);
    }
}

/// Produces the same calendar-grid geometry used by the current renderer for
/// provider-neutral pointer hit testing. It has no UI or application effects.
pub fn calendar_hit_geometry(app: &App, frame_area: Rect) -> Option<CalendarHitGeometry> {
    if matches!(
        app.mode,
        Mode::CalendarManager
            | Mode::CalendarManagerDetails
            | Mode::CalendarCreate
            | Mode::CalendarRename
            | Mode::CalendarColor
            | Mode::CalendarDeleteConfirm
    ) {
        return None;
    }
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .split(frame_area);
    let content = if app.sidebar_visible && rows[1].width >= 64 {
        Layout::horizontal([Constraint::Length(28), Constraint::Min(24)]).split(rows[1])[1]
    } else {
        rows[1]
    };
    match app.view {
        View::Day => timeline_hit_geometry(app, content, &[app.active_date], true),
        View::Week => {
            let (start, _) = app.view_range();
            let first = start.with_timezone(&Local).date_naive();
            let days: [chrono::NaiveDate; 7] =
                std::array::from_fn(|offset| first + Duration::days(offset as i64));
            timeline_hit_geometry(app, content, &days, false)
        }
        View::Month => month_hit_geometry(app, content),
        View::Agenda => None,
    }
}

fn screen_rect(rect: Rect) -> ScreenRect {
    ScreenRect::new(rect.x, rect.y, rect.width, rect.height)
}

/// Returns the all-day lanes that fit while preserving a usable time grid,
/// plus the number that must be represented by an explicit overflow row.
/// This depends only on terminal geometry, never an arbitrary event limit.
fn all_day_lane_layout(total_lanes: usize, inner_height: u16) -> (u16, usize) {
    if total_lanes == 0 {
        return (0, 0);
    }
    // Header + separator + at least two timeline rows leave this many rows
    // for all-day content. Reserve the final available row for `+N more`.
    let capacity = usize::from(inner_height.saturating_sub(4));
    if total_lanes <= capacity {
        (total_lanes as u16, 0)
    } else {
        let displayed = capacity.saturating_sub(1);
        (displayed as u16, total_lanes.saturating_sub(displayed))
    }
}

fn timeline_hit_geometry(
    app: &App,
    area: Rect,
    days: &[chrono::NaiveDate],
    day_view: bool,
) -> Option<CalendarHitGeometry> {
    let inner = content_block(Line::default()).inner(area);
    if days.is_empty() || inner.width < (days.len() as u16 * 6 + 7) || inner.height < 5 {
        return None;
    }
    let events = app.visible_events();
    let total_all_day_lanes = days
        .iter()
        .map(|day| {
            events
                .iter()
                .filter(|event| event.all_day && event.occurs_on(*day))
                .count()
        })
        .max()
        .unwrap_or(0);
    let (all_day_lanes, all_day_overflow) = all_day_lane_layout(total_all_day_lanes, inner.height);
    let header_rows = 1;
    let all_day_rows = if all_day_lanes > 0 {
        all_day_lanes + u16::from(all_day_overflow > 0) + 1
    } else if all_day_overflow > 0 {
        2
    } else {
        0
    };
    let grid_y = inner.y + header_rows + all_day_rows;
    let grid_height = inner.height.saturating_sub(header_rows + all_day_rows);
    if grid_height < 2 {
        return None;
    }
    let columns = Layout::horizontal(
        [
            vec![Constraint::Length(7)],
            vec![Constraint::Ratio(1, days.len() as u32); days.len()],
        ]
        .concat(),
    )
    .split(inner);
    let day_columns = days
        .iter()
        .enumerate()
        .map(|(index, date)| CalendarDayColumn {
            date: *date,
            rect: screen_rect(Rect::new(
                columns[index + 1].x,
                inner.y + 1,
                columns[index + 1].width,
                inner.height.saturating_sub(1),
            )),
        })
        .collect::<Vec<_>>();
    let first_day = columns[1];
    let last_day = columns[days.len()];
    let all_day_area = (all_day_lanes > 0).then_some(screen_rect(Rect::new(
        first_day.x,
        inner.y + 1,
        last_day
            .x
            .saturating_add(last_day.width)
            .saturating_sub(first_day.x),
        all_day_lanes,
    )));
    let viewport = timeline_viewport(app, grid_height);
    let timed_area = screen_rect(Rect::new(
        first_day.x,
        grid_y,
        last_day
            .x
            .saturating_add(last_day.width)
            .saturating_sub(first_day.x),
        viewport.rows,
    ));
    let mut event_regions = Vec::new();
    for lane in 0..all_day_lanes {
        for (day_index, day) in days.iter().enumerate() {
            if let Some(event) = events
                .iter()
                .filter(|event| event.all_day && event.occurs_on(*day))
                .nth(lane as usize)
            {
                event_regions.push(CalendarEventRegion {
                    event_id: event.id.clone(),
                    rect: screen_rect(Rect::new(
                        columns[day_index + 1].x,
                        inner.y + 1 + lane,
                        columns[day_index + 1].width,
                        1,
                    )),
                });
            }
        }
    }
    for (day_index, day) in days.iter().enumerate() {
        let cell = Rect::new(
            columns[day_index + 1].x,
            grid_y,
            columns[day_index + 1].width,
            grid_height,
        );
        let items = events
            .iter()
            .enumerate()
            .filter(|(_, event)| !event.all_day)
            .filter_map(|(event_index, event)| {
                item_for_day(event_index, event.start, event.end, false, *day)
            })
            .collect::<Vec<_>>();
        for positioned in timeline_segments(&events, &items, cell, viewport) {
            debug_assert!(positioned.end_minute > positioned.start_minute);
            event_regions.push(CalendarEventRegion {
                event_id: events[positioned.event_index].id.clone(),
                rect: screen_rect(positioned.rect),
            });
        }
    }
    let geometry = TimelineHitGeometry {
        day_columns,
        all_day_area,
        timed_area,
        viewport,
        event_regions,
    };
    Some(if day_view {
        CalendarHitGeometry::Day(geometry)
    } else {
        CalendarHitGeometry::Week(geometry)
    })
}

#[derive(Debug, Clone, Copy)]
struct MonthCellLayout {
    date: chrono::NaiveDate,
    rect: Rect,
    content: Rect,
}

#[derive(Debug)]
struct MonthGridLayout {
    inner: Rect,
    week_number_column: Option<Rect>,
    day_columns: Vec<Rect>,
    cells: Vec<MonthCellLayout>,
}

/// Month owns a compact presentation model: a date header and a bounded list
/// of rows. It deliberately shares event IDs, but not timeline geometry.
fn month_grid_layout(app: &App, area: Rect) -> Option<MonthGridLayout> {
    let inner = content_block(Line::default()).inner(area);
    if inner.width < 28 || inner.height < 8 {
        return None;
    }
    let mut constraints = Vec::new();
    if app.config.show_week_numbers {
        constraints.push(Constraint::Length(4));
    }
    constraints.extend([Constraint::Ratio(1, 7); 7]);
    let columns = Layout::horizontal(constraints).split(inner);
    let day_column_start = usize::from(app.config.show_week_numbers);
    let first = app.active_date.with_day(1).unwrap();
    let offset = if app.config.week_start.eq_ignore_ascii_case("sunday") {
        first.weekday().num_days_from_sunday()
    } else {
        first.weekday().num_days_from_monday()
    };
    let grid_start = first - Duration::days(offset.into());
    let row_height = ((inner.height - 1) / 6).max(1);
    let cells = (0..42u16)
        .map(|cell_index| {
            let column = (cell_index % 7) as usize;
            let row = cell_index / 7;
            let rect = Rect::new(
                columns[column + day_column_start].x,
                inner.y + 1 + row * row_height,
                columns[column + day_column_start].width,
                row_height,
            );
            MonthCellLayout {
                date: grid_start + Duration::days(cell_index.into()),
                rect,
                // The Month renderer paints the top/left cell grid edges;
                // all content and hit rows are derived from this same area.
                content: Rect::new(
                    rect.x.saturating_add(1),
                    rect.y.saturating_add(1),
                    rect.width.saturating_sub(1),
                    rect.height.saturating_sub(1),
                ),
            }
        })
        .collect();
    Some(MonthGridLayout {
        inner,
        week_number_column: app.config.show_week_numbers.then_some(columns[0]),
        day_columns: columns[day_column_start..].to_vec(),
        cells,
    })
}

fn month_events_for_date(app: &App, date: chrono::NaiveDate) -> Vec<&Event> {
    let mut events = app
        .snapshot
        .events
        .iter()
        .filter(|event| {
            app.calendar(&event.calendar_id)
                .is_some_and(|calendar| calendar.enabled)
                && event.occurs_on(date)
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| {
        let kind = |event: &&Event| {
            let multi_day = event
                .all_day_date_range()
                .is_some_and(|(start, end)| end > start + Duration::days(1))
                || (!event.all_day
                    && event.start.with_timezone(&Local).date_naive()
                        != event.end.with_timezone(&Local).date_naive());
            match (multi_day, event.all_day) {
                (true, _) => 0,
                (false, true) => 1,
                (false, false) => 2,
            }
        };
        kind(left)
            .cmp(&kind(right))
            .then_with(|| left.start.cmp(&right.start))
            .then_with(|| left.id.cmp(&right.id))
    });
    events
}

/// Returns (visible event rows, hidden count). On overflow the final usable
/// row is reserved for `+N more`; no event is partially rendered into it.
fn month_row_budget(event_count: usize, content_height: u16) -> (usize, usize) {
    let event_capacity = usize::from(content_height.saturating_sub(1));
    if event_count <= event_capacity {
        (event_count, 0)
    } else if event_capacity == 0 {
        (0, event_count)
    } else {
        let shown = event_capacity - 1;
        (shown, event_count - shown)
    }
}

fn month_event_continues_from_previous(event: &Event, date: chrono::NaiveDate) -> bool {
    event
        .all_day_date_range()
        .map(|(start, _)| start < date)
        .unwrap_or_else(|| event.start.with_timezone(&Local).date_naive() < date)
}

fn month_event_label(
    app: &App,
    event: &Event,
    date: chrono::NaiveDate,
    selected: bool,
    width: u16,
) -> String {
    let body = if month_event_continues_from_previous(event, date) {
        // Retain an individual occurrence row but avoid repeating a long
        // title in each cell spanned by a multi-day event.
        "›".to_owned()
    } else if event.all_day {
        format!("• {}", event.title)
    } else {
        format!(
            "{} {}",
            time_text(app, &event.start.with_timezone(&Local)),
            event.title
        )
    };
    truncate_display_width(
        &format!("{}{}", if selected { "▶ " } else { "" }, body),
        usize::from(width),
    )
}

fn month_hit_geometry(app: &App, area: Rect) -> Option<CalendarHitGeometry> {
    let layout = month_grid_layout(app, area)?;
    let mut cells = Vec::new();
    let mut event_regions = Vec::new();
    for cell in &layout.cells {
        cells.push(CalendarMonthCell {
            date: cell.date,
            rect: screen_rect(cell.rect),
        });
        let events = month_events_for_date(app, cell.date);
        let (shown_count, _) = month_row_budget(events.len(), cell.content.height);
        for (line, event) in events.into_iter().take(shown_count).enumerate() {
            event_regions.push(CalendarEventRegion {
                event_id: event.id.clone(),
                rect: screen_rect(Rect::new(
                    cell.content.x,
                    cell.content.y + 1 + line as u16,
                    cell.content.width,
                    1,
                )),
            });
        }
    }
    Some(CalendarHitGeometry::Month(MonthHitGeometry {
        cells,
        event_regions,
    }))
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::horizontal([
        Constraint::Length(24),
        Constraint::Min(20),
        Constraint::Length(24),
    ])
    .split(area);
    let brand = Line::from(vec![
        Span::styled("  ◉ ", Style::default().fg(RED)),
        Span::styled(
            "Terminal Calendar",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(brand)
            .style(Style::default().bg(PANEL))
            .block(Block::default().padding(Padding::top(1))),
        columns[0],
    );

    let tabs = [View::Day, View::Week, View::Month, View::Agenda]
        .into_iter()
        .map(|view| {
            if view == app.view {
                Span::styled(
                    format!(" {} ", view.label()),
                    Style::default()
                        .fg(BG)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {} ", view.label()), Style::default().fg(MUTED))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(tabs))
            .alignment(Alignment::Center)
            .style(Style::default().bg(PANEL))
            .block(Block::default().padding(Padding::top(1))),
        columns[1],
    );

    let sync = if app.syncing {
        "↻ Syncing…"
    } else if let Some(updated) = app.snapshot.updated_at {
        let age = Local::now().signed_duration_since(updated.with_timezone(&Local));
        if age.num_minutes() < 1 {
            "✓ Updated now"
        } else {
            "✓ Cached"
        }
    } else {
        "○ Offline cache"
    };
    frame.render_widget(
        Paragraph::new(sync)
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED).bg(PANEL))
            .block(Block::default().padding(Padding {
                left: 0,
                right: 2,
                top: 1,
                bottom: 0,
            })),
        columns[2],
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let (text, status_style) = footer_text(app, area.width);
    // The footer changes length frequently (especially after an error). Clear
    // its one-row buffer first so a shorter hint cannot leave stale glyphs.
    frame.render_widget(Clear, area);
    let line = Line::from(Span::styled(text, status_style));
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::default().bg(PANEL))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::Rgb(45, 49, 59))),
            ),
        area,
    );
}

fn footer_text(app: &App, width: u16) -> (String, Style) {
    let width = usize::from(width.saturating_sub(2));
    let current_status = app
        .status
        .as_ref()
        .filter(|(_, _, at)| at.elapsed().as_secs() < 8);
    // Errors are intentionally first. A range or reconnect issue is next;
    // only then can a transient success confirmation replace the normal hints.
    if let Some((text, true, _)) = current_status {
        return (
            format!(" {}", truncate_display_width(text, width.saturating_sub(1))),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        );
    }
    if let Some((state, failed)) = footer_backend_state(app) {
        return (
            format!(
                " {}",
                truncate_display_width(state, width.saturating_sub(1))
            ),
            Style::default()
                .fg(if failed { RED } else { ACCENT })
                .add_modifier(Modifier::BOLD),
        );
    }
    if let Some((text, false, _)) = current_status {
        return (
            format!(" {}", truncate_display_width(text, width.saturating_sub(1))),
            Style::default()
                .fg(Color::Rgb(80, 205, 130))
                .add_modifier(Modifier::BOLD),
        );
    }
    let hint = footer_hint(app);
    let prefix = if width >= 120 {
        format!(
            " {} · {} · {} calendars · ",
            app.view.label().to_ascii_uppercase(),
            app.active_date.format("%a %-d %b %Y"),
            app.snapshot
                .calendars
                .iter()
                .filter(|calendar| calendar.enabled)
                .count(),
        )
    } else if width >= 96 {
        format!(
            " {} · {} · ",
            app.view.label().to_ascii_uppercase(),
            app.active_date.format("%-d %b")
        )
    } else {
        String::new()
    };
    let available = width.saturating_sub(display_width(&prefix));
    (
        format!(
            " {prefix}{}",
            truncate_display_width(&hint, available.saturating_sub(1))
        ),
        Style::default().fg(MUTED),
    )
}

fn footer_backend_state(app: &App) -> Option<(&'static str, bool)> {
    use crate::{app::VisibleRangeState, backend::BackendState};
    if matches!(app.visible_range_state, VisibleRangeState::Failed(_)) {
        Some(("RANGE FAILED · R retry", true))
    } else if matches!(app.backend_state, BackendState::ProtocolMismatch) {
        Some(("BACKEND PROTOCOL MISMATCH", true))
    } else if matches!(app.backend_state, BackendState::PermissionDenied) {
        Some(("CALENDAR PERMISSION DENIED", true))
    } else if matches!(app.backend_state, BackendState::Failed) {
        Some(("BACKEND FAILED", true))
    } else if matches!(app.backend_state, BackendState::Disconnected) {
        Some(("BACKEND DISCONNECTED", true))
    } else if matches!(app.backend_state, BackendState::Restarting) {
        Some(("BACKEND RESTARTING", false))
    } else if matches!(app.visible_range_state, VisibleRangeState::Loading) {
        Some(("LOADING RANGE", false))
    } else if app.syncing {
        Some(("SYNCING", false))
    } else {
        None
    }
}

fn footer_hint(app: &App) -> String {
    match app.mode {
        Mode::Normal => match app.view {
            View::Day => {
                "j/k select · h/l date · PgUp/Dn timeline · n new · / search · : commands · ? help"
                    .into()
            }
            View::Week => {
                "h/l day · j/k week · PgUp/Dn timeline · n new · / search · : commands · ? help"
                    .into()
            }
            View::Month => {
                "h/l day · j/k week · Tab event · n new · / search · : commands · ? help".into()
            }
            View::Agenda => "j/k select · h/l date · n new · / search · : commands · ? help".into(),
        },
        Mode::Calendars => "j/k calendar · Space visibility · c / Esc return · ? help".into(),
        Mode::Details => {
            let mut hint = String::from("Esc close · e edit · D duplicate · d delete");
            if app.details_event_ref().is_some_and(|event| {
                !event.url.trim().is_empty() || !event.location.trim().is_empty()
            }) {
                hint.push_str(" · o open link/location");
            }
            hint
        }
        Mode::Form => "Tab / ↑↓ field · ←→ choices · Ctrl-S save · Esc cancel".into(),
        Mode::Search => {
            "type query · ↑↓ result · Enter activate · Shift-Enter details · Esc close".into()
        }
        Mode::Palette => "type filter · ↑↓ command · Enter run · Esc close".into(),
        Mode::DateJump => "type date · Enter apply · Esc cancel".into(),
        Mode::Help => "j/k scroll · PgUp/PgDn page · Home/End · Esc close".into(),
        Mode::CalendarManager => calendar_manager_help(app),
        Mode::CalendarManagerDetails => "Enter / Esc return to Calendar Manager".into(),
        Mode::CalendarCreate => "Tab field · ←→ source · Ctrl-S save · Esc cancel".into(),
        Mode::CalendarRename => "type title · Ctrl-S save · Esc cancel".into(),
        Mode::CalendarColor => "type #RRGGBB · Ctrl-S save · Esc cancel".into(),
        Mode::CalendarDeleteConfirm => "y delete · n / Esc cancel".into(),
        Mode::QuickAdd => "type expression · Ctrl-S save · Ctrl-E details · Esc cancel".into(),
        Mode::DiscardConfirm => "Y save · N discard · Esc continue editing".into(),
        Mode::Delete => "Y / Enter delete · N / Esc cancel".into(),
        Mode::RecurringEditScope | Mode::RecurringDeleteScope => {
            "1 this occurrence · 2 this and future · Esc cancel".into()
        }
    }
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn calendar_manager_help(app: &App) -> String {
    let can_rename = app.calendar_capabilities.can_update
        && app
            .snapshot
            .calendars
            .get(app.selected_calendar)
            .is_some_and(|calendar| calendar.permissions.can_modify_metadata);
    let can_color = app.calendar_capabilities.can_change_color
        && app
            .snapshot
            .calendars
            .get(app.selected_calendar)
            .is_some_and(|calendar| calendar.permissions.can_modify_metadata);
    let can_delete = app.calendar_capabilities.can_delete
        && app
            .snapshot
            .calendars
            .get(app.selected_calendar)
            .is_some_and(|calendar| calendar.permissions.can_delete);
    let mut help = String::from(" j/k select");
    if app.calendar_capabilities.can_create {
        help.push_str("  c create");
    }
    if can_rename {
        help.push_str("  e rename");
    }
    if can_color {
        help.push_str("  C color");
    }
    if can_delete {
        help.push_str("  d delete");
    }
    help.push_str("  Enter details  Esc return ");
    help
}

fn draw_calendar_manager(frame: &mut Frame, app: &App, area: Rect) {
    let panes = if area.width >= 72 {
        Layout::horizontal([Constraint::Percentage(43), Constraint::Percentage(57)]).split(area)
    } else {
        Layout::vertical([Constraint::Percentage(48), Constraint::Percentage(52)]).split(area)
    };
    let list_block = content_block(Line::from(Span::styled(
        " Calendar Manager ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    let list_area = list_block.inner(panes[0]);
    frame.render_widget(list_block, panes[0]);
    let focused = app.mode == Mode::CalendarManager;
    let items = app
        .snapshot
        .calendars
        .iter()
        .enumerate()
        .map(|(index, calendar)| {
            let selected = index == app.selected_calendar;
            let visibility = if calendar.enabled { "☑" } else { "☐" };
            let readonly = if calendar.is_writable { "" } else { "  🔒" };
            ListItem::new(Line::from(vec![
                Span::styled(
                    if selected { "▶ " } else { "  " },
                    Style::default().fg(if focused { ACCENT } else { MUTED }),
                ),
                Span::styled(
                    "● ",
                    Style::default().fg(calendar_color(Some(&calendar.color))),
                ),
                Span::styled(
                    format!("{visibility} {}{readonly}", calendar.title),
                    Style::default().fg(TEXT).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        draw_empty(
            frame,
            list_area,
            "No calendars",
            "Refresh calendar metadata to try again",
        );
    } else {
        frame.render_widget(List::new(items), list_area);
    }

    let details_block = content_block(Line::from(Span::styled(
        " Selected Calendar ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    let details_area = details_block.inner(panes[1]);
    frame.render_widget(details_block, panes[1]);
    draw_calendar_manager_metadata(frame, app, details_area);
}

fn draw_calendar_manager_metadata(frame: &mut Frame, app: &App, area: Rect) {
    let Some(calendar) = app.snapshot.calendars.get(app.selected_calendar) else {
        draw_empty(
            frame,
            area,
            "No calendar selected",
            "Calendar details will appear here",
        );
        return;
    };
    let source = if calendar.account.is_empty() {
        calendar.provider.clone()
    } else if calendar.provider.is_empty() {
        calendar.account.clone()
    } else {
        format!("{} · {}", calendar.account, calendar.provider)
    };
    let permission = |allowed: bool| if allowed { "allowed" } else { "not allowed" };
    let capability = |supported: bool| {
        if supported {
            "supported"
        } else {
            "unsupported"
        }
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "● ",
                Style::default().fg(calendar_color(Some(&calendar.color))),
            ),
            Span::styled(
                calendar.title.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        detail_line("Source", &source, TEXT),
        detail_line("ID", &calendar.id, MUTED),
        Line::from(""),
        Line::from(Span::styled(
            "Permissions",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        detail_line(
            "Create events",
            permission(calendar.permissions.can_create_events),
            permission_color(calendar.permissions.can_create_events),
        ),
        detail_line(
            "Modify events",
            permission(calendar.permissions.can_modify_events),
            permission_color(calendar.permissions.can_modify_events),
        ),
        detail_line(
            "Metadata",
            permission(calendar.permissions.can_modify_metadata),
            permission_color(calendar.permissions.can_modify_metadata),
        ),
        detail_line(
            "Delete",
            permission(calendar.permissions.can_delete),
            permission_color(calendar.permissions.can_delete),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Backend capabilities",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        detail_line(
            "Create",
            capability(app.calendar_capabilities.can_create),
            permission_color(app.calendar_capabilities.can_create),
        ),
        detail_line(
            "Rename",
            capability(app.calendar_capabilities.can_update),
            permission_color(app.calendar_capabilities.can_update),
        ),
        detail_line(
            "Set color",
            capability(app.calendar_capabilities.can_change_color),
            permission_color(app.calendar_capabilities.can_change_color),
        ),
        detail_line(
            "Delete",
            capability(app.calendar_capabilities.can_delete),
            permission_color(app.calendar_capabilities.can_delete),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Read-only information only — no calendar actions are available yet.",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn draw_calendar_manager_details(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 64, 68);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(calendar_detail_lines(
            app.snapshot.calendars.get(app.selected_calendar),
            app,
        ))
        .wrap(Wrap { trim: true })
        .block(popup_block(" Calendar Details ", " Enter or Esc return ")),
        area,
    );
}

fn draw_calendar_create_form(frame: &mut Frame, app: &App) {
    let area = centered_fixed(
        frame.area(),
        66.min(frame.area().width),
        13.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let block = popup_block(
        " Create Calendar ",
        " Tab field · ←/→ source · Ctrl-S save · Esc cancel ",
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(form) = app.calendar_form.as_ref() else {
        return;
    };
    for (index, field) in CalendarFormField::ALL.iter().copied().enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        let columns = Layout::horizontal([Constraint::Length(12), Constraint::Min(12)]).split(row);
        let selected = index == form.selected;
        frame.render_widget(
            Paragraph::new(format!("{}:", field.label())).style(
                Style::default()
                    .fg(if selected { ACCENT } else { MUTED })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            columns[0],
        );
        let value = if selected {
            form.value(&app.calendar_sources)
        } else {
            let mut copy = form.clone();
            copy.selected = index;
            copy.value(&app.calendar_sources)
        };
        frame.render_widget(
            Paragraph::new(value).style(Style::default().fg(TEXT).bg(if selected {
                Color::Rgb(38, 43, 53)
            } else {
                PANEL
            })),
            columns[1],
        );
    }
    let source_rows = app
        .calendar_sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let chosen = form.source_index == Some(index);
            Line::from(Span::styled(
                format!(
                    "{} {} ({}){}",
                    if chosen { "▶" } else { " " },
                    source.title,
                    source.source_type,
                    if source.is_writable {
                        ""
                    } else {
                        "  unavailable"
                    }
                ),
                Style::default().fg(if chosen { ACCENT } else { MUTED }),
            ))
        })
        .collect::<Vec<_>>();
    let sources_area = Rect::new(
        inner.x,
        inner.y + 4,
        inner.width,
        inner.height.saturating_sub(4),
    );
    frame.render_widget(
        Paragraph::new(source_rows).wrap(Wrap { trim: true }),
        sources_area,
    );
}

fn draw_calendar_rename_form(frame: &mut Frame, app: &App) {
    let area = centered_fixed(
        frame.area(),
        64.min(frame.area().width),
        8.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let block = popup_block(" Rename Calendar ", " Ctrl-S save · Esc cancel ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(form) = app.calendar_rename_form.as_ref() else {
        return;
    };
    let row = Rect::new(inner.x, inner.y, inner.width, 1);
    let columns = Layout::horizontal([Constraint::Length(12), Constraint::Min(12)]).split(row);
    frame.render_widget(
        Paragraph::new("Title:").style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(form.title.clone())
            .style(Style::default().fg(TEXT).bg(Color::Rgb(38, 43, 53))),
        columns[1],
    );
    frame.render_widget(
        Paragraph::new("The calendar ID remains unchanged.").style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
}

fn draw_calendar_color_form(frame: &mut Frame, app: &App) {
    let area = centered_fixed(
        frame.area(),
        64.min(frame.area().width),
        8.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let block = popup_block(" Change Calendar Color ", " Ctrl-S save · Esc cancel ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(form) = app.calendar_color_form.as_ref() else {
        return;
    };
    let row = Rect::new(inner.x, inner.y, inner.width, 1);
    let columns = Layout::horizontal([Constraint::Length(12), Constraint::Min(12)]).split(row);
    frame.render_widget(
        Paragraph::new("Color:").style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(form.color.clone())
            .style(Style::default().fg(TEXT).bg(Color::Rgb(38, 43, 53))),
        columns[1],
    );
    let hint = if app.mutation_state == crate::app::MutationState::Saving {
        Span::styled(
            "Changing color…",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "Use the canonical #RRGGBB value.",
            Style::default().fg(MUTED),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
}

fn calendar_detail_lines(calendar: Option<&CalendarInfo>, app: &App) -> Vec<Line<'static>> {
    let Some(calendar) = calendar else {
        return vec![Line::from("Calendar was removed during refresh.")];
    };
    let permission = |allowed: bool| if allowed { "allowed" } else { "not allowed" };
    vec![
        Line::from(Span::styled(
            calendar.title.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        detail_line("Source", &calendar.account, TEXT),
        detail_line("ID", &calendar.id, MUTED),
        detail_line(
            "Event changes",
            permission(calendar.permissions.can_modify_events),
            permission_color(calendar.permissions.can_modify_events),
        ),
        detail_line(
            "Metadata",
            permission(calendar.permissions.can_modify_metadata),
            permission_color(calendar.permissions.can_modify_metadata),
        ),
        detail_line(
            "Delete",
            permission(calendar.permissions.can_delete),
            permission_color(calendar.permissions.can_delete),
        ),
        Line::from(""),
        detail_line(
            "Backend create",
            if app.calendar_capabilities.can_create {
                "supported"
            } else {
                "unsupported"
            },
            permission_color(app.calendar_capabilities.can_create),
        ),
        detail_line(
            "Backend rename",
            if app.calendar_capabilities.can_update {
                "supported"
            } else {
                "unsupported"
            },
            permission_color(app.calendar_capabilities.can_update),
        ),
        detail_line(
            "Backend color",
            if app.calendar_capabilities.can_change_color {
                "supported"
            } else {
                "unsupported"
            },
            permission_color(app.calendar_capabilities.can_change_color),
        ),
        detail_line(
            "Backend delete",
            if app.calendar_capabilities.can_delete {
                "supported"
            } else {
                "unsupported"
            },
            permission_color(app.calendar_capabilities.can_delete),
        ),
    ]
}

fn permission_color(allowed: bool) -> Color {
    if allowed {
        Color::Rgb(80, 205, 130)
    } else {
        MUTED
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.mode == Mode::Calendars;
    let block = Block::default()
        .title(" Calendars ")
        .borders(Borders::RIGHT | Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(if focused {
            ACCENT
        } else {
            Color::Rgb(48, 53, 64)
        }))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    let mut last_account = None::<&str>;
    for (index, calendar) in app.snapshot.calendars.iter().enumerate() {
        if last_account != Some(calendar.account.as_str()) {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                if calendar.account.is_empty() {
                    "Other"
                } else {
                    &calendar.account
                },
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            )));
            last_account = Some(&calendar.account);
        }
        let selected = focused && index == app.selected_calendar;
        let color = calendar_color(Some(&calendar.color));
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "▶ " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(
                calendar.title.clone(),
                Style::default().fg(TEXT).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                if calendar.is_writable { "" } else { "  🔒" },
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if calendar.enabled { " [x]" } else { " [ ]" },
                Style::default().fg(if calendar.enabled {
                    Color::Rgb(80, 205, 130)
                } else {
                    MUTED
                }),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(PANEL)),
        inner,
    );
}

fn content_block(title: Line<'static>) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(48, 53, 64)))
        .style(Style::default().bg(BG))
        .padding(Padding::horizontal(1))
}

fn date_title(app: &App) -> String {
    match app.view {
        View::Day => app.active_date.format("%A, %B %-d, %Y").to_string(),
        View::Week => {
            let (start, end) = app.view_range();
            format!(
                "{} – {}",
                start.with_timezone(&Local).format("%b %-d"),
                (end - Duration::seconds(1))
                    .with_timezone(&Local)
                    .format("%b %-d, %Y")
            )
        }
        View::Month => app.active_date.format("%B %Y").to_string(),
        View::Agenda => format!("Agenda from {}", app.active_date.format("%B %-d, %Y")),
    }
}

fn draw_day(frame: &mut Frame, app: &App, area: Rect) {
    draw_timeline(frame, app, area, &[app.active_date]);
    draw_drag_preview(frame, app, area);
}

fn draw_week(frame: &mut Frame, app: &App, area: Rect) {
    let (start, _) = app.view_range();
    let first = start.with_timezone(&Local).date_naive();
    let days: [chrono::NaiveDate; 7] =
        std::array::from_fn(|offset| first + Duration::days(offset as i64));
    draw_timeline(frame, app, area, &days);
    draw_drag_preview(frame, app, area);
}

fn draw_timeline(frame: &mut Frame, app: &App, area: Rect, days: &[chrono::NaiveDate]) {
    let block = content_block(Line::from(Span::styled(
        format!(" {} ", date_title(app)),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if days.is_empty() || inner.width < (days.len() as u16 * 6 + 7) || inner.height < 5 {
        draw_empty(
            frame,
            inner,
            "Terminal is too small for the time grid",
            "Resize to at least 80×24",
        );
        return;
    }
    let events = app.visible_events();
    let total_all_day_lanes = days
        .iter()
        .map(|day| {
            events
                .iter()
                .filter(|event| event.all_day && event.occurs_on(*day))
                .count()
        })
        .max()
        .unwrap_or(0);
    let (all_day_lanes, all_day_overflow) = all_day_lane_layout(total_all_day_lanes, inner.height);
    let header_rows = 1;
    let all_day_rows = if all_day_lanes > 0 {
        all_day_lanes + u16::from(all_day_overflow > 0) + 1
    } else if all_day_overflow > 0 {
        2
    } else {
        0
    };
    let grid_y = inner.y + header_rows + all_day_rows;
    let grid_height = inner.height.saturating_sub(header_rows + all_day_rows);
    if grid_height < 2 {
        return;
    }
    let viewport = timeline_viewport(app, grid_height);
    let columns = Layout::horizontal(
        [
            vec![Constraint::Length(7)],
            vec![Constraint::Ratio(1, days.len() as u32); days.len()],
        ]
        .concat(),
    )
    .split(inner);
    let compact_week_labels = days.len() == 7;
    for (index, day) in days.iter().enumerate() {
        let is_today = *day == Local::now().date_naive();
        let is_selected = *day == app.active_date;
        frame.render_widget(
            Paragraph::new(format!("{} {}", day.format("%a"), day.day()))
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(if is_today { BG } else { TEXT })
                        .bg(if is_today {
                            ACCENT
                        } else if is_selected {
                            Color::Rgb(38, 43, 53)
                        } else {
                            BG
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            Rect::new(columns[index + 1].x, inner.y, columns[index + 1].width, 1),
        );
        if is_today {
            frame.render_widget(
                Block::default().style(Style::default().bg(Color::Rgb(20, 27, 40))),
                Rect::new(
                    columns[index + 1].x,
                    grid_y,
                    columns[index + 1].width,
                    viewport.rows,
                ),
            );
        }
    }
    if all_day_rows > 0 {
        frame.render_widget(
            Paragraph::new("all-day").style(Style::default().fg(MUTED)),
            Rect::new(columns[0].x, inner.y + 1, columns[0].width, 1),
        );
        for lane in 0..all_day_lanes {
            for (day_index, day) in days.iter().enumerate() {
                if let Some((event_index, event)) = events
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| event.all_day && event.occurs_on(*day))
                    .nth(lane as usize)
                {
                    let selected = event_index == app.selected_event;
                    let color =
                        calendar_color(app.calendar(&event.calendar_id).map(|c| c.color.as_str()));
                    let rect = Rect::new(
                        columns[day_index + 1].x,
                        inner.y + 1 + lane,
                        columns[day_index + 1].width,
                        1,
                    );
                    let label = if selected {
                        format!("▶ {}", event.title)
                    } else {
                        format!(" {}", event.title)
                    };
                    frame.render_widget(
                        Paragraph::new(if compact_week_labels {
                            fit_timeline_label(&label, rect.width)
                        } else {
                            label
                        })
                        .style(event_style(color, selected)),
                        rect,
                    );
                }
            }
        }
        if all_day_overflow > 0 {
            for (day_index, day) in days.iter().enumerate() {
                let hidden = events
                    .iter()
                    .filter(|event| event.all_day && event.occurs_on(*day))
                    .skip(all_day_lanes as usize)
                    .count();
                if hidden > 0 {
                    frame.render_widget(
                        Paragraph::new(format!(" +{hidden} more"))
                            .style(Style::default().fg(MUTED)),
                        Rect::new(
                            columns[day_index + 1].x,
                            inner.y + 1 + all_day_lanes,
                            columns[day_index + 1].width,
                            1,
                        ),
                    );
                }
            }
        }
    }
    for row in 0..viewport.rows {
        let minute = viewport
            .start_minute
            .saturating_add(row.saturating_mul(viewport.minutes_per_row));
        let y = grid_y + row;
        let major = minute % 60 == 0;
        if major {
            frame.render_widget(
                Paragraph::new(hour_text(app, u32::from(minute / 60)))
                    .style(Style::default().fg(MUTED)),
                Rect::new(columns[0].x, y, columns[0].width, 1),
            );
        }
        for column in columns.iter().skip(1) {
            frame.render_widget(
                Paragraph::new(if major { "─" } else { "┄" })
                    .style(Style::default().fg(Color::Rgb(43, 47, 57))),
                Rect::new(column.x, y, column.width, 1),
            );
        }
    }
    let now = Local::now();
    let mut labeled_long_events = std::collections::HashSet::new();
    for (day_index, day) in days.iter().enumerate() {
        let cell = Rect::new(
            columns[day_index + 1].x,
            grid_y,
            columns[day_index + 1].width,
            viewport.rows,
        );
        if *day == now.date_naive() {
            let minute = (now.hour() * 60 + now.minute()) as u16;
            if let Some(row) = viewport.row_for(minute) {
                frame.render_widget(
                    Paragraph::new("━━━━ now")
                        .style(Style::default().fg(RED).add_modifier(Modifier::BOLD)),
                    Rect::new(cell.x, cell.y + row, cell.width, 1),
                );
            }
        }
        let items = events
            .iter()
            .enumerate()
            .filter(|(_, event)| !event.all_day)
            .filter_map(|(event_index, event)| {
                item_for_day(event_index, event.start, event.end, false, *day)
            })
            .collect::<Vec<_>>();
        let segments = timeline_segments(&events, &items, cell, viewport);
        draw_long_event_runs(
            frame,
            app,
            &events,
            &long_event_visual_runs(&segments),
            &mut labeled_long_events,
            days.len() == 1,
            cell,
        );
        let mut labeled_foreground_events = std::collections::HashSet::new();
        for positioned in segments.into_iter().filter(|segment| !segment.is_long) {
            debug_assert!(positioned.end_minute > positioned.start_minute);
            let rect = positioned.rect;
            let event = events[positioned.event_index];
            let selected = positioned.event_index == app.selected_event;
            let color = calendar_color(app.calendar(&event.calendar_id).map(|c| c.color.as_str()));
            let show_label = labeled_foreground_events.insert(positioned.event_index);
            let selection_prefix = if selected { "▶ " } else { "" };
            let label = if !show_label {
                String::new()
            } else if rect.height >= 3 {
                format!(
                    "{selection_prefix}{}\n{}–{}",
                    event.title,
                    time_text(app, &event.start.with_timezone(&Local)),
                    time_text(app, &event.end.with_timezone(&Local))
                )
            } else if rect.height == 2 {
                format!("{selection_prefix}{}", event.title)
            } else {
                // A one-row event still needs an identifying label. The
                // terminal clips this naturally in narrow Day/Week columns;
                // replacing it with a bare bullet made short timed events
                // look as though the overlap layout had dropped them.
                format!("{}{}", if selected { "▶ " } else { "• " }, event.title)
            };
            let label = if compact_week_labels {
                compact_week_timeline_label(&label, rect.width, rect.height, false)
            } else {
                label
            };
            // Paragraph only writes its glyphs. Paint the whole event region
            // first so a shorter later label cannot leave a suffix from an
            // earlier event in the same terminal cells.
            let style = event_style(color, selected);
            frame.render_widget(Block::default().style(style), rect);
            let widget = Paragraph::new(label)
                .wrap(Wrap { trim: true })
                .style(style)
                .block(
                    Block::default()
                        // A two-row terminal block has no usable inner line
                        // once top and bottom borders are applied. Keep its
                        // title visible; three rows can afford the frame.
                        .borders(if rect.height >= 3 && rect.width >= 5 {
                            Borders::ALL
                        } else {
                            Borders::empty()
                        })
                        .border_style(selection_border_style(color, selected)),
                );
            frame.render_widget(widget, rect);
        }
    }
}

fn draw_long_event_runs(
    frame: &mut Frame,
    app: &App,
    events: &[&Event],
    runs: &[LongEventVisualRun],
    labeled_events: &mut std::collections::HashSet<usize>,
    day_view: bool,
    cell: Rect,
) {
    let rail_indices = runs
        .iter()
        .map(|run| run.event_index)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .enumerate()
        .map(|(index, event_index)| (event_index, index as u16))
        .collect::<std::collections::BTreeMap<_, _>>();
    let rail_width = (rail_indices.len() as u16).min(cell.width);
    let label_x = cell.x.saturating_add(rail_width);
    for run in runs {
        let event = events[run.event_index];
        let selected = run.event_index == app.selected_event;
        let style = long_event_style(selected);
        if day_view {
            // Day rails have a stable home next to the time axis instead of
            // inheriting the changing width of a background segment.
            let rail_index = rail_indices[&run.event_index].min(cell.width.saturating_sub(1));
            let rail = Rect::new(
                cell.x.saturating_add(rail_index),
                run.rect.y,
                1,
                run.rect.height,
            );
            frame.render_widget(
                Paragraph::new(long_day_rail_text(run))
                    .style(style)
                    .wrap(Wrap { trim: true }),
                rail,
            );
            if labeled_events.insert(run.event_index) {
                let label = format!(
                    " {}{}  {}–{}",
                    if selected { "▶ " } else { "" },
                    event.title,
                    time_text(app, &event.start.with_timezone(&Local)),
                    time_text(app, &event.end.with_timezone(&Local))
                );
                frame.render_widget(
                    Paragraph::new(label).wrap(Wrap { trim: true }).style(style),
                    Rect::new(
                        label_x,
                        rail.y,
                        cell.x.saturating_add(cell.width).saturating_sub(label_x),
                        1,
                    ),
                );
            }
        } else if labeled_events.insert(run.event_index) {
            let continuation = if run.clipped_top { "↑ " } else { "" };
            let label = format!(
                "{}{}{}  {}–{}",
                if selected { "▶ " } else { "" },
                continuation,
                event.title,
                time_text(app, &event.start.with_timezone(&Local)),
                time_text(app, &event.end.with_timezone(&Local))
            );
            frame.render_widget(
                Paragraph::new(fit_timeline_label(&label, run.rect.width))
                    .wrap(Wrap { trim: true })
                    .style(style),
                // A Week annotation is deliberately one row only. Long
                // event geometry remains available to hit testing through
                // TimelineSegment, but the presentation must not turn it
                // into a vertical rail in every 30-minute row.
                Rect::new(run.rect.x, run.rect.y, run.rect.width, 1),
            );
        }
    }
}

fn long_day_rail_text(run: &LongEventVisualRun) -> String {
    let mut text = String::new();
    for row in 0..run.rect.height {
        let glyph = if row == 0 {
            if run.clipped_top { "↑" } else { "┌" }
        } else if row + 1 == run.rect.height {
            if run.clipped_bottom { "↓" } else { "└" }
        } else {
            "│"
        };
        if row > 0 {
            text.push('\n');
        }
        text.push_str(glyph);
    }
    text
}

fn event_style(color: Color, selected: bool) -> Style {
    let foreground = readable_foreground(color);
    if selected {
        Style::default()
            .fg(foreground)
            .bg(color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(foreground).bg(color)
    }
}

fn long_event_style(selected: bool) -> Style {
    let band = Color::Rgb(30, 34, 43);
    if selected {
        Style::default()
            .fg(TEXT)
            .bg(Color::Rgb(50, 59, 74))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED).bg(band)
    }
}

fn selection_border_style(color: Color, selected: bool) -> Style {
    Style::default()
        .fg(if selected { TEXT } else { color })
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        })
}

/// Week columns can be only a handful of terminal cells wide.  Clip labels
/// before Ratatui paints them so truncation is deterministic, Unicode-width
/// aware, and never depends on a neighbouring day column being empty.
fn fit_timeline_label(label: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    for marker in ["▶ ", "• "] {
        if let Some(title) = label.strip_prefix(marker) {
            let marker_width = marker
                .chars()
                .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
                .sum::<usize>();
            if width <= marker_width {
                return marker.chars().next().unwrap_or(' ').to_string();
            }
            return format!(
                "{marker}{}",
                truncate_display_width(title, width - marker_width)
            );
        }
    }
    truncate_display_width(label, width)
}

fn compact_week_timeline_label(label: &str, width: u16, height: u16, long: bool) -> String {
    if long || height < 3 {
        return fit_timeline_label(label, width);
    }
    let mut lines = label.lines();
    let title = fit_timeline_label(lines.next().unwrap_or_default(), width);
    let detail = lines.next().unwrap_or_default();
    // A time range is less useful than a stable title in a tiny Week cell.
    if width < 7 || detail.is_empty() {
        title
    } else {
        format!(
            "{title}\n{}",
            truncate_display_width(detail, usize::from(width))
        )
    }
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    let width = text
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum::<usize>();
    if width <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".into();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > max_width - 1 {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

fn draw_month(frame: &mut Frame, app: &App, area: Rect) {
    let block = content_block(Line::from(Span::styled(
        format!(" {} ", date_title(app)),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(block, area);
    let Some(layout) = month_grid_layout(app, area) else {
        return;
    };
    let day_names = if app.config.week_start.eq_ignore_ascii_case("sunday") {
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
    } else {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
    };
    if let Some(week_column) = layout.week_number_column {
        frame.render_widget(
            Paragraph::new("Wk")
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
            Rect::new(week_column.x, layout.inner.y, week_column.width, 1),
        );
    }
    for (index, name) in day_names.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(*name)
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
            Rect::new(
                layout.day_columns[index].x,
                layout.inner.y,
                layout.day_columns[index].width,
                1,
            ),
        );
    }
    let selected_id = app
        .selected_event_ref()
        .filter(|event| event.occurs_on(app.active_date))
        .map(|event| event.id.as_str());
    for (cell_index, cell) in layout.cells.iter().enumerate() {
        if let Some(week_column) = layout.week_number_column
            && cell_index % 7 == 0
        {
            frame.render_widget(
                Paragraph::new(format!("{:02}", cell.date.iso_week().week()))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(MUTED)),
                Rect::new(week_column.x, cell.rect.y, week_column.width, 1),
            );
        }
        let in_month = cell.date.month() == app.active_date.month();
        let today = cell.date == Local::now().date_naive();
        let active_date = cell.date == app.active_date;
        let events = month_events_for_date(app, cell.date);
        let (shown_count, hidden_count) = month_row_budget(events.len(), cell.content.height);

        // Paint every cell on every draw, including empty content rows, so a
        // shorter replacement label can never leave a stale glyph behind.
        frame.render_widget(
            Block::default()
                .style(Style::default().bg(BG))
                .borders(Borders::TOP | Borders::LEFT)
                .border_style(Style::default().fg(Color::Rgb(38, 42, 51))),
            cell.rect,
        );

        if cell.content.height == 0 || cell.content.width == 0 {
            frame.render_widget(
                Paragraph::new(format!(
                    "{}{}{}",
                    if active_date { "▶" } else { " " },
                    cell.date.day(),
                    if events.is_empty() {
                        String::new()
                    } else {
                        format!(" +{}", events.len())
                    },
                ))
                .style(Style::default().fg(if in_month { TEXT } else { MUTED })),
                cell.rect,
            );
            continue;
        }
        let date_header = format!(
            "{} {}{}",
            if active_date {
                "▶"
            } else if today {
                "●"
            } else {
                " "
            },
            cell.date.day(),
            if cell.content.height == 1 && !events.is_empty() {
                format!(" +{}", events.len())
            } else {
                String::new()
            },
        );
        frame.render_widget(
            Paragraph::new(truncate_display_width(
                &date_header,
                usize::from(cell.content.width),
            ))
            .style(
                Style::default()
                    .fg(if today {
                        BG
                    } else if in_month {
                        TEXT
                    } else {
                        MUTED
                    })
                    .bg(if today {
                        ACCENT
                    } else if active_date {
                        Color::Rgb(38, 43, 53)
                    } else {
                        BG
                    })
                    .add_modifier(if today || active_date {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Rect::new(cell.content.x, cell.content.y, cell.content.width, 1),
        );
        for (line, event) in events.iter().take(shown_count).enumerate() {
            let selected = selected_id == Some(event.id.as_str());
            let color = calendar_color(app.calendar(&event.calendar_id).map(|c| c.color.as_str()));
            frame.render_widget(
                Paragraph::new(month_event_label(
                    app,
                    event,
                    cell.date,
                    selected,
                    cell.content.width,
                ))
                .style(
                    Style::default()
                        .fg(if selected { BG } else { color })
                        .bg(if selected { color } else { BG })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Rect::new(
                    cell.content.x,
                    cell.content.y + 1 + line as u16,
                    cell.content.width,
                    1,
                ),
            );
        }
        if hidden_count > 0 && cell.content.height >= 2 {
            frame.render_widget(
                Paragraph::new(truncate_display_width(
                    &format!("+{hidden_count} more"),
                    usize::from(cell.content.width),
                ))
                .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
                Rect::new(
                    cell.content.x,
                    cell.content.y + 1 + shown_count as u16,
                    cell.content.width,
                    1,
                ),
            );
        }
    }
    draw_drag_preview(frame, app, area);
}

/// Renders presentation-only drag information above calendar grids. The
/// session owns this data; rendering it never selects, moves, or saves events.
fn draw_drag_preview(frame: &mut Frame, app: &App, content: Rect) {
    let Some(preview) = app.drag_preview() else {
        return;
    };
    if content.width < 32 || content.height < 7 {
        return;
    }
    let width = content.width.min(58);
    let area = Rect::new(
        content
            .x
            .saturating_add(content.width.saturating_sub(width)),
        content.y,
        width,
        7,
    );
    let lines = match preview {
        DragPreview::Timed {
            event_id,
            title,
            original_start,
            original_end,
            proposed_start,
            proposed_end,
            current_target,
        } => {
            let original_start = original_start.with_timezone(&Local);
            let original_end = original_end.with_timezone(&Local);
            let proposed_start = proposed_start.with_timezone(&Local);
            let proposed_end = proposed_end.with_timezone(&Local);
            vec![
                Line::from(Span::styled(
                    format!("{}  ({event_id})", title),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "From  {} {}–{}",
                    original_start.format("%a %-d %b"),
                    time_text(app, &original_start),
                    time_text(app, &original_end),
                )),
                Line::from(format!(
                    "To    {} {}–{}",
                    proposed_start.format("%a %-d %b"),
                    time_text(app, &proposed_start),
                    time_text(app, &proposed_end),
                )),
                Line::from(Span::styled(
                    drag_target_label(&current_target),
                    Style::default().fg(MUTED),
                )),
            ]
        }
        DragPreview::AllDay {
            event_id,
            title,
            original_start_date,
            original_end_date_exclusive,
            proposed_start_date,
            proposed_end_date_exclusive,
            current_target,
        } => vec![
            Line::from(Span::styled(
                format!("{}  ({event_id})", title),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "From  [{original_start_date}, {original_end_date_exclusive})"
            )),
            Line::from(format!(
                "To    [{proposed_start_date}, {proposed_end_date_exclusive})"
            )),
            Line::from(Span::styled(
                drag_target_label(&current_target),
                Style::default().fg(MUTED),
            )),
        ],
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL))
            .block(
                Block::default()
                    .title(" Drag preview ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn drag_target_label(target: &crate::hit_test::CalendarHitTarget) -> String {
    match target {
        crate::hit_test::CalendarHitTarget::TimedSlot { date, minute } => format!(
            "Target  {} {:02}:{:02}",
            date.format("%a %-d %b"),
            minute / 60,
            minute % 60
        ),
        crate::hit_test::CalendarHitTarget::AllDayRow { date } => {
            format!("Target  all-day {date}")
        }
        crate::hit_test::CalendarHitTarget::EmptyCalendarCell { date } => {
            format!("Target  {date}")
        }
        crate::hit_test::CalendarHitTarget::ExistingEvent { event_id } => {
            format!("Target  event {event_id}")
        }
        crate::hit_test::CalendarHitTarget::OutsideCalendar => "Target  outside calendar".into(),
    }
}

fn draw_agenda(frame: &mut Frame, app: &App, area: Rect) {
    let block = content_block(Line::from(Span::styled(
        format!(" {} ", date_title(app)),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    let events = app.visible_events();
    if events.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        draw_empty(
            frame,
            inner,
            "Nothing scheduled",
            "Press n to create an event",
        );
        return;
    }
    let mut previous_date = None;
    let today = Local::now().date_naive();
    let mut items = Vec::new();
    let debug_ui = std::env::var_os("TUI_CALENDAR_DEBUG_UI").is_some();
    let mut grouped_rows = 0usize;
    let mut event_list_indices = Vec::with_capacity(events.len());
    for (event_index, event) in events.iter().enumerate() {
        let date = agenda_display_date(event, app.active_date);
        if previous_date != Some(date) {
            let relative = match date.signed_duration_since(today).num_days() {
                0 => "Today".to_owned(),
                1 => "Tomorrow".to_owned(),
                -1 => "Yesterday".to_owned(),
                _ => date.format("%A, %-d %B").to_string(),
            };
            if debug_ui {
                eprintln!(
                    "agenda row={} type=date_header date={} selected=false",
                    items.len(),
                    date
                );
            }
            items.push(ListItem::new(Line::from(Span::styled(
                format!(" {relative}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))));
            previous_date = Some(date);
            grouped_rows += 1;
        }
        let calendar = app.calendar(&event.calendar_id);
        let row_index = items.len();
        event_list_indices.push(row_index);
        if debug_ui {
            eprintln!(
                "agenda row={} type=event date={} event_index={} event_id={:?} title={:?} selected={}",
                row_index,
                date,
                event_index,
                event.id,
                event.title,
                event_index == app.selected_event,
            );
        }
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                if event.all_day {
                    "  All day  ".to_owned()
                } else {
                    format!(
                        "  {:<9}",
                        time_text(app, &event.start.with_timezone(&Local))
                    )
                },
                Style::default().fg(MUTED),
            ),
            Span::styled(
                "● ",
                Style::default().fg(calendar_color(calendar.map(|c| c.color.as_str()))),
            ),
            Span::styled(
                event.title.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {}",
                    calendar.map(|c| c.title.as_str()).unwrap_or("Unknown")
                ),
                Style::default().fg(MUTED),
            ),
        ])));
    }
    if debug_ui {
        eprintln!(
            "agenda render: input_events={} grouped_rows={} rendered_rows={} viewport_rows={}",
            events.len(),
            grouped_rows,
            items.len(),
            block.inner(area).height,
        );
    }
    let viewport_rows = block.inner(area).height;
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::Rgb(42, 47, 58)))
        .highlight_symbol("▶");
    let selected_list_index = event_list_indices.get(app.selected_event).copied();
    let mut state = ListState::default().with_selected(selected_list_index);
    if debug_ui {
        eprintln!(
            "agenda state: selected_event={} selected_list_index={selected_list_index:?} offset_before={} total_rows={} viewport_rows={}",
            app.selected_event,
            state.offset(),
            event_list_indices.len() + grouped_rows,
            viewport_rows,
        );
    }
    frame.render_stateful_widget(list, area, &mut state);
    if debug_ui {
        eprintln!("agenda state: offset_after={}", state.offset());
    }
}

/// Agenda is a forward-looking view. An event that began before its first
/// visible day but still intersects it belongs under that first day, rather
/// than in an off-screen historical date group.
fn agenda_display_date(event: &Event, first_visible_date: chrono::NaiveDate) -> chrono::NaiveDate {
    event.display_start_date().max(first_visible_date)
}

fn draw_calendar_delete_confirm(frame: &mut Frame, app: &App) {
    let area = centered_fixed(
        frame.area(),
        62.min(frame.area().width),
        10.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let Some(confirmation) = app.calendar_delete_confirmation.as_ref() else {
        return;
    };
    let body = if app.mutation_state == crate::app::MutationState::Deleting {
        vec![
            Line::from(Span::styled(
                "Deleting calendar…",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(confirmation.title.clone()),
        ]
    } else {
        vec![
            Line::from(format!("Delete calendar “{}”?", confirmation.title)),
            Line::from(""),
            Line::from("This action removes the calendar from its EventKit source"),
            Line::from("and may remove the events contained in it."),
            Line::from(""),
            Line::from(Span::styled(
                "[y] Delete   [n] Cancel   [Esc] Cancel",
                Style::default().fg(MUTED),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(body)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(popup_block(" Delete Calendar ", " ")),
        area,
    );
}

fn draw_quick_add(frame: &mut Frame, app: &App) {
    let area = centered(
        frame.area(),
        82.min(frame.area().width),
        42.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let preview = app.quick_add_preview();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "> ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.quick_add_input.clone(), Style::default().fg(TEXT)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]),
        Line::from(""),
    ];
    match (&preview.status, &preview.draft) {
        (crate::quick_add::QuickAddStatus::Ready, Some(draft)) => {
            let calendar = app
                .snapshot
                .calendars
                .iter()
                .find(|calendar| calendar.id == draft.calendar_id)
                .map(|calendar| calendar.title.as_str())
                .unwrap_or("—");
            let (date, time) = match &draft.time {
                crate::model::EventTimeInput::Timed { start, end } => {
                    let start = start.with_timezone(&Local);
                    let end = end.with_timezone(&Local);
                    (
                        start.format("%a %d %b %Y").to_string(),
                        format!("{} – {}", start.format("%H:%M"), end.format("%H:%M")),
                    )
                }
                crate::model::EventTimeInput::AllDay {
                    start_date,
                    end_date_exclusive,
                } => (
                    start_date.format("%a %d %b %Y").to_string(),
                    format!(
                        "all-day through {}",
                        end_date_exclusive
                            .checked_sub_signed(Duration::days(1))
                            .expect("validated all-day range has an inclusive end")
                            .format("%a %d %b %Y")
                    ),
                ),
                crate::model::EventTimeInput::LegacyAllDayUnknown { .. } => {
                    ("legacy all-day".into(), "all-day".into())
                }
            };
            lines.extend([
                Line::from(format!("Title       {}", draft.title)),
                Line::from(format!("Date        {date}")),
                Line::from(format!("Time        {time}")),
                Line::from(format!("Calendar    {calendar}")),
                Line::from(format!(
                    "Location    {}",
                    if draft.location.is_empty() {
                        "—"
                    } else {
                        &draft.location
                    }
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Ctrl-S saves · Ctrl-E opens structured editor",
                    Style::default().fg(MUTED),
                )),
            ]);
        }
        _ => {
            let detail = preview
                .warnings
                .first()
                .map(String::as_str)
                .unwrap_or("Enter a title, date, or time");
            lines.push(Line::from(Span::styled(detail, Style::default().fg(RED))));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(popup_block(" Quick Add ", " Esc cancel ")),
        area,
    );
}

fn draw_details(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 72, 82);
    frame.render_widget(Clear, area);
    let Some(event) = app.details_event_ref() else {
        return;
    };
    let calendar = app.calendar(&event.calendar_id);
    let local_start = event.start.with_timezone(&Local);
    let local_end = event.end.with_timezone(&Local);
    let time = if event.all_day {
        "All day".to_owned()
    } else {
        format!(
            "{}–{}",
            time_text(app, &local_start),
            time_text(app, &local_end)
        )
    };
    let mut lines = vec![
        Line::from(Span::styled(
            event.title.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        detail_line(
            "Calendar",
            calendar.map(|c| c.title.as_str()).unwrap_or("Unknown"),
            calendar_color(calendar.map(|c| c.color.as_str())),
        ),
        detail_line(
            "Date",
            &event
                .display_start_date()
                .format("%A, %-d %B %Y")
                .to_string(),
            TEXT,
        ),
        detail_line("Time", &time, TEXT),
    ];
    if !event.location.is_empty() {
        lines.push(detail_line("Location", &event.location, TEXT));
    }
    if let Some(organizer) = &event.organizer {
        lines.push(detail_line(
            "Organizer",
            &format!("{} {}", organizer.name, organizer.email),
            TEXT,
        ));
    }
    if !event.attendees.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Attendees (read-only via EventKit)",
            Style::default().fg(MUTED),
        )));
        for attendee in &event.attendees {
            let marker = match attendee.status {
                crate::model::InvitationStatus::Accepted => "✓",
                crate::model::InvitationStatus::Declined => "×",
                crate::model::InvitationStatus::Tentative => "?",
                crate::model::InvitationStatus::Pending => "…",
                _ => "•",
            };
            let name = if attendee.name.is_empty() {
                &attendee.email
            } else {
                &attendee.name
            };
            let mut metadata = vec![attendee.status.to_string()];
            if !attendee.role.is_empty() && attendee.role != "unknown" {
                metadata.push(attendee.role.clone());
            }
            if !attendee.participant_type.is_empty() && attendee.participant_type != "unknown" {
                metadata.push(attendee.participant_type.clone());
            }
            if attendee.is_current_user {
                metadata.push("you".into());
            }
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(ACCENT)),
                Span::styled(name.to_owned(), Style::default().fg(TEXT)),
                Span::styled(
                    format!("  {}", metadata.join(" · ")),
                    Style::default().fg(MUTED),
                ),
            ]));
        }
    }
    if !event.alarms.is_empty() {
        lines.push(detail_line(
            "Reminders",
            &event
                .alarms
                .iter()
                .map(format_alarm)
                .collect::<Vec<_>>()
                .join(", "),
            TEXT,
        ));
    }
    if event.has_recurrence {
        lines.push(detail_line(
            "Repeat",
            &humanize_recurrence(&event.recurrence),
            TEXT,
        ));
    }
    lines.push(detail_line(
        "Availability",
        &event.availability.to_string(),
        TEXT,
    ));
    if !event.url.is_empty() {
        lines.push(detail_line("URL", &event.url, ACCENT));
    }
    if !event.notes.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Notes",
            Style::default().fg(MUTED),
        )));
        lines.push(Line::from(event.notes.clone()));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0))
            .block(popup_block(
                " Event Details ",
                " e edit · D duplicate · d delete · o open URL/location · j/k scroll · Esc close ",
            )),
        area,
    );
}

fn draw_search(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 78, 72);
    frame.render_widget(Clear, area);
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "/ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.search_query.clone(), Style::default().fg(TEXT)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]))
        .block(
            Block::default()
                .title(" Global Search ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(PANEL)),
        ),
        rows[0],
    );
    let results = app.search_results();
    let items = results
        .iter()
        .map(|event| {
            let when = if let Some((start, end_exclusive)) = event.all_day_date_range() {
                let inclusive_end = end_exclusive.pred_opt().unwrap_or(start);
                if start == inclusive_end {
                    format!("{} · all-day", start.format("%Y-%m-%d"))
                } else {
                    format!(
                        "{}–{} · all-day",
                        start.format("%Y-%m-%d"),
                        inclusive_end.format("%Y-%m-%d")
                    )
                }
            } else {
                event
                    .start
                    .with_timezone(&Local)
                    .format(if app.config.time_format.eq_ignore_ascii_case("12h") {
                        "%Y-%m-%d %I:%M %p"
                    } else {
                        "%Y-%m-%d %H:%M"
                    })
                    .to_string()
            };
            let recurrence = if event.has_recurrence { "  ↻" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{when}  "), Style::default().fg(MUTED)),
                Span::styled(
                    event.title.clone(),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(recurrence, Style::default().fg(ACCENT)),
                Span::styled(
                    format!(
                        "  {}",
                        app.calendar(&event.calendar_id)
                            .map(|c| c.title.as_str())
                            .unwrap_or("")
                    ),
                    Style::default().fg(calendar_color(
                        app.calendar(&event.calendar_id).map(|c| c.color.as_str()),
                    )),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" {} results ", results.len()))
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .border_style(Style::default().fg(Color::Rgb(48, 53, 64)))
                .style(Style::default().bg(PANEL)),
        )
        .highlight_style(Style::default().bg(Color::Rgb(42, 47, 58)))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(if results.is_empty() {
        None
    } else {
        Some(app.search_selected)
    });
    frame.render_stateful_widget(list, rows[1], &mut state);
}

fn draw_palette(frame: &mut Frame, app: &App) {
    let area = centered_fixed(
        frame.area(),
        frame
            .area()
            .width
            .saturating_mul(3)
            .saturating_div(4)
            .max(36),
        14.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(2)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "> ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.palette_query.clone(), Style::default().fg(TEXT)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]))
        .block(popup_block(" Command Palette ", " Esc close ")),
        rows[0],
    );
    let commands = app.palette_entries();
    let items = commands
        .iter()
        .map(|entry| {
            let label_style = if entry.enabled {
                Style::default().fg(TEXT)
            } else {
                Style::default().fg(MUTED)
            };
            let suffix = if entry.enabled {
                String::new()
            } else {
                format!("  — {}", entry.unavailable_reason())
            };
            ListItem::new(Line::from(vec![
                Span::styled(entry.command.label(), label_style),
                Span::styled(
                    if entry.command.key_hint().is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", entry.command.key_hint())
                    },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(suffix, Style::default().fg(MUTED)),
            ]))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(PANEL)),
        )
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().bg(Color::Rgb(42, 47, 58)));
    let mut state = ListState::default().with_selected(if commands.is_empty() {
        None
    } else {
        Some(app.palette_selected)
    });
    frame.render_stateful_widget(list, rows[1], &mut state);
}

fn draw_date_jump(frame: &mut Frame, app: &App) {
    let area = centered_fixed(
        frame.area(),
        56.min(frame.area().width),
        7.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(vec![
            Span::styled("Go to date: ", Style::default().fg(MUTED)),
            Span::styled(app.palette_query.clone(), Style::default().fg(TEXT)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]),
        Line::from(Span::styled(
            "YYYY-MM-DD · DD.MM.YYYY · today · tomorrow",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(text).block(popup_block(" Go to Date ", " Enter apply · Esc cancel ")),
        area,
    );
}

fn draw_form(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 84, 92);
    frame.render_widget(Clear, area);
    let Some(form) = &app.form else {
        return;
    };
    let title = match &form.editor_mode {
        EditorMode::Create => " Create Event ",
        EditorMode::Edit { .. } => " Edit Event ",
        EditorMode::Duplicate { .. } => " Duplicate Event ",
    };
    let inner_block = popup_block(
        title,
        " Tab field · ←/→ choices · Ctrl-S save · Esc cancel ",
    );
    let inner = inner_block.inner(area);
    frame.render_widget(inner_block, area);
    let row_height = 1u16;
    let visible_fields = form.visible_fields();
    let available = inner.height.min(visible_fields.len() as u16);
    for (index, field) in visible_fields
        .iter()
        .copied()
        .enumerate()
        .take(available as usize)
    {
        let row = Rect::new(
            inner.x,
            inner.y + index as u16 * row_height,
            inner.width,
            row_height,
        );
        let columns = Layout::horizontal([Constraint::Length(16), Constraint::Min(10)]).split(row);
        let selected = index == form.selected;
        frame.render_widget(
            Paragraph::new(format!("{}:", field.label())).style(
                Style::default()
                    .fg(if selected { ACCENT } else { MUTED })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            columns[0],
        );
        let value = if field == form.field() {
            form.value(&app.snapshot.calendars)
        } else {
            let mut copy = form.clone();
            copy.selected = index;
            copy.value(&app.snapshot.calendars)
        };
        frame.render_widget(
            Paragraph::new(value).style(Style::default().fg(TEXT).bg(if selected {
                Color::Rgb(38, 43, 53)
            } else {
                PANEL
            })),
            columns[1],
        );
    }
    if inner.height > available {
        frame.render_widget(
            Paragraph::new(format!("Repeat summary: {}", form.recurrence.summary()))
                .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.y + available, inner.width, 1),
        );
    }
}

fn draw_discard_confirm(frame: &mut Frame) {
    let area = centered_fixed(
        frame.area(),
        46.min(frame.area().width),
        7.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Save changes before closing?"),
            Line::from(Span::styled(
                "Y save · N discard · Esc continue editing",
                Style::default().fg(MUTED),
            )),
        ])
        .block(popup_block(" Unsaved Changes ", " ")),
        area,
    );
}

fn draw_recurring_scope(frame: &mut Frame, title: &str) {
    let area = centered_fixed(frame.area(), 52, 9);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("1  This occurrence"),
            Line::from("2  This and future events"),
            Line::from(""),
            Line::from(Span::styled("Esc  Cancel", Style::default().fg(MUTED))),
        ])
        .alignment(Alignment::Center)
        .block(popup_block(&format!(" {title} "), "")),
        area,
    );
}

fn draw_delete(frame: &mut Frame, app: &App) {
    let area = centered_fixed(frame.area(), 58, 9);
    frame.render_widget(Clear, area);
    let title = app
        .selected_event_ref()
        .map(|event| event.title.as_str())
        .unwrap_or("event");
    let scope = match app.delete_span {
        EventSpan::ThisEvent => "This occurrence",
        EventSpan::FutureEvents => "This and future events",
    };
    let text = Text::from(vec![
        Line::from(format!("Delete “{title}”?")),
        Line::from(""),
        Line::from(format!("Scope: {scope}")),
        Line::from(""),
        Line::from(Span::styled(
            "Y / Enter confirm · N / Esc cancel",
            Style::default().fg(MUTED),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(popup_block(" Confirm Delete ", "")),
        area,
    );
}

fn draw_help(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 72, 82);
    frame.render_widget(Clear, area);
    let lines = help_lines();
    let block = popup_block(
        " Keyboard Shortcuts ",
        " j/k scroll · PgUp/PgDn · ? / Esc close ",
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let max_scroll = lines.len().saturating_sub(usize::from(inner.height));
    let start = usize::from(app.help_scroll).min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(start).collect::<Vec<_>>()),
        inner,
    );
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("Day/Agenda: j/k or ↑/↓ select an event"),
        Line::from("Month: h/l or ←/→ day · j/k or ↑/↓ week · Tab event"),
        Line::from("Week: h/l or ←/→ day · j/k week"),
        Line::from(format!(
            "{}            Today",
            PaletteCommand::Today.key_hint()
        )),
        Line::from(format!(
            "{}       Day / week / month / agenda",
            PaletteCommand::Day.key_hint()
        )),
        Line::from("H/L            Previous / next period"),
        Line::from(""),
        Line::from(Span::styled(
            "Events",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}             New event",
            PaletteCommand::NewEvent.key_hint()
        )),
        Line::from(format!(
            "{}             Quick Add",
            PaletteCommand::QuickAdd.key_hint()
        )),
        Line::from(format!(
            "{}             Edit selected event",
            PaletteCommand::EditEvent.key_hint()
        )),
        Line::from(format!(
            "{}             Duplicate selected event",
            PaletteCommand::DuplicateEvent.key_hint()
        )),
        Line::from(format!(
            "{}             Delete selected event",
            PaletteCommand::DeleteEvent.key_hint()
        )),
        Line::from(format!(
            "{} / {}        Undo / redo confirmed action",
            PaletteCommand::Undo.key_hint(),
            PaletteCommand::Redo.key_hint()
        )),
        Line::from("Enter         Event details"),
        Line::from("Alt-h/l       Move selected event by day"),
        Line::from("Alt-j/k       Move selected timed event by configured step"),
        Line::from(""),
        Line::from(Span::styled(
            "Search, timeline & calendars",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}             Search events",
            PaletteCommand::Search.key_hint()
        )),
        Line::from(format!("{}             Command palette", ":")),
        Line::from("PgUp/PgDn     Day/Week timeline scroll (Manual viewport)"),
        Line::from("c             Calendar sidebar · Space toggles local visibility"),
        Line::from("gc            Calendar Manager (read-only)"),
        Line::from(format!(
            "{}             Refresh · {} retry visible range",
            PaletteCommand::Refresh.key_hint(),
            PaletteCommand::RetryVisibleRange.key_hint()
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Details, editor & mouse",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("Details: e edit · D duplicate · d delete · o open URL/location"),
        Line::from("Editor: Tab / ↑↓ field · ←→ choices · Ctrl-S save · Esc cancel"),
        Line::from("Recurring: 1 this occurrence · 2 this and future · Esc cancel"),
        Line::from("Mouse: click selects; drag moves through the same safe event workflow"),
        Line::from(""),
        Line::from("?             This help · q quits in normal mode · Esc closes overlays"),
    ]
}

fn popup_block(title: &str, bottom: &str) -> Block<'static> {
    let mut block = Block::default()
        .title(title.to_owned())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().fg(TEXT).bg(PANEL))
        .padding(Padding::uniform(1));
    if !bottom.is_empty() {
        block = block.title_bottom(Line::from(bottom.to_owned()).alignment(Alignment::Center));
    }
    block
}

fn draw_empty(frame: &mut Frame, area: Rect, title: &str, subtitle: &str) {
    if area.height < 2 {
        return;
    }
    let y = area.y + area.height / 2 - 1;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                title.to_owned(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                subtitle.to_owned(),
                Style::default().fg(MUTED),
            )),
        ])
        .alignment(Alignment::Center),
        Rect::new(area.x, y, area.width, 2),
    );
}

fn detail_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(color)),
    ])
}

fn format_alarm(alarm: &crate::model::Alarm) -> String {
    if let Some(seconds) = alarm.relative_seconds {
        let seconds = seconds.abs();
        if seconds % 86400 == 0 {
            format!("{} day(s) before", seconds / 86400)
        } else if seconds % 3600 == 0 {
            format!("{} hour(s) before", seconds / 3600)
        } else {
            format!("{} minute(s) before", seconds / 60)
        }
    } else {
        alarm
            .absolute_date
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    }
}

fn calendar_color(hex: Option<&str>) -> Color {
    let Some(hex) = hex.and_then(|value| value.strip_prefix('#')) else {
        return ACCENT;
    };
    if hex.len() != 6 {
        return ACCENT;
    }
    let component = |range| u8::from_str_radix(&hex[range], 16).ok();
    match (component(0..2), component(2..4), component(4..6)) {
        (Some(red), Some(green), Some(blue)) => Color::Rgb(red, green, blue),
        _ => ACCENT,
    }
}

fn readable_foreground(color: Color) -> Color {
    let Color::Rgb(red, green, blue) = color else {
        return BG;
    };
    let luminance = (u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114) / 1000;
    if luminance > 155 { BG } else { TEXT }
}

fn time_text(app: &App, time: &DateTime<Local>) -> String {
    if app.config.time_format.eq_ignore_ascii_case("12h") {
        time.format("%-I:%M %p").to_string()
    } else {
        time.format("%H:%M").to_string()
    }
}

fn hour_text(app: &App, hour: u32) -> String {
    if app.config.time_format.eq_ignore_ascii_case("12h") {
        let suffix = if hour < 12 { "AM" } else { "PM" };
        let display = match hour % 12 {
            0 => 12,
            value => value,
        };
        format!("{display:>2} {suffix}")
    } else {
        format!("{hour:02}:00")
    }
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(20);
    let height = area
        .height
        .saturating_mul(height_percent)
        .saturating_div(100)
        .max(6);
    centered_fixed(area, width.min(area.width), height.min(area.height))
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

#[allow(dead_code)]
fn permission_message(status: AuthorizationStatus) -> Option<&'static str> {
    match status {
        AuthorizationStatus::Denied => Some(
            "Calendar access denied — enable it in System Settings → Privacy & Security → Calendars",
        ),
        AuthorizationStatus::Restricted => Some("Calendar access is restricted by macOS policy"),
        AuthorizationStatus::WriteOnly => Some("Full Calendar access is required"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::CalendarBackend,
        config::Config,
        hit_test::CalendarHitTarget,
        model::{Event, Snapshot},
    };
    use chrono::{NaiveDate, NaiveDateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    /// Timeline geometry intentionally follows the user's local calendar. A
    /// fixture therefore starts from local wall-clock text and derives its UTC
    /// provider instant through the same local zone the renderer uses. This
    /// keeps its 08:00–17:00 schedule identical on UTC CI and Europe/Berlin,
    /// without mutating process-wide TZ in parallel tests or hard-coding DST.
    fn fixture_local_utc(local: &str) -> chrono::DateTime<Utc> {
        NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixture_local_time(day: NaiveDate, time: &str) -> chrono::DateTime<Utc> {
        fixture_local_utc(&format!("{} {time}", day.format("%Y-%m-%d")))
    }

    async fn dense_day_app(count: usize, all_day: bool) -> App {
        let backend = crate::backend::MockBackend::seeded();
        let mut calendars = backend.calendars().await.unwrap();
        for calendar in &mut calendars {
            calendar.enabled = true;
        }
        let template = backend
            .events(
                chrono::Utc::now() - Duration::days(7),
                chrono::Utc::now() + Duration::days(7),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| !event.all_day)
            .unwrap();
        let date = template.start.with_timezone(&Local).date_naive();
        let events = (0..count)
            .map(|index| {
                let mut event: Event = template.clone();
                event.id = format!("dense-{index}");
                event.title = format!("Dense event {index}");
                if all_day {
                    event.all_day = true;
                    event.all_day_start_date = Some(date);
                    event.all_day_end_date_exclusive = Some(date + Duration::days(1));
                }
                event
            })
            .collect();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events,
                authorization: AuthorizationStatus::FullAccess,
                updated_at: None,
            },
        );
        app.active_date = date;
        app
    }

    async fn august_27_mixed_event_app() -> App {
        let mut app = dense_day_app(7, false).await;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        let schedule = [
            (
                "Maxim Granitskiy-Vacation",
                "2026-08-16 00:00",
                "2026-08-31 23:59",
            ),
            (
                "Sven Olde Daalhuis-Homeoffice",
                "2026-08-27 08:00",
                "2026-08-27 17:00",
            ),
            (
                "QNAP Festplatte Dima Disk 1",
                "2026-08-27 09:00",
                "2026-08-27 09:30",
            ),
            ("Dev Stand-up", "2026-08-27 09:45", "2026-08-27 10:00"),
            ("Veronica - Music", "2026-08-27 12:30", "2026-08-27 13:30"),
            (
                "Service Review: Tickets in Jira (Cloudvoid and Bestex)",
                "2026-08-27 15:45",
                "2026-08-27 16:00",
            ),
            (
                "Service Review: Cloudvoid (ConnectWise tickets)",
                "2026-08-27 16:00",
                "2026-08-27 16:15",
            ),
        ];
        for (event, (title, start, end)) in app.snapshot.events.iter_mut().zip(schedule) {
            event.title = title.into();
            event.start = fixture_local_utc(start);
            event.end = fixture_local_utc(end);
        }
        let mut all_day = app.snapshot.events[0].clone();
        all_day.id = "sommerferien".into();
        all_day.title = "Sommerferien".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(day);
        all_day.all_day_end_date_exclusive = Some(day + Duration::days(1));
        app.snapshot.events.push(all_day);
        app.active_date = day;
        app
    }

    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test]
    async fn footer_hints_are_contextual_and_fit_narrow_terminals() {
        let mut app = dense_day_app(1, false).await;
        assert!(footer_hint(&app).contains("PgUp/Dn timeline"));
        app.view = View::Month;
        assert!(!footer_hint(&app).contains("timeline"));
        app.view = View::Agenda;
        assert!(footer_hint(&app).contains("j/k select"));
        app.mode = Mode::Calendars;
        assert!(footer_hint(&app).contains("Space visibility"));
        app.mode = Mode::Details;
        assert!(footer_hint(&app).contains("D duplicate"));
        app.mode = Mode::Form;
        assert!(footer_hint(&app).contains("Ctrl-S save"));
        app.mode = Mode::Search;
        assert!(footer_hint(&app).contains("type query"));

        for width in [200, 160, 120, 100, 80] {
            let (text, _) = footer_text(&app, width);
            assert!(display_width(&text) <= usize::from(width.saturating_sub(1)));
            let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            assert_eq!(terminal.backend().buffer().area.width, width);
        }

        let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
        app.status = Some((
            "A deliberately long transient footer message".into(),
            true,
            std::time::Instant::now(),
        ));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(rendered(&terminal).contains("deliberately long"));
        app.status = None;
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(!rendered(&terminal).contains("deliberately long"));
    }

    #[tokio::test]
    async fn footer_prioritizes_errors_then_backend_failures_then_context_hints() {
        let mut app = dense_day_app(1, false).await;
        app.status = Some((
            "Saved successfully".into(),
            false,
            std::time::Instant::now(),
        ));
        app.visible_range_state =
            crate::app::VisibleRangeState::Failed(crate::app::VisibleRangeError {
                request_id: 1,
                range: app.visible_range_request(),
                error: "offline".into(),
                timestamp: chrono::Utc::now(),
            });
        let (failed, _) = footer_text(&app, 120);
        assert!(failed.contains("RANGE FAILED"));
        app.status = Some(("Permission denied".into(), true, std::time::Instant::now()));
        let (error, _) = footer_text(&app, 120);
        assert!(error.contains("Permission denied"));
        app.status = None;
        app.visible_range_state = crate::app::VisibleRangeState::Ready;
        let (normal, _) = footer_text(&app, 120);
        assert!(normal.contains("commands"));
    }

    #[tokio::test]
    async fn dense_snapshot_reaches_day_geometry_and_agenda_without_a_limit() {
        let mut app = dense_day_app(10, false).await;
        app.view = View::Day;
        assert_eq!(app.snapshot.events.len(), 10);
        assert_eq!(app.visible_events().len(), 10);

        let Some(CalendarHitGeometry::Day(geometry)) =
            calendar_hit_geometry(&app, Rect::new(0, 0, 160, 60))
        else {
            panic!("day geometry must be available");
        };
        assert_eq!(geometry.event_regions.len(), 10);

        app.view = View::Agenda;
        let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        for index in 0..10 {
            assert!(
                contents.contains(&format!("Dense event {index}")),
                "{contents:?}"
            );
        }
    }

    #[tokio::test]
    async fn month_cell_reports_dense_event_overflow_inside_the_cell() {
        let mut app = dense_day_app(10, false).await;
        app.view = View::Month;
        assert_eq!(app.visible_events().len(), 10);

        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        assert!(
            contents.contains(&format!("▶ {} +10", app.active_date.day())),
            "{contents:?}"
        );
    }

    #[tokio::test]
    async fn month_uses_shared_visible_rows_for_overflow_and_hit_identity() {
        let mut app = dense_day_app(10, false).await;
        app.view = View::Month;
        let area = Rect::new(0, 0, 120, 40);
        let Some(CalendarHitGeometry::Month(geometry)) = calendar_hit_geometry(&app, area) else {
            panic!("month geometry must be available");
        };
        let visible = geometry
            .event_regions
            .iter()
            .filter(|region| region.event_id.starts_with("dense-"))
            .collect::<Vec<_>>();
        assert_eq!(visible.len(), 2, "two rows plus one overflow row fit");
        for region in &visible {
            assert!(matches!(
                geometry.hit_test(region.rect.x, region.rect.y),
                CalendarHitTarget::ExistingEvent { ref event_id } if event_id == &region.event_id
            ));
        }

        let hidden_id = "dense-9";
        assert!(
            !visible.iter().any(|region| region.event_id == hidden_id),
            "the overflow marker must not pretend that hidden events have visible rows"
        );
        app.selected_event = app
            .visible_events()
            .iter()
            .position(|event| event.id == hidden_id)
            .unwrap();
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some(hidden_id)
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = rendered(&terminal);
        assert!(rendered.contains("+8 more"), "{rendered:?}");
        let prefix = time_text(&app, &app.snapshot.events[0].start.with_timezone(&Local));
        assert!(
            rendered.contains(&format!("{prefix} Dense ev")),
            "{rendered:?}"
        );
    }

    #[tokio::test]
    async fn month_uses_display_width_truncation_and_clears_replaced_rows() {
        let mut app = dense_day_app(1, false).await;
        app.view = View::Month;
        app.snapshot.events[0].title = "Überprüfung 日本語 calendar title".into();
        let label = month_event_label(&app, &app.snapshot.events[0], app.active_date, false, 12);
        assert!(label.ends_with('…'), "{label:?}");
        assert!(
            label
                .chars()
                .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
                .sum::<usize>()
                <= 12
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(rendered(&terminal).contains('…'));
        app.snapshot.events.clear();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            !rendered(&terminal).contains("Überprüfung"),
            "a redraw with fewer rows must clear old month text"
        );
    }

    #[tokio::test]
    async fn month_marks_multiday_continuations_without_repeating_titles() {
        let mut app = dense_day_app(1, false).await;
        app.view = View::Month;
        let start = app.active_date;
        {
            let event = &mut app.snapshot.events[0];
            event.id = "month-multiday".into();
            event.title = "Conference".into();
            event.all_day = true;
            event.all_day_start_date = Some(start);
            event.all_day_end_date_exclusive = Some(start + Duration::days(3));
        }
        let event = &app.snapshot.events[0];
        assert_eq!(
            month_event_label(&app, event, start, false, 30),
            "• Conference"
        );
        assert_eq!(
            month_event_label(&app, event, start + Duration::days(1), false, 30),
            "›"
        );

        let Some(CalendarHitGeometry::Month(geometry)) =
            calendar_hit_geometry(&app, Rect::new(0, 0, 120, 40))
        else {
            panic!("month geometry must be available");
        };
        assert_eq!(
            geometry
                .event_regions
                .iter()
                .filter(|region| region.event_id == "month-multiday")
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn month_cells_and_event_rows_stay_in_bounds_on_narrow_and_wide_terminals() {
        let mut app = dense_day_app(4, false).await;
        app.view = View::Month;
        for (width, height) in [(80, 24), (180, 52)] {
            let area = Rect::new(0, 0, width, height);
            let Some(CalendarHitGeometry::Month(geometry)) = calendar_hit_geometry(&app, area)
            else {
                panic!("month geometry must fit {width}x{height}");
            };
            for region in &geometry.event_regions {
                let cell = geometry
                    .cells
                    .iter()
                    .find(|cell| cell.rect.contains(region.rect.x, region.rect.y))
                    .unwrap_or_else(|| panic!("{} escaped its month cell", region.event_id));
                assert!(
                    region.rect.x.saturating_add(region.rect.width)
                        <= cell.rect.x.saturating_add(cell.rect.width)
                        && region.rect.y.saturating_add(region.rect.height)
                            <= cell.rect.y.saturating_add(cell.rect.height)
                );
            }
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            assert!(rendered(&terminal).contains(&app.active_date.day().to_string()));
        }
    }

    #[tokio::test]
    async fn month_marks_today_and_a_visible_selected_occurrence_without_coloring_the_cell() {
        let mut app = dense_day_app(2, false).await;
        app.view = View::Month;
        app.selected_event = 0;
        let area = Rect::new(0, 0, 120, 40);
        let Some(CalendarHitGeometry::Month(geometry)) = calendar_hit_geometry(&app, area) else {
            panic!("month geometry must be available");
        };
        let selected = geometry
            .event_regions
            .iter()
            .find(|region| region.event_id == "dense-0")
            .unwrap();
        let selected_color = calendar_color(
            app.calendar(&app.snapshot.events[0].calendar_id)
                .map(|calendar| calendar.color.as_str()),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let selected_cell = &terminal.backend().buffer()[(selected.rect.x, selected.rect.y)];
        assert_eq!(selected_cell.symbol(), "▶");
        assert_eq!(selected_cell.bg, selected_color);

        let mut today_app = App::new(Config::default(), Snapshot::default());
        today_app.view = View::Month;
        today_app.active_date = Local::now().date_naive();
        let Some(CalendarHitGeometry::Month(today_geometry)) =
            calendar_hit_geometry(&today_app, area)
        else {
            panic!("month geometry must be available");
        };
        let today = today_geometry
            .cells
            .iter()
            .find(|cell| cell.date == today_app.active_date)
            .unwrap();
        terminal.draw(|frame| draw(frame, &today_app)).unwrap();
        let today_cell = &terminal.backend().buffer()[(today.rect.x + 1, today.rect.y + 1)];
        assert_eq!(today_cell.symbol(), "▶");
        assert_eq!(today_cell.bg, ACCENT);
    }

    #[tokio::test]
    async fn timeline_reports_all_day_overflow_instead_of_hiding_lanes() {
        let mut app = dense_day_app(8, true).await;
        app.view = View::Day;
        assert_eq!(app.visible_events().len(), 8);

        let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(rendered(&terminal).contains("more"));
    }

    #[tokio::test]
    async fn all_day_events_stay_out_of_timed_overlap_geometry() {
        let mut app = dense_day_app(3, false).await;
        let mut all_day = app.snapshot.events[0].clone();
        all_day.id = "all-day-vacation".into();
        all_day.title = "Vacation".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(app.active_date);
        all_day.all_day_end_date_exclusive = Some(app.active_date + Duration::days(1));
        app.snapshot.events.push(all_day);
        app.view = View::Day;

        let Some(CalendarHitGeometry::Day(geometry)) =
            calendar_hit_geometry(&app, Rect::new(0, 0, 160, 60))
        else {
            panic!("day geometry must be available");
        };
        let all_day_region = geometry
            .event_regions
            .iter()
            .find(|region| region.event_id == "all-day-vacation")
            .expect("all-day event must have a lane region");
        assert!(all_day_region.rect.y < geometry.timed_area.y);

        let timed_regions = geometry
            .event_regions
            .iter()
            .filter(|region| region.event_id.starts_with("dense-"))
            .collect::<Vec<_>>();
        assert_eq!(timed_regions.len(), 3);
        assert!(
            timed_regions
                .iter()
                .all(|region| { region.rect.y >= geometry.timed_area.y && region.rect.width > 0 })
        );
        assert!(
            timed_regions
                .iter()
                .all(|region| !all_day_region.rect.contains(region.rect.x, region.rect.y))
        );
    }

    #[tokio::test]
    async fn day_timeline_assigns_geometry_to_every_event_in_a_dense_schedule() {
        let mut app = dense_day_app(10, false).await;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        // These are explicit 08:00–18:00 local wall-clock times. The fixture
        // derives provider instants through the renderer's local zone, so UTC
        // CI sees the same local schedule. The maximum concurrency is
        // two, so every event must fit in the day timeline without clipping.
        let schedule = [
            ("08:00", "09:00"),
            ("08:30", "10:00"),
            ("09:00", "11:00"),
            ("10:00", "12:00"),
            ("11:00", "13:00"),
            ("12:30", "14:00"),
            ("13:00", "15:00"),
            ("15:00", "16:00"),
            ("16:00", "17:00"),
            ("17:00", "18:00"),
        ];
        for (event, (start, end)) in app.snapshot.events.iter_mut().zip(schedule) {
            event.start = fixture_local_time(day, start);
            event.end = fixture_local_time(day, end);
        }
        app.active_date = day;
        app.view = View::Day;
        app.timeline_start_minute = 7 * 60;
        assert_eq!(app.visible_events().len(), 10);

        let Some(CalendarHitGeometry::Day(geometry)) =
            calendar_hit_geometry(&app, Rect::new(0, 0, 120, 48))
        else {
            panic!("day geometry must be available");
        };
        let timed_regions = geometry
            .event_regions
            .iter()
            .filter(|region| region.event_id.starts_with("dense-"))
            .collect::<Vec<_>>();
        assert_eq!(
            timed_regions
                .iter()
                .map(|region| region.event_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            10
        );
        assert!(timed_regions.iter().all(|region| {
            region.rect.width > 0
                && region.rect.height > 0
                && region.rect.x >= geometry.timed_area.x
                && region.rect.x.saturating_add(region.rect.width)
                    <= geometry
                        .timed_area
                        .x
                        .saturating_add(geometry.timed_area.width)
                && region.rect.y >= geometry.timed_area.y
                && region.rect.y.saturating_add(region.rect.height)
                    <= geometry
                        .timed_area
                        .y
                        .saturating_add(geometry.timed_area.height)
        }));
    }

    #[tokio::test]
    async fn timeline_paints_short_events_after_a_multiday_timed_background() {
        let mut app = dense_day_app(7, false).await;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        let schedule = [
            ("2026-08-16 00:00", "2026-08-31 23:59"),
            ("2026-08-27 08:00", "2026-08-27 09:00"),
            ("2026-08-27 08:30", "2026-08-27 10:00"),
            ("2026-08-27 10:00", "2026-08-27 11:00"),
            ("2026-08-27 12:30", "2026-08-27 14:00"),
            ("2026-08-27 13:00", "2026-08-27 15:00"),
            ("2026-08-27 15:00", "2026-08-27 16:00"),
        ];
        for (event, (start, end)) in app.snapshot.events.iter_mut().zip(schedule) {
            event.start = fixture_local_utc(start);
            event.end = fixture_local_utc(end);
        }
        app.snapshot.events[0].id = "multiday-background".into();
        app.active_date = day;
        app.view = View::Day;

        let Some(CalendarHitGeometry::Day(geometry)) =
            calendar_hit_geometry(&app, Rect::new(0, 0, 120, 60))
        else {
            panic!("day geometry must be available");
        };
        let timed_regions = geometry
            .event_regions
            .iter()
            .filter(|region| {
                region.event_id == "multiday-background" || region.event_id.starts_with("dense-")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            timed_regions
                .iter()
                .map(|region| region.event_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            7
        );
        assert!(
            timed_regions
                .iter()
                .any(|region| region.event_id == "multiday-background")
        );
        assert!(timed_regions.iter().all(|region| {
            region.rect.width > 0
                && region.rect.height > 0
                && region.rect.x >= geometry.timed_area.x
                && region.rect.x.saturating_add(region.rect.width)
                    <= geometry
                        .timed_area
                        .x
                        .saturating_add(geometry.timed_area.width)
                && region.rect.y >= geometry.timed_area.y
                && region.rect.y.saturating_add(region.rect.height)
                    <= geometry
                        .timed_area
                        .y
                        .saturating_add(geometry.timed_area.height)
        }));
    }

    #[tokio::test]
    async fn one_row_timeline_events_keep_an_identifying_label() {
        let mut app = dense_day_app(2, false).await;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        app.snapshot.events[0].start = fixture_local_utc("2026-08-16 00:00");
        app.snapshot.events[0].end = fixture_local_utc("2026-08-31 23:59");
        app.snapshot.events[1].start = fixture_local_utc("2026-08-27 09:45");
        app.snapshot.events[1].end = fixture_local_utc("2026-08-27 10:00");
        app.active_date = day;
        app.view = View::Day;

        let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            rendered(&terminal).contains("• Dense event 1"),
            "{}",
            rendered(&terminal)
        );
    }

    #[tokio::test]
    async fn day_timeline_writes_short_event_labels_into_the_final_buffer() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Day;
        assert_eq!(app.visible_events().len(), 8);

        let mut terminal = Terminal::new(TestBackend::new(160, 60)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        for title in ["QNAP", "Dev Stand-up", "Service Review"] {
            assert!(
                contents.contains(title),
                "short event label {title:?} was erased from the final buffer: {contents:?}"
            );
        }
    }

    #[tokio::test]
    async fn auto_timeline_viewport_uses_the_rendered_grid_height_and_keeps_manual_scroll() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Day;
        sync_timeline_viewport(&mut app, Rect::new(0, 0, 120, 32));
        assert!(
            (7 * 60..=9 * 60).contains(&app.timeline_start_minute),
            "auto start={} should expose the working cluster",
            app.timeline_start_minute
        );

        app.scroll_timeline(6 * 60);
        let manual = app.timeline_start_minute;
        sync_timeline_viewport(&mut app, Rect::new(0, 0, 120, 32));
        assert_eq!(app.timeline_start_minute, manual);
    }

    #[tokio::test]
    async fn selected_timeline_events_keep_calendar_color_and_use_the_hit_test_identity() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Day;
        let area = Rect::new(0, 0, 160, 60);
        let Some(CalendarHitGeometry::Day(geometry)) = calendar_hit_geometry(&app, area) else {
            panic!("day geometry must be available");
        };
        let qnap = app
            .visible_events()
            .iter()
            .find(|event| event.title == "QNAP Festplatte Dima Disk 1")
            .map(|event| (event.id.clone(), event.calendar_id.clone()))
            .unwrap();
        let qnap_region = geometry
            .event_regions
            .iter()
            .find(|region| region.event_id == qnap.0)
            .unwrap();
        let CalendarHitTarget::ExistingEvent { event_id } =
            geometry.hit_test(qnap_region.rect.x, qnap_region.rect.y)
        else {
            panic!("QNAP region must hit QNAP");
        };
        app.selected_event = app
            .visible_events()
            .iter()
            .position(|event| event.id == event_id)
            .unwrap();
        let qnap_color = calendar_color(app.calendar(&qnap.1).map(|c| c.color.as_str()));

        let mut terminal = Terminal::new(TestBackend::new(160, 60)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        assert!(
            contents.contains("▶ QNAP Festplatte Dima Disk 1"),
            "{contents:?}"
        );
        assert!(contents.contains("• Dev Stand-up"), "{contents:?}");
        let cell = &terminal.backend().buffer()[(qnap_region.rect.x, qnap_region.rect.y)];
        assert_eq!(cell.symbol(), "▶");
        assert_eq!(cell.bg, qnap_color, "selection must retain calendar color");

        for (title, marker) in [
            ("Veronica - Music", "▶ Veronica - Music"),
            ("Maxim Granitskiy-Vacation", "▶ Maxim Granitskiy-Vacation"),
            ("Sommerferien", "▶ Sommerferien"),
        ] {
            let event_id = app
                .visible_events()
                .iter()
                .find(|event| event.title == title)
                .map(|event| event.id.clone())
                .unwrap();
            app.selected_event = app
                .visible_events()
                .iter()
                .position(|candidate| candidate.id == event_id)
                .unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            assert!(
                rendered(&terminal).contains(marker),
                "selected {title:?} needs an explicit marker"
            );
        }
    }

    #[test]
    fn week_label_compaction_preserves_selection_and_unicode_display_width() {
        assert_eq!(fit_timeline_label("▶ Service Review", 1), "▶");
        assert_eq!(fit_timeline_label("▶ Service Review", 2), "▶");
        assert_eq!(fit_timeline_label("▶ Service Review", 3), "▶ …");
        assert_eq!(
            fit_timeline_label("▶ Überprüfung der Service Review", 12),
            "▶ Überprüfu…"
        );
        assert_eq!(
            compact_week_timeline_label("▶ Service Review\n15:45–16:00", 6, 3, false),
            "▶ Ser…"
        );
    }

    #[tokio::test]
    async fn week_view_compacts_labels_without_crossing_day_columns_or_losing_hits() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Week;
        let selected_id = app
            .visible_events()
            .iter()
            .find(|event| event.title == "QNAP Festplatte Dima Disk 1")
            .map(|event| event.id.clone())
            .unwrap();
        app.selected_event = app
            .visible_events()
            .iter()
            .position(|event| event.id == selected_id)
            .unwrap();
        let selected_color = app
            .visible_events()
            .iter()
            .find(|event| event.id == selected_id)
            .map(|event| calendar_color(app.calendar(&event.calendar_id).map(|c| c.color.as_str())))
            .unwrap();

        for width in [200, 160, 120, 100, 80] {
            let area = Rect::new(0, 0, width, 40);
            let Some(CalendarHitGeometry::Week(geometry)) = calendar_hit_geometry(&app, area)
            else {
                panic!("week geometry must fit at width {width}");
            };
            for region in &geometry.event_regions {
                let column = geometry
                    .day_columns
                    .iter()
                    .find(|column| {
                        region.rect.x >= column.rect.x
                            && region.rect.x.saturating_add(region.rect.width)
                                <= column.rect.x.saturating_add(column.rect.width)
                    })
                    .unwrap_or_else(|| {
                        panic!("{} escaped a day column at width {width}", region.event_id)
                    });
                assert!(column.rect.contains(region.rect.x, region.rect.y));
            }
            let qnap_region = geometry
                .event_regions
                .iter()
                .find(|region| region.event_id == selected_id)
                .unwrap();
            assert!(matches!(
                geometry.hit_test(qnap_region.rect.x, qnap_region.rect.y),
                CalendarHitTarget::ExistingEvent { ref event_id } if event_id == &selected_id
            ));

            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let contents = rendered(&terminal);
            assert!(
                contents.contains('▶'),
                "selected marker missing at width {width}: {contents:?}"
            );
            let selected_cell =
                &terminal.backend().buffer()[(qnap_region.rect.x, qnap_region.rect.y)];
            assert_eq!(selected_cell.symbol(), "▶");
            assert_eq!(selected_cell.bg, selected_color);
            assert!(
                !contents.contains("│1") && !contents.contains("│2"),
                "Week must not expose internal long-event lane numbers: {contents:?}"
            );
            assert!(
                !contents.contains("..."),
                "Week must not emit long-event continuation artifacts: {contents:?}"
            );
            assert!(
                !contents.contains("Dev Stand-uptte Dima Disk 1"),
                "stale label suffix at width {width}: {contents:?}"
            );
            assert!(
                contents.contains("Maxim"),
                "Week needs a discoverable long-event annotation at width {width}: {contents:?}"
            );

            let maxim_id = app
                .visible_events()
                .iter()
                .find(|event| event.title == "Maxim Granitskiy-Vacation")
                .map(|event| event.id.as_str())
                .unwrap();
            let maxim_rows = geometry
                .event_regions
                .iter()
                .filter(|region| region.event_id == maxim_id)
                .collect::<Vec<_>>();
            let top_row = maxim_rows.iter().map(|region| region.rect.y).min().unwrap();
            for region in maxim_rows
                .into_iter()
                .filter(|region| region.rect.y > top_row)
            {
                let symbol = terminal.backend().buffer()[(region.rect.x, region.rect.y)].symbol();
                assert!(
                    !matches!(symbol, "│" | "┌" | "└" | "↓"),
                    "Week long events must not paint vertical rails: {contents:?}"
                );
            }
        }
    }

    #[test]
    fn day_long_event_rail_is_a_single_continuous_presentation_run() {
        let run = LongEventVisualRun {
            event_index: 0,
            rect: Rect::new(0, 0, 1, 4),
            clipped_top: true,
            clipped_bottom: true,
        };
        assert_eq!(long_day_rail_text(&run), "↑\n│\n│\n↓");

        let bounded = LongEventVisualRun {
            clipped_top: false,
            clipped_bottom: false,
            ..run
        };
        assert_eq!(long_day_rail_text(&bounded), "┌\n│\n│\n└");
    }

    #[tokio::test]
    async fn august_27_timeline_keeps_short_foreground_events_readable_and_hittable() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Day;
        let area = Rect::new(0, 0, 160, 60);
        let Some(CalendarHitGeometry::Day(geometry)) = calendar_hit_geometry(&app, area) else {
            panic!("day geometry must be available");
        };

        let timed = geometry
            .event_regions
            .iter()
            .filter(|region| region.event_id != "sommerferien")
            .collect::<Vec<_>>();
        assert_eq!(
            timed
                .iter()
                .map(|region| region.event_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            7
        );
        assert!(timed.iter().all(|region| {
            region.rect.width > 0
                && region.rect.height > 0
                && region.rect.x >= geometry.timed_area.x
                && region.rect.x.saturating_add(region.rect.width)
                    <= geometry
                        .timed_area
                        .x
                        .saturating_add(geometry.timed_area.width)
                && region.rect.y >= geometry.timed_area.y
                && region.rect.y.saturating_add(region.rect.height)
                    <= geometry
                        .timed_area
                        .y
                        .saturating_add(geometry.timed_area.height)
        }));

        let region = |title: &str| {
            let event = app
                .snapshot
                .events
                .iter()
                .find(|event| event.title == title)
                .unwrap();
            timed
                .iter()
                .find(|region| region.event_id == event.id)
                .unwrap()
        };
        let qnap = region("QNAP Festplatte Dima Disk 1");
        let standup = region("Dev Stand-up");
        let jira = region("Service Review: Tickets in Jira (Cloudvoid and Bestex)");
        let cloudvoid = region("Service Review: Cloudvoid (ConnectWise tickets)");
        assert_ne!(qnap.rect, standup.rect);
        assert_ne!(jira.rect, cloudvoid.rect);

        let events = app.visible_events();
        let items = events
            .iter()
            .enumerate()
            .filter(|(_, event)| !event.all_day)
            .filter_map(|(event_index, event)| {
                item_for_day(event_index, event.start, event.end, false, app.active_date)
            })
            .collect::<Vec<_>>();
        let cell = Rect::new(
            geometry.timed_area.x,
            geometry.timed_area.y,
            geometry.timed_area.width,
            geometry.timed_area.height,
        );
        let segments = timeline_segments(&events, &items, cell, geometry.viewport);
        let maxim_index = events
            .iter()
            .position(|event| event.title == "Maxim Granitskiy-Vacation")
            .unwrap();
        let sven_index = events
            .iter()
            .position(|event| event.title == "Sven Olde Daalhuis-Homeoffice")
            .unwrap();
        let qnap_index = events
            .iter()
            .position(|event| event.title == "QNAP Festplatte Dima Disk 1")
            .unwrap();
        let maxim_during_three_way = segments
            .iter()
            .find(|segment| {
                segment.event_index == maxim_index
                    && segment.start_minute == 9 * 60
                    && segment.end_minute == 9 * 60 + 30
            })
            .unwrap();
        let maxim_after_sven = segments
            .iter()
            .find(|segment| segment.event_index == maxim_index && segment.start_minute == 17 * 60)
            .unwrap();
        let qnap_segment = segments
            .iter()
            .find(|segment| segment.event_index == qnap_index)
            .unwrap();
        assert!(maxim_after_sven.rect.width > maxim_during_three_way.rect.width);
        assert!(qnap_segment.rect.width > cell.width / 2);
        assert!(
            segments
                .iter()
                .filter(|segment| segment.event_index == sven_index)
                .all(|segment| segment.start_minute >= 8 * 60 && segment.end_minute <= 17 * 60)
        );
        for segment in &segments {
            assert!(matches!(
                geometry.hit_test(segment.rect.x, segment.rect.y),
                CalendarHitTarget::ExistingEvent { ref event_id }
                    if event_id == &events[segment.event_index].id
            ));
        }

        app.view = View::Week;
        let Some(CalendarHitGeometry::Week(week_geometry)) = calendar_hit_geometry(&app, area)
        else {
            panic!("week geometry must be available");
        };
        assert_eq!(
            week_geometry
                .event_regions
                .iter()
                .filter(|region| region.event_id != "sommerferien")
                .map(|region| region.event_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            7
        );
        app.view = View::Day;

        // The segment list is the paint order consumed by hit testing.
        assert!(matches!(
            geometry.hit_test(qnap.rect.x, qnap.rect.y),
            CalendarHitTarget::ExistingEvent { ref event_id } if event_id == &qnap.event_id
        ));

        let mut terminal = Terminal::new(TestBackend::new(160, 60)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        let maxim_top = segments
            .iter()
            .filter(|segment| segment.event_index == maxim_index)
            .map(|segment| segment.rect.y)
            .min()
            .unwrap();
        let sven_top = segments
            .iter()
            .filter(|segment| segment.event_index == sven_index)
            .map(|segment| segment.rect.y)
            .min()
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Day presentation has exactly one dedicated rail strip immediately
        // after the time axis. Its rails are continuous runs, while normal
        // appointments retain their original main-content geometry.
        assert_eq!(buffer[(geometry.timed_area.x, maxim_top)].symbol(), "↑");
        assert_eq!(buffer[(geometry.timed_area.x, maxim_top + 1)].symbol(), "│");
        assert_eq!(buffer[(geometry.timed_area.x + 1, sven_top)].symbol(), "┌");
        assert!(qnap_segment.rect.x >= geometry.timed_area.x + 2);
        assert_eq!(
            app.snapshot.events[0].calendar_id, app.snapshot.events[1].calendar_id,
            "the two long events intentionally share a calendar color fixture"
        );
        for title in [
            "Maxim Granitskiy-Vacation",
            "Sven Olde Daalhuis-Homeoffice",
            "Veronica - Music",
            "QNAP Festplatte Dima Disk 1",
            "Dev Stand-up",
            "Service Review: Tickets in Jira (Cloudvoid and Bestex)",
            "Service Review: Cloudvoid (ConnectWise tickets)",
        ] {
            assert!(
                contents.contains(title),
                "{title:?} must remain visible in the final timeline buffer: {contents:?}"
            );
        }
        assert!(
            contents.contains("Maxim Granitskiy-Vacation"),
            "long-event title must remain discoverable: {contents:?}"
        );
        assert!(
            contents.contains('↑'),
            "a clipped Day rail needs a continuous-run start marker: {contents:?}"
        );
        assert!(
            contents.contains("┌Sven Olde Daalhuis-Homeoffice  08:00–17:00"),
            "long events need one sparse duration label: {contents:?}"
        );
        assert_eq!(contents.matches("Maxim Granitskiy-Vacation").count(), 1);
        assert_eq!(contents.matches("Sven Olde Daalhuis-Homeoffice").count(), 1);
        assert!(!contents.contains("│1") && !contents.contains("│2"));
        assert!(
            !contents.contains("Dev Stand-uptte Dima Disk 1"),
            "a short label must not retain the suffix of an earlier paint: {contents:?}"
        );
        for hour in 24..=35 {
            assert!(
                !contents.contains(&format!("{hour:02}:00")),
                "timeline must stop at the local day boundary: {contents:?}"
            );
        }
    }

    #[tokio::test]
    async fn compact_timeline_keeps_qnap_and_dev_on_separate_painted_rows() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Day;
        // This height used to choose 60-minute rows, quantizing both short
        // appointments into one terminal row. A compact terminal now scrolls
        // through 30-minute rows instead of merging their labels.
        let area = Rect::new(0, 0, 160, 35);
        let Some(CalendarHitGeometry::Day(geometry)) = calendar_hit_geometry(&app, area) else {
            panic!("day geometry must be available");
        };
        assert_eq!(geometry.viewport.minutes_per_row, 30);
        assert_eq!(geometry.viewport.start_minute, 7 * 60);

        let find_region = |title: &str| {
            let event = app
                .snapshot
                .events
                .iter()
                .find(|event| event.title == title)
                .unwrap();
            geometry
                .event_regions
                .iter()
                .find(|region| region.event_id == event.id)
                .unwrap()
        };
        let qnap = find_region("QNAP Festplatte Dima Disk 1");
        let standup = find_region("Dev Stand-up");
        assert_eq!(qnap.rect.height, 1);
        assert_eq!(standup.rect.height, 1);
        assert_ne!(qnap.rect.y, standup.rect.y);

        let mut terminal = Terminal::new(TestBackend::new(160, 35)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        assert!(contents.contains("QNAP Festplatte Dima Disk 1"));
        assert!(contents.contains("Dev Stand-up"));
        assert!(
            contents.contains("↓"),
            "a long event continuing below a compact viewport needs a cue: {contents:?}"
        );
        assert!(!contents.contains("Dev Stand-uptte Dima Disk 1"));
    }

    #[tokio::test]
    async fn agenda_keeps_every_visible_event_and_places_spans_on_its_first_day() {
        let mut app = august_27_mixed_event_app().await;
        app.view = View::Agenda;

        let visible = app.visible_events();
        assert_eq!(visible.len(), 8);
        let titles = visible
            .iter()
            .map(|event| event.title.clone())
            .collect::<Vec<_>>();

        let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let contents = rendered(&terminal);
        for title in &titles {
            assert!(
                contents.contains(title),
                "missing {title:?} in {contents:?}"
            );
        }
        assert!(contents.contains("Thursday, 27 August"), "{contents:?}");
        assert!(!contents.contains("Monday, 17 August"), "{contents:?}");

        app.selected_event = titles.len() - 1;
        let selected_title = titles.last().unwrap();
        let mut short_terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
        short_terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            rendered(&short_terminal).contains(selected_title),
            "the existing stateful agenda list must scroll to the selected event"
        );
    }

    #[tokio::test]
    async fn month_and_agenda_titles_follow_active_date_not_a_long_event_start() {
        let mut app = august_27_mixed_event_app().await;
        let long_event = app
            .snapshot
            .events
            .iter_mut()
            .find(|event| event.title == "Maxim Granitskiy-Vacation")
            .unwrap();
        long_event.start = fixture_local_utc("2026-07-20 00:00");
        long_event.end = fixture_local_utc("2026-09-02 23:59");
        app.active_date = NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();

        app.view = View::Agenda;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let agenda = rendered(&terminal);
        assert!(agenda.contains("Agenda from August 27, 2026"), "{agenda:?}");
        assert!(!agenda.contains("Agenda from July 20, 2026"), "{agenda:?}");

        app.view = View::Month;
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let month = rendered(&terminal);
        assert!(month.contains("August 2026"), "{month:?}");
        assert!(!month.contains("July 2026"), "{month:?}");
    }

    #[tokio::test]
    async fn calendar_manager_renders_mixed_permission_metadata() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: None,
            },
        );
        app.mode = Mode::CalendarManager;
        app.selected_calendar = app
            .snapshot
            .calendars
            .iter()
            .position(|calendar| calendar.id == "shared")
            .unwrap();
        app.calendar_capabilities.can_create = true;
        app.calendar_capabilities.can_update = true;
        app.calendar_capabilities.can_change_color = true;
        app.calendar_capabilities.can_delete = true;

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Shared Calendar"));
        assert!(rendered.contains("not allowed"));
        assert!(rendered.contains("Backend capabilities"));
    }

    #[tokio::test]
    async fn drag_preview_renders_and_cancellation_removes_it_without_mutation() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let events = backend
            .events(
                chrono::Utc::now() - Duration::days(7),
                chrono::Utc::now() + Duration::days(7),
                &[],
            )
            .await
            .unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events,
                authorization: AuthorizationStatus::FullAccess,
                updated_at: None,
            },
        );
        app.view = View::Day;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let original_events = app.snapshot.events.clone();
        assert!(app.start_drag_session(
            event.id.clone(),
            CalendarHitTarget::ExistingEvent {
                event_id: event.id.clone(),
            },
        ));
        assert!(app.update_drag_preview(CalendarHitTarget::TimedSlot {
            date: event.start.with_timezone(&Local).date_naive(),
            minute: 15 * 60,
        }));

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Drag preview"));
        assert!(rendered.contains("Architecture review"));
        assert!(rendered.contains("From"));
        assert!(rendered.contains("Target"));
        assert_eq!(app.snapshot.events, original_events);

        app.cancel_drag_session();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Drag preview"));
        assert_eq!(app.snapshot.events, original_events);
    }

    #[tokio::test]
    async fn all_day_drag_preview_renders_floating_date_ranges() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let start = Local::now().date_naive();
        let mut event = backend
            .events(
                chrono::Utc::now() - Duration::days(7),
                chrono::Utc::now() + Duration::days(7),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        event.id = "preview-all-day".into();
        event.calendar_id = "personal".into();
        event.all_day = true;
        event.all_day_start_date = Some(start);
        event.all_day_end_date_exclusive = Some(start + Duration::days(2));
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![event.clone()],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: None,
            },
        );
        app.view = View::Month;
        app.active_date = start;
        assert!(app.start_drag_session(
            event.id.clone(),
            CalendarHitTarget::ExistingEvent {
                event_id: event.id.clone(),
            },
        ));
        assert!(
            app.update_drag_preview(CalendarHitTarget::EmptyCalendarCell {
                date: start + Duration::days(4),
            })
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Drag preview"));
        assert!(rendered.contains(&format!("From  [{start}, {})", start + Duration::days(2))));
        assert!(rendered.contains(&format!(
            "To    [{}, {})",
            start + Duration::days(4),
            start + Duration::days(6)
        )));
        assert!(!rendered.contains("UTC"));
    }

    #[test]
    fn week_navigation_rerenders_the_visible_seven_day_window() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.view = View::Week;
        app.active_date = chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let before = date_title(&app);
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains(&before)
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        let after = date_title(&app);
        assert_ne!(before, after);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(&after), "{rendered:?}");
    }

    #[test]
    fn month_renderer_marks_the_active_date_without_relying_on_color() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.view = View::Month;
        app.active_date = chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("▶ 10"), "{rendered:?}");
    }

    #[tokio::test]
    async fn timeline_caps_unrenderable_overlap_columns_to_the_terminal_width() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let base = backend
            .events(
                chrono::Utc::now() - Duration::days(7),
                chrono::Utc::now() + Duration::days(7),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| !event.all_day)
            .unwrap();
        // More concurrent events than a week-cell can render side-by-side
        // must be safely clipped rather than producing an out-of-frame rect.
        let events = (0..200)
            .map(|index| {
                let mut event = base.clone();
                event.id = format!("overlap-{index}");
                event.title = format!("Overlap {index}");
                event
            })
            .collect();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events,
                authorization: AuthorizationStatus::FullAccess,
                updated_at: None,
            },
        );
        app.view = View::Week;
        app.active_date = base.start.with_timezone(&Local).date_naive();

        let mut terminal = Terminal::new(TestBackend::new(160, 48)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(calendar_hit_geometry(&app, Rect::new(0, 0, 160, 48)).is_some());
    }
}
