use crate::{
    app::{App, CalendarFormField, DragPreview, EditorMode, Mode, View, humanize_recurrence},
    hit_test::{
        CalendarDayColumn, CalendarEventRegion, CalendarHitGeometry, CalendarMonthCell,
        MonthHitGeometry, ScreenRect, TimelineHitGeometry,
    },
    layout::{TimelineViewport, item_for_day, layout_overlaps},
    model::{AuthorizationStatus, CalendarInfo, EventSpan},
};
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

const BG: Color = Color::Rgb(15, 17, 22);
const PANEL: Color = Color::Rgb(24, 27, 34);
const MUTED: Color = Color::Rgb(128, 137, 154);
const TEXT: Color = Color::Rgb(231, 234, 240);
const ACCENT: Color = Color::Rgb(94, 158, 255);
const RED: Color = Color::Rgb(255, 91, 86);

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
        Mode::Help => draw_help(frame),
        Mode::Normal => {}
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
    let all_day_lanes = days
        .iter()
        .map(|day| {
            events
                .iter()
                .filter(|event| event.all_day && event.occurs_on(*day))
                .count()
        })
        .max()
        .unwrap_or(0)
        .min(3) as u16;
    let header_rows = 1;
    let all_day_rows = if all_day_lanes > 0 {
        all_day_lanes + 1
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
    let timed_area = screen_rect(Rect::new(
        first_day.x,
        grid_y,
        last_day
            .x
            .saturating_add(last_day.width)
            .saturating_sub(first_day.x),
        grid_height,
    ));
    let minutes_per_row = if grid_height >= 30 { 30 } else { 60 };
    let viewport = TimelineViewport {
        start_minute: (app.timeline_start_minute / minutes_per_row) * minutes_per_row,
        minutes_per_row,
        rows: grid_height,
    };
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
            .filter_map(|(event_index, event)| {
                item_for_day(event_index, event.start, event.end, event.all_day, *day)
            })
            .collect::<Vec<_>>();
        for positioned in layout_overlaps(&items) {
            let Some((row, height)) =
                viewport.rows_for_range(positioned.start_minute, positioned.end_minute)
            else {
                continue;
            };
            // The overlap layout can legitimately have more columns than a
            // narrow terminal cell has character columns. Do not construct a
            // rectangle outside the frame for visually unrepresentable lanes.
            let columns = positioned.columns.min(cell.width.max(1));
            if positioned.column >= columns {
                continue;
            }
            let width = (cell.width / columns).max(1);
            let x = cell
                .x
                .saturating_add(positioned.column.saturating_mul(width));
            let right = if positioned.column + 1 == columns {
                cell.x.saturating_add(cell.width)
            } else {
                x.saturating_add(width)
            };
            event_regions.push(CalendarEventRegion {
                event_id: events[positioned.event_index].id.clone(),
                rect: screen_rect(Rect::new(
                    x,
                    cell.y + row,
                    right.saturating_sub(x).max(1),
                    height.max(1),
                )),
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

fn month_hit_geometry(app: &App, area: Rect) -> Option<CalendarHitGeometry> {
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
    let enabled = app
        .snapshot
        .calendars
        .iter()
        .filter(|calendar| calendar.enabled)
        .map(|calendar| calendar.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut cells = Vec::new();
    let mut event_regions = Vec::new();
    for cell_index in 0..42u16 {
        let column = (cell_index % 7) as usize;
        let row = cell_index / 7;
        let date = grid_start + Duration::days(cell_index.into());
        let cell = Rect::new(
            columns[column + day_column_start].x,
            inner.y + 1 + row * row_height,
            columns[column + day_column_start].width,
            row_height,
        );
        cells.push(CalendarMonthCell {
            date,
            rect: screen_rect(cell),
        });
        for (line, event) in app
            .snapshot
            .events
            .iter()
            .filter(|event| enabled.contains(event.calendar_id.as_str()) && event.occurs_on(date))
            .take(row_height.saturating_sub(1) as usize)
            .enumerate()
        {
            event_regions.push(CalendarEventRegion {
                event_id: event.id.clone(),
                rect: screen_rect(Rect::new(cell.x, cell.y + 1 + line as u16, cell.width, 1)),
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
    let help = match app.mode {
        Mode::Normal => match app.view {
            View::Month => {
                " h/l day  j/k week  Tab event  Enter details  gg today  gd/gw/gm/ga view "
            }
            View::Week => " h/l day  j/k week  gg today  gd/gw/gm/ga view  n new  / search ",
            View::Day | View::Agenda => {
                " j/k select  h/l navigate  gg today  gd/gw/gm/ga view  n new  / search "
            }
        },
        Mode::Search => " Type to search  ↑/↓ select  Enter details  Esc close ",
        Mode::Form => " Tab field  ←/→ choices  Ctrl-S save  Esc cancel ",
        Mode::Palette => " Type to filter commands  ↑/↓ select  Enter run  Esc close ",
        Mode::DateJump => " Enter date as YYYY-MM-DD · Esc cancel ",
        Mode::CalendarManager => {
            return draw_footer_with_help(frame, app, area, calendar_manager_help(app));
        }
        Mode::CalendarManagerDetails => " Enter / Esc return to Calendar Manager ",
        Mode::CalendarCreate => " Tab field · ←/→ source · Ctrl-S save · Esc cancel ",
        Mode::CalendarRename => " Edit title · Ctrl-S save · Esc cancel ",
        Mode::CalendarColor => " Edit #RRGGBB · Ctrl-S save · Esc cancel ",
        Mode::CalendarDeleteConfirm => " y delete · n / Esc cancel ",
        Mode::QuickAdd => " Type expression · Ctrl-S save · Ctrl-E edit details · Esc cancel ",
        _ => " Esc close ",
    };
    let message = app
        .status
        .as_ref()
        .filter(|(_, _, at)| at.elapsed().as_secs() < 8);
    let line = if let Some((text, error, _)) = message {
        Line::from(Span::styled(
            format!(" {} ", text),
            Style::default()
                .fg(if *error {
                    RED
                } else {
                    Color::Rgb(80, 205, 130)
                })
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled(
                format!(
                    " {}  |  {}  |  ",
                    app.view.label().to_ascii_uppercase(),
                    app.active_date.format("%a %-d %b %Y")
                ),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if matches!(
                    app.visible_range_state,
                    crate::app::VisibleRangeState::Loading
                ) {
                    "LOADING RANGE"
                } else if matches!(
                    app.visible_range_state,
                    crate::app::VisibleRangeState::Failed(_)
                ) {
                    "RANGE FAILED"
                } else if matches!(app.backend_state, crate::backend::BackendState::Restarting) {
                    "BACKEND RESTARTING"
                } else if matches!(
                    app.backend_state,
                    crate::backend::BackendState::Disconnected
                ) {
                    "BACKEND DISCONNECTED"
                } else if app.syncing {
                    "SYNCING"
                } else if app.snapshot.updated_at.is_some() {
                    "SYNCED"
                } else {
                    "OFFLINE"
                },
                Style::default()
                    .fg(if app.snapshot.updated_at.is_some() {
                        Color::Rgb(80, 205, 130)
                    } else {
                        RED
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  |  {} calendars  ",
                    app.snapshot
                        .calendars
                        .iter()
                        .filter(|calendar| calendar.enabled)
                        .count()
                ),
                Style::default().fg(MUTED),
            ),
            Span::styled(help, Style::default().fg(MUTED)),
        ])
    };
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

fn draw_footer_with_help(frame: &mut Frame, app: &App, area: Rect, help: String) {
    let message = app
        .status
        .as_ref()
        .filter(|(_, _, at)| at.elapsed().as_secs() < 8);
    let line = if let Some((text, error, _)) = message {
        Line::from(Span::styled(
            format!(" {text} "),
            Style::default()
                .fg(if *error {
                    RED
                } else {
                    Color::Rgb(80, 205, 130)
                })
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(help, Style::default().fg(MUTED)))
    };
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
    let all_day_lanes = days
        .iter()
        .map(|day| {
            events
                .iter()
                .filter(|event| event.all_day && event.occurs_on(*day))
                .count()
        })
        .max()
        .unwrap_or(0)
        .min(3) as u16;
    let header_rows = 1;
    let all_day_rows = if all_day_lanes > 0 {
        all_day_lanes + 1
    } else {
        0
    };
    let grid_y = inner.y + header_rows + all_day_rows;
    let grid_height = inner.height.saturating_sub(header_rows + all_day_rows);
    if grid_height < 2 {
        return;
    }
    let columns = Layout::horizontal(
        [
            vec![Constraint::Length(7)],
            vec![Constraint::Ratio(1, days.len() as u32); days.len()],
        ]
        .concat(),
    )
    .split(inner);
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
                    grid_height,
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
                    frame.render_widget(
                        Paragraph::new(format!(" {}", event.title))
                            .style(event_style(color, selected)),
                        rect,
                    );
                }
            }
        }
    }
    let minutes_per_row = if grid_height >= 30 { 30 } else { 60 };
    let start_minute = (app.timeline_start_minute / minutes_per_row) * minutes_per_row;
    let viewport = TimelineViewport {
        start_minute,
        minutes_per_row,
        rows: grid_height,
    };
    for row in 0..grid_height {
        let minute = viewport
            .start_minute
            .saturating_add(row.saturating_mul(minutes_per_row));
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
    for (day_index, day) in days.iter().enumerate() {
        let cell = Rect::new(
            columns[day_index + 1].x,
            grid_y,
            columns[day_index + 1].width,
            grid_height,
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
            .filter_map(|(event_index, event)| {
                item_for_day(event_index, event.start, event.end, event.all_day, *day)
            })
            .collect::<Vec<_>>();
        for positioned in layout_overlaps(&items) {
            let Some((row, height)) =
                viewport.rows_for_range(positioned.start_minute, positioned.end_minute)
            else {
                continue;
            };
            // See the matching geometry path above. A terminal cannot render
            // an unlimited number of side-by-side overlaps in one cell.
            let columns = positioned.columns.min(cell.width.max(1));
            if positioned.column >= columns {
                continue;
            }
            let width = (cell.width / columns).max(1);
            let x = cell
                .x
                .saturating_add(positioned.column.saturating_mul(width));
            let right = if positioned.column + 1 == columns {
                cell.x.saturating_add(cell.width)
            } else {
                x.saturating_add(width)
            };
            let rect = Rect::new(
                x,
                cell.y + row,
                right.saturating_sub(x).max(1),
                height.max(1),
            );
            let event = events[positioned.event_index];
            let selected = positioned.event_index == app.selected_event;
            let color = calendar_color(app.calendar(&event.calendar_id).map(|c| c.color.as_str()));
            let label = if rect.height >= 3 {
                format!(
                    "{}\n{}–{}",
                    event.title,
                    time_text(app, &event.start.with_timezone(&Local)),
                    time_text(app, &event.end.with_timezone(&Local))
                )
            } else if rect.height == 2 {
                event.title.clone()
            } else {
                "•".into()
            };
            let widget = Paragraph::new(label)
                .wrap(Wrap { trim: true })
                .style(event_style(color, selected))
                .block(
                    Block::default()
                        .borders(if rect.height >= 2 && rect.width >= 5 {
                            Borders::ALL
                        } else {
                            Borders::empty()
                        })
                        .border_style(Style::default().fg(if selected { TEXT } else { color })),
                );
            frame.render_widget(widget, rect);
        }
    }
}

fn event_style(color: Color, selected: bool) -> Style {
    let foreground = readable_foreground(color);
    if selected {
        Style::default()
            .fg(foreground)
            .bg(color)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(foreground).bg(color)
    }
}

fn draw_month(frame: &mut Frame, app: &App, area: Rect) {
    let block = content_block(Line::from(Span::styled(
        format!(" {} ", date_title(app)),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 28 || inner.height < 8 {
        return;
    }
    let header_height = 1;
    let day_names = if app.config.week_start.eq_ignore_ascii_case("sunday") {
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
    } else {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
    };
    let mut constraints = Vec::new();
    if app.config.show_week_numbers {
        constraints.push(Constraint::Length(4));
    }
    constraints.extend([Constraint::Ratio(1, 7); 7]);
    let columns = Layout::horizontal(constraints).split(Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height,
    ));
    let day_column_start = usize::from(app.config.show_week_numbers);
    if app.config.show_week_numbers {
        frame.render_widget(
            Paragraph::new("Wk")
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
            Rect::new(columns[0].x, inner.y, columns[0].width, 1),
        );
    }
    for (index, name) in day_names.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(*name)
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
            Rect::new(
                columns[index + day_column_start].x,
                inner.y,
                columns[index + day_column_start].width,
                1,
            ),
        );
    }
    let first = app.active_date.with_day(1).unwrap();
    let offset = if app.config.week_start.eq_ignore_ascii_case("sunday") {
        first.weekday().num_days_from_sunday()
    } else {
        first.weekday().num_days_from_monday()
    };
    let grid_start = first - Duration::days(offset.into());
    let grid_height = inner.height - header_height;
    let row_height = (grid_height / 6).max(1);
    let enabled = app
        .snapshot
        .calendars
        .iter()
        .filter(|c| c.enabled)
        .map(|c| c.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let selected_id = app
        .selected_event_ref()
        .filter(|event| event.occurs_on(app.active_date))
        .map(|event| event.id.as_str());
    for cell_index in 0..42u16 {
        let column = (cell_index % 7) as usize;
        let row = cell_index / 7;
        let date = grid_start + Duration::days(cell_index.into());
        let cell = Rect::new(
            columns[column + day_column_start].x,
            inner.y + 1 + row * row_height,
            columns[column + day_column_start].width,
            row_height,
        );
        if app.config.show_week_numbers && column == 0 {
            frame.render_widget(
                Paragraph::new(format!("{:02}", date.iso_week().week()))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(MUTED)),
                Rect::new(columns[0].x, cell.y, columns[0].width, 1),
            );
        }
        let in_month = date.month() == app.active_date.month();
        let today = date == Local::now().date_naive();
        let active_date = date == app.active_date;
        let events = app
            .snapshot
            .events
            .iter()
            .filter(|e| enabled.contains(e.calendar_id.as_str()) && e.occurs_on(date))
            .collect::<Vec<_>>();
        let mut lines = vec![Line::from(Span::styled(
            // Keep the active calendar cursor recognizable in terminals that
            // do not faithfully render background colors. Today retains its
            // accent color while an active today still carries this marker.
            format!("{}{}", if active_date { "▶" } else { " " }, date.day()),
            Style::default()
                .fg(if today {
                    BG
                } else if in_month {
                    TEXT
                } else {
                    Color::Rgb(76, 82, 96)
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
        ))];
        for event in events.iter().take(row_height.saturating_sub(1) as usize) {
            let selected = selected_id == Some(event.id.as_str());
            let color = calendar_color(app.calendar(&event.calendar_id).map(|c| c.color.as_str()));
            lines.push(Line::from(Span::styled(
                format!("{} {}", if selected { "▶" } else { "•" }, event.title),
                Style::default()
                    .fg(if selected { BG } else { color })
                    .bg(if selected { color } else { BG })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )));
        }
        if events.len() > row_height.saturating_sub(1) as usize {
            lines.push(Line::from(Span::styled(
                format!(
                    "+{} more",
                    events.len() - row_height.saturating_sub(1) as usize
                ),
                Style::default().fg(MUTED),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::TOP | Borders::LEFT)
                    .border_style(Style::default().fg(Color::Rgb(38, 42, 51))),
            ),
            cell,
        );
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
    for event in &events {
        let date = event.display_start_date();
        if previous_date != Some(date) {
            let relative = match date.signed_duration_since(today).num_days() {
                0 => "Today".to_owned(),
                1 => "Tomorrow".to_owned(),
                -1 => "Yesterday".to_owned(),
                _ => date.format("%A, %-d %B").to_string(),
            };
            items.push(ListItem::new(Line::from(Span::styled(
                format!(" {relative}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))));
            previous_date = Some(date);
        }
        let calendar = app.calendar(&event.calendar_id);
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
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::Rgb(42, 47, 58)))
        .highlight_symbol("▶");
    let selected_list_index = events
        .iter()
        .take(app.selected_event.saturating_add(1))
        .map(|event| event.display_start_date())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        .saturating_add(app.selected_event);
    let mut state = ListState::default().with_selected(Some(selected_list_index));
    frame.render_stateful_widget(list, area, &mut state);
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
    let Some(event) = app.selected_event_ref() else {
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

fn draw_help(frame: &mut Frame) {
    let area = centered(frame.area(), 72, 82);
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("j/k or ↑/↓  Select event"),
        Line::from("h/l or ←/→  Previous/next period"),
        Line::from("gg            Today"),
        Line::from("gd/gw/gm/ga  Day/week/month/agenda"),
        Line::from(""),
        Line::from(Span::styled(
            "Events",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("n             New event"),
        Line::from("e             Edit selected event"),
        Line::from("D             Duplicate selected event"),
        Line::from("d             Delete selected event"),
        Line::from("u / Ctrl-R    Undo / redo confirmed event action"),
        Line::from("Enter         Event details"),
        Line::from(""),
        Line::from(Span::styled(
            "Calendars & system",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("c             Calendar list / filters"),
        Line::from("gc            Calendar Manager (read-only)"),
        Line::from(":             Command palette"),
        Line::from("/             Global search"),
        Line::from("r             Refresh all    R             Refresh visible range"),
        Line::from("q             Quit / close"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(popup_block(" Keyboard Shortcuts ", " ? or Esc close ")),
        area,
    );
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
        backend::CalendarBackend, config::Config, hit_test::CalendarHitTarget, model::Snapshot,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

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
        assert!(rendered.contains("▶10"), "{rendered:?}");
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
