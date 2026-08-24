//! Deterministic, offline Quick Add parsing. The parser receives all context
//! explicitly so it never depends on the machine clock or EventKit.

use crate::{
    config::EventConfig,
    model::{Alarm, CalendarInfo, EventTimeInput, RecurrenceFrequency, RecurrenceRule},
};
use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
    Weekday,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAddDraft {
    pub title: String,
    pub time: EventTimeInput,
    pub calendar_id: String,
    pub location: String,
    pub notes: String,
    pub recurrence: Vec<RecurrenceRule>,
    pub alarms: Vec<Alarm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickAddStatus {
    Ready,
    Incomplete,
    Ambiguous,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAddParseResult {
    pub status: QuickAddStatus,
    pub draft: Option<QuickAddDraft>,
    pub recognized_tokens: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct QuickAddContext<'a> {
    pub reference: DateTime<Local>,
    pub selected_date: NaiveDate,
    pub event: &'a EventConfig,
    pub calendars: &'a [CalendarInfo],
}

/// Parsing precedence: quoted control tokens, calendar/location tokens,
/// absolute/relative/weekday date, time range/single time, all-day, then title.
pub fn parse(input: &str, context: QuickAddContext<'_>) -> QuickAddParseResult {
    let mut words = lex(input);
    let mut recognized = vec![];
    let mut warnings = vec![];
    let mut calendar_id = None;
    let mut location = String::new();
    let mut date = None;
    let mut time = None;
    let mut range_end = None;
    let mut all_day = false;
    let mut recurrence = vec![];
    let mut alarms = vec![];

    for word in &mut words {
        if word.used {
            continue;
        }
        if let Some(value) = word.text.strip_prefix('#') {
            let matches = context
                .calendars
                .iter()
                .filter(|c| c.title.eq_ignore_ascii_case(value))
                .collect::<Vec<_>>();
            word.used = true;
            recognized.push(format!("calendar:{value}"));
            match matches.as_slice() {
                [calendar] => calendar_id = Some(calendar.id.clone()),
                [] => return invalid(recognized, vec!["Calendar was not found".into()]),
                _ => {
                    return ambiguous(recognized, vec![format!("Calendar “{value}” is ambiguous")]);
                }
            }
        } else if let Some(value) = word.text.strip_prefix('@') {
            if value.is_empty() {
                return invalid(recognized, vec!["Location is empty".into()]);
            }
            location = value.into();
            word.used = true;
            recognized.push(format!("location:{value}"));
        } else if let Some(value) = word.text.strip_prefix("/repeat:") {
            let frequency = match value.to_ascii_lowercase().as_str() {
                "daily" => RecurrenceFrequency::Daily,
                "weekly" => RecurrenceFrequency::Weekly,
                "monthly" => RecurrenceFrequency::Monthly,
                "yearly" => RecurrenceFrequency::Yearly,
                _ => return invalid(recognized, vec!["Unsupported repeat value".into()]),
            };
            recurrence.push(RecurrenceRule {
                frequency,
                interval: 1,
                days_of_week: vec![],
                occurrence_count: None,
                end_date: None,
            });
            word.used = true;
            recognized.push(format!("repeat:{value}"));
        } else if let Some(value) = word.text.strip_prefix("/alert:") {
            let seconds = parse_alarm(value).ok_or(());
            let Ok(seconds) = seconds else {
                return invalid(recognized, vec!["Invalid alert value".into()]);
            };
            alarms.push(Alarm {
                relative_seconds: Some(-seconds),
                absolute_date: None,
                is_editable: true,
            });
            word.used = true;
            recognized.push(format!("alert:{value}"));
        }
    }
    for word in &mut words {
        if word.used {
            continue;
        }
        if is_all_day(&word.text) {
            all_day = true;
            word.used = true;
            recognized.push("all-day".into());
            continue;
        }
        if let Some(value) = parse_absolute_date(&word.text) {
            if date.replace(value).is_some() {
                return invalid(recognized, vec!["Conflicting dates".into()]);
            }
            word.used = true;
            recognized.push(format!("date:{}", word.text));
            continue;
        }
        if let Some(value) = parse_relative_date(&word.text, context.reference.date_naive()) {
            if date.replace(value).is_some() {
                return invalid(recognized, vec!["Conflicting dates".into()]);
            }
            word.used = true;
            recognized.push(format!("date:{}", word.text));
            continue;
        }
        if let Some(value) = parse_weekday(&word.text, context.reference.date_naive()) {
            if date.replace(value).is_some() {
                return invalid(recognized, vec!["Conflicting dates".into()]);
            }
            word.used = true;
            recognized.push(format!("weekday:{}", word.text));
        }
    }
    for word in &mut words {
        if word.used {
            continue;
        }
        if let Some((start, end)) = parse_range(&word.text) {
            if time.replace(start).is_some() || range_end.replace(end).is_some() {
                return invalid(recognized, vec!["Conflicting times".into()]);
            }
            if end <= start {
                return invalid(recognized, vec!["End time must be after start time".into()]);
            }
            word.used = true;
            recognized.push(format!("time:{}", word.text));
            continue;
        }
        if looks_like_time(&word.text) {
            let Some(value) = parse_time(&word.text) else {
                return invalid(recognized, vec![format!("Invalid time: {}", word.text)]);
            };
            if time.replace(value).is_some() {
                return invalid(recognized, vec!["Conflicting times".into()]);
            }
            word.used = true;
            recognized.push(format!("time:{}", word.text));
        }
    }
    let title = words
        .iter()
        .filter(|word| !word.used)
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned();
    if title.is_empty() {
        return QuickAddParseResult {
            status: QuickAddStatus::Incomplete,
            draft: None,
            recognized_tokens: recognized,
            warnings: vec!["Add an event title".into()],
        };
    }
    let calendar_id = match calendar_id.or_else(|| {
        context
            .calendars
            .iter()
            .find(|c| c.permissions.can_create_events)
            .map(|c| c.id.clone())
    }) {
        Some(id) => id,
        None => return invalid(recognized, vec!["No writable calendar is available".into()]),
    };
    let date = date.unwrap_or(context.selected_date);
    if all_day && !alarms.is_empty() {
        return invalid(
            recognized,
            vec!["Relative reminders for all-day events are not supported yet".into()],
        );
    }
    let time = if all_day {
        let Some(end_date_exclusive) = date.checked_add_signed(Duration::days(1)) else {
            return invalid(recognized, vec!["All-day end date is out of range".into()]);
        };
        match EventTimeInput::all_day(date, end_date_exclusive) {
            Ok(time) => time,
            Err(message) => return invalid(recognized, vec![message]),
        }
    } else {
        let start_time = time.unwrap_or_else(|| default_time(context.event));
        let Some(start) = local_datetime(date, start_time) else {
            return invalid(
                recognized,
                vec!["Time is invalid in the local timezone".into()],
            );
        };
        let end = if let Some(end_time) = range_end {
            local_datetime(date, end_time)
        } else {
            Some(start + Duration::minutes(i64::from(context.event.default_duration_minutes)))
        };
        let Some(end) = end else {
            return invalid(
                recognized,
                vec!["End time is invalid in the local timezone".into()],
            );
        };
        match EventTimeInput::timed(start.with_timezone(&Utc), end.with_timezone(&Utc)) {
            Ok(time) => time,
            Err(message) => return invalid(recognized, vec![message]),
        }
    };
    QuickAddParseResult {
        status: QuickAddStatus::Ready,
        draft: Some(QuickAddDraft {
            title,
            time,
            calendar_id,
            location,
            notes: String::new(),
            recurrence,
            alarms,
        }),
        recognized_tokens: recognized,
        warnings: std::mem::take(&mut warnings),
    }
}

#[derive(Debug)]
struct Word {
    text: String,
    used: bool,
}
fn lex(input: &str) -> Vec<Word> {
    let mut out = vec![];
    let mut chars = input.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(first) = chars.next() else { break };
        let mut value = String::new();
        value.push(first);
        if matches!(first, '#' | '@') && chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                value.push(c);
            }
        } else {
            while chars.peek().is_some_and(|c| !c.is_whitespace()) {
                value.push(chars.next().unwrap());
            }
        }
        out.push(Word {
            text: value,
            used: false,
        });
    }
    out
}
fn invalid(recognized_tokens: Vec<String>, warnings: Vec<String>) -> QuickAddParseResult {
    QuickAddParseResult {
        status: QuickAddStatus::Invalid,
        draft: None,
        recognized_tokens,
        warnings,
    }
}
fn ambiguous(recognized_tokens: Vec<String>, warnings: Vec<String>) -> QuickAddParseResult {
    QuickAddParseResult {
        status: QuickAddStatus::Ambiguous,
        draft: None,
        recognized_tokens,
        warnings,
    }
}
fn parse_absolute_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .or_else(|| NaiveDate::parse_from_str(value, "%d.%m.%Y").ok())
}
fn parse_relative_date(value: &str, today: NaiveDate) -> Option<NaiveDate> {
    match value.to_ascii_lowercase().as_str() {
        "today" | "heute" => Some(today),
        "tomorrow" | "morgen" => Some(today + Duration::days(1)),
        "yesterday" => Some(today - Duration::days(1)),
        _ => None,
    }
}
fn parse_weekday(value: &str, today: NaiveDate) -> Option<NaiveDate> {
    let target = match value.to_ascii_lowercase().as_str() {
        "monday" | "mon" | "montag" => Weekday::Mon,
        "tuesday" | "tue" | "dienstag" => Weekday::Tue,
        "wednesday" | "wed" | "mittwoch" => Weekday::Wed,
        "thursday" | "thu" | "donnerstag" => Weekday::Thu,
        "friday" | "fri" | "freitag" => Weekday::Fri,
        "saturday" | "sat" | "samstag" => Weekday::Sat,
        "sunday" | "sun" | "sonntag" => Weekday::Sun,
        _ => return None,
    };
    let delta =
        (target.num_days_from_monday() as i64 - today.weekday().num_days_from_monday() as i64 + 7)
            % 7;
    Some(today + Duration::days(if delta == 0 { 7 } else { delta }))
}
fn parse_time(value: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M").ok()
}
fn looks_like_time(value: &str) -> bool {
    value.contains(':') && !value.contains("//")
}
fn parse_range(value: &str) -> Option<(NaiveTime, NaiveTime)> {
    let (a, b) = value.split_once('-').or_else(|| value.split_once('–'))?;
    Some((parse_time(a)?, parse_time(b)?))
}
fn is_all_day(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "all-day" | "allday" | "ganztägig"
    )
}
fn parse_alarm(value: &str) -> Option<i64> {
    let (n, u) = value.split_at(value.len().checked_sub(1)?);
    let n = n.parse::<i64>().ok()?;
    match u {
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        "d" => Some(n * 86400),
        _ => None,
    }
}
fn default_time(config: &EventConfig) -> NaiveTime {
    NaiveTime::parse_from_str(&config.default_start_time, "%H:%M")
        .unwrap_or(NaiveTime::from_hms_opt(9, 0, 0).unwrap())
}
fn local_datetime(date: NaiveDate, time: NaiveTime) -> Option<DateTime<Local>> {
    match Local.from_local_datetime(&NaiveDateTime::new(date, time)) {
        chrono::LocalResult::Single(value) => Some(value),
        chrono::LocalResult::Ambiguous(value, _) => Some(value),
        chrono::LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EventConfig;
    fn calendars() -> Vec<CalendarInfo> {
        vec![CalendarInfo {
            id: "personal".into(),
            source_id: String::new(),
            permissions: crate::model::CalendarPermissions {
                can_create_events: true,
                ..Default::default()
            },
            title: "Personal".into(),
            account: String::new(),
            provider: String::new(),
            color: String::new(),
            is_writable: true,
            enabled: true,
        }]
    }
    fn ctx<'a>(calendars: &'a [CalendarInfo]) -> QuickAddContext<'a> {
        QuickAddContext {
            reference: Local
                .with_ymd_and_hms(2026, 8, 22, 16, 0, 0)
                .single()
                .unwrap(),
            selected_date: NaiveDate::from_ymd_opt(2026, 9, 20).unwrap(),
            event: Box::leak(Box::new(EventConfig::default())),
            calendars,
        }
    }
    #[test]
    fn parses_dates_times_languages_and_tokens() {
        let c = calendars();
        let r = parse("Lunch tomorrow 13:00 #Personal @\"Munich Office\"", ctx(&c));
        assert_eq!(r.status, QuickAddStatus::Ready);
        let d = r.draft.unwrap();
        assert_eq!(d.title, "Lunch");
        assert_eq!(d.location, "Munich Office");
        assert_eq!(d.calendar_id, "personal");
        let (start, _) = d.time.as_timed_range().unwrap();
        assert_eq!(
            start.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 8, 23).unwrap()
        );
        assert_eq!(
            start.with_timezone(&Local).time(),
            NaiveTime::from_hms_opt(13, 0, 0).unwrap()
        );
        let g = parse("Gym heute 19:00", ctx(&c));
        assert_eq!(
            g.draft
                .unwrap()
                .time
                .as_timed_range()
                .unwrap()
                .0
                .with_timezone(&Local)
                .date_naive(),
            NaiveDate::from_ymd_opt(2026, 8, 22).unwrap()
        );
        let f = parse("Meeting Freitag 10:00", ctx(&c));
        assert_eq!(
            f.draft
                .unwrap()
                .time
                .as_timed_range()
                .unwrap()
                .0
                .with_timezone(&Local)
                .date_naive(),
            NaiveDate::from_ymd_opt(2026, 8, 28).unwrap()
        );
    }
    #[test]
    fn parses_absolute_range_all_day_and_diagnostics() {
        let c = calendars();
        let r = parse("Dentist 20.09.2026 14:30", ctx(&c));
        assert_eq!(
            r.draft
                .unwrap()
                .time
                .as_timed_range()
                .unwrap()
                .0
                .with_timezone(&Local)
                .date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()
        );
        let r = parse("Dinner tomorrow 18:00-20:30", ctx(&c));
        let d = r.draft.unwrap();
        assert_eq!(
            d.time
                .as_timed_range()
                .unwrap()
                .1
                .with_timezone(&Local)
                .time(),
            NaiveTime::from_hms_opt(20, 30, 0).unwrap()
        );
        assert_eq!(
            parse("Vacation morgen all-day", ctx(&c))
                .draft
                .unwrap()
                .time
                .as_all_day_range(),
            Some((
                NaiveDate::from_ymd_opt(2026, 8, 23).unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 24).unwrap(),
            ))
        );
        assert_eq!(
            parse("Lunch tomorrow 25:00", ctx(&c)).status,
            QuickAddStatus::Invalid
        );
        assert_eq!(
            parse("Meeting tomorrow 2026-09-20 10:00", ctx(&c)).status,
            QuickAddStatus::Invalid
        );
        assert_eq!(
            parse("Meeting 10:00 14:00", ctx(&c)).status,
            QuickAddStatus::Invalid
        );
        assert_eq!(
            parse("tomorrow 13:00", ctx(&c)).status,
            QuickAddStatus::Incomplete
        );
    }

    #[test]
    fn all_day_quick_add_keeps_calendar_dates_without_timezone_conversion() {
        let calendars = calendars();
        let expected = EventTimeInput::AllDay {
            start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
        };
        let first = parse("Holiday 2026-09-10 all-day", ctx(&calendars));
        let later_context = QuickAddContext {
            reference: Local
                .with_ymd_and_hms(2026, 12, 31, 23, 0, 0)
                .single()
                .unwrap(),
            ..ctx(&calendars)
        };
        let second = parse("Holiday 2026-09-10 all-day", later_context);
        assert_eq!(first.draft.unwrap().time, expected);
        assert_eq!(second.draft.unwrap().time, expected);
    }

    #[test]
    fn rejects_relative_alerts_for_all_day_events_but_keeps_timed_alerts() {
        let c = calendars();
        let all_day = parse("Holiday all-day /alert:15m", ctx(&c));
        assert_eq!(all_day.status, QuickAddStatus::Invalid);
        assert!(all_day.draft.is_none());
        let timed = parse("Meeting tomorrow 14:00 /alert:15m", ctx(&c));
        assert_eq!(timed.status, QuickAddStatus::Ready);
        assert_eq!(timed.draft.unwrap().alarms[0].relative_seconds, Some(-900));
    }
}
