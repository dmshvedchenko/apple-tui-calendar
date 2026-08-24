use chrono::{DateTime, Duration, Local, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};

/// Provider-neutral half-open UTC transport interval for event fetching.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstantRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Provider-neutral half-open calendar-date interval for floating all-day
/// event inclusion. These dates intentionally carry no time zone.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CalendarDateRange {
    pub start_date: NaiveDate,
    pub end_date_exclusive: NaiveDate,
}

/// Complete fetch intent. Timed events use `instant_range`; `all_day_range`
/// communicates the separate calendar-date requirement when the caller has
/// one. Provider implementations may add date-aware handling later without
/// changing the timed-event transport contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FetchRequest {
    pub instant_range: InstantRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_day_range: Option<CalendarDateRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CalendarInfo {
    pub id: String,
    /// Stable EventKit source identity; unlike the account title this is safe
    /// to use for future source-aware calendar-management requests.
    #[serde(default)]
    pub source_id: String,
    #[serde(default)]
    pub permissions: CalendarPermissions,
    pub title: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default)]
    pub is_writable: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CalendarPermissions {
    pub can_create_events: bool,
    pub can_modify_events: bool,
    pub can_modify_metadata: bool,
    pub can_delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CalendarError {
    NotFound,
    PermissionDenied,
    ReadOnly,
    CannotModifyMetadata,
    CannotDelete,
    SourceNotFound,
    SourceUnavailable,
    Unsupported,
    InvalidTitle,
    InvalidColor,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CalendarErrorEnvelope {
    #[serde(rename = "type")]
    pub kind: CalendarError,
    #[serde(default)]
    pub message: String,
}

fn default_color() -> String {
    "#5E9EFF".into()
}

fn default_true() -> bool {
    true
}

/// Native-service capability discovery for the future calendar manager.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CalendarCapabilities {
    pub can_list_sources: bool,
    pub can_create: bool,
    pub can_update: bool,
    pub can_delete: bool,
    pub can_change_color: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CalendarSource {
    pub id: String,
    pub title: String,
    pub source_type: String,
    pub is_writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateCalendarRequest {
    pub title: String,
    pub color: String,
    pub source_id: String,
}

/// Successful `calendar.create` response. The wire shape intentionally uses
/// the same provider-neutral calendar metadata as discovery.
pub type CreateCalendarResponse = CalendarInfo;

/// Provider-neutral request for changing calendar metadata without exposing
/// EventKit types to the application layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RenameCalendarRequest {
    pub calendar_id: String,
    pub title: String,
}

/// Successful `calendar.rename` response uses the regular discovery model so
/// callers can replace stale metadata atomically.
pub type RenameCalendarResponse = CalendarInfo;

/// Provider-neutral request for changing an EventKit calendar's display
/// color. Colors use the v2 wire format `#RRGGBB` only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SetCalendarColorRequest {
    pub calendar_id: String,
    pub color: String,
}

/// Successful `calendar.setColor` response is the refreshed calendar model.
pub type SetCalendarColorResponse = CalendarInfo;

/// Provider-neutral request for removing a calendar. The UI must obtain an
/// explicit confirmation before sending this destructive operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteCalendarRequest {
    pub calendar_id: String,
}

/// Acknowledges the stable calendar ID that was removed. This avoids relying
/// on a platform-specific empty/null response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteCalendarResponse {
    pub calendar_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub status: InvitationStatus,
    #[serde(default)]
    pub is_current_user: bool,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub participant_type: String,
    #[serde(default)]
    pub schedule_status: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum InvitationStatus {
    Accepted,
    Declined,
    Tentative,
    Pending,
    Delegated,
    #[default]
    Unknown,
}

impl std::fmt::Display for InvitationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Accepted => "accepted",
                Self::Declined => "declined",
                Self::Tentative => "maybe",
                Self::Pending => "pending",
                Self::Delegated => "delegated",
                Self::Unknown => "unknown",
            }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Alarm {
    #[serde(default)]
    pub relative_seconds: Option<i64>,
    #[serde(default)]
    pub absolute_date: Option<DateTime<Utc>>,
    /// Supplied by the EventKit bridge. Missing metadata from an older cache
    /// is conservatively treated as protected rather than assumed editable.
    #[serde(default)]
    pub is_editable: bool,
}

/// Explicit update-only alarm intent. Creation always supplies a concrete
/// alarm list because there is no provider object to preserve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", content = "alarms", rename_all = "camelCase")]
pub enum AlarmMutation {
    #[default]
    Preserve,
    Replace(Vec<Alarm>),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RecurrenceFrequency {
    Daily,
    #[default]
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecurrenceRule {
    pub frequency: RecurrenceFrequency,
    #[serde(default = "one")]
    pub interval: u32,
    #[serde(default)]
    pub days_of_week: Vec<String>,
    pub occurrence_count: Option<u32>,
    pub end_date: Option<DateTime<Utc>>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum Availability {
    #[default]
    Busy,
    Free,
    Tentative,
    Unavailable,
    NotSupported,
}

impl std::fmt::Display for Availability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Busy => "busy",
                Self::Free => "free",
                Self::Tentative => "tentative",
                Self::Unavailable => "unavailable",
                Self::NotSupported => "not supported",
            }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub all_day: bool,
    /// Authoritative provider-normalized calendar-date identity for an
    /// all-day event. These fields are deliberately optional so cached IPC
    /// v2 payloads created before their introduction remain readable. The
    /// legacy UTC instants must not be used to reconstruct a missing value:
    /// EventKit floating dates may already have been interpreted in another
    /// helper timezone.
    #[serde(default)]
    pub all_day_start_date: Option<NaiveDate>,
    #[serde(default)]
    pub all_day_end_date_exclusive: Option<NaiveDate>,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub time_zone: String,
    #[serde(default)]
    pub time_zone_provenance: TimeZoneProvenance,
    #[serde(default)]
    pub availability: Availability,
    #[serde(default)]
    pub organizer: Option<Attendee>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    #[serde(default)]
    pub alarms: Vec<Alarm>,
    #[serde(default)]
    pub recurrence: Vec<RecurrenceRule>,
    #[serde(default)]
    pub has_recurrence: bool,
    #[serde(default)]
    pub is_detached: bool,
    #[serde(default)]
    pub invitation_status: InvitationStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum TimeZoneProvenance {
    ExplicitEvent,
    HelperFallback,
    #[default]
    Unknown,
}

impl Event {
    /// Returns the trusted provider-normalized all-day interval only when the
    /// payload explicitly supplied a valid calendar-date range. This must not
    /// fall back to `start`/`end`: those compatibility instants do not retain
    /// the identity of an EventKit floating all-day date.
    pub fn all_day_date_range(&self) -> Option<(NaiveDate, NaiveDate)> {
        let (start, end) = (self.all_day_start_date?, self.all_day_end_date_exclusive?);
        (self.all_day && end > start).then_some((start, end))
    }

    /// Read-side membership for a half-open calendar-date interval. Trusted
    /// all-day metadata is authoritative; missing or invalid metadata leaves
    /// the event on the intentionally isolated legacy compatibility path.
    pub fn all_day_intersects_dates(&self, start: NaiveDate, end_exclusive: NaiveDate) -> bool {
        self.all_day_date_range()
            .is_some_and(|(event_start, event_end)| {
                event_start < end_exclusive && event_end > start
            })
    }

    /// Date used by read-only grouping and details. Trusted all-day metadata
    /// must never be reinterpreted through either `Local` or `time_zone`.
    pub fn display_start_date(&self) -> NaiveDate {
        self.all_day_date_range()
            .map(|(start, _)| start)
            .unwrap_or_else(|| self.start.with_timezone(&Local).date_naive())
    }

    pub fn occurs_on(&self, date: NaiveDate) -> bool {
        if let Some((start, end_exclusive)) = self.all_day_date_range() {
            return start <= date && date < end_exclusive;
        }
        // Compatibility fallback for timed events and old all-day payloads
        // whose original floating calendar-date identity is unavailable.
        let local_start = self.start.with_timezone(&Local).date_naive();
        let local_end = self.end.with_timezone(&Local).date_naive();
        local_start <= date && local_end >= date
    }

    pub fn searchable_text(&self, calendar: Option<&CalendarInfo>) -> String {
        let attendees = self
            .attendees
            .iter()
            .map(|a| format!("{} {}", a.name, a.email))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{} {} {} {} {}",
            self.title,
            self.notes,
            self.location,
            attendees,
            calendar.map(|c| c.title.as_str()).unwrap_or_default()
        )
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
    }
}

/// Authoritative temporal input for a new event. Timed events carry instants;
/// all-day events carry floating calendar dates. The legacy variant is kept
/// only so pre-refactor drafts can remain explicit about their lost date
/// identity until their callers migrate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EventTimeInput {
    Timed {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    AllDay {
        start_date: NaiveDate,
        end_date_exclusive: NaiveDate,
    },
    /// Compatibility-only representation for old all-day drafts that had
    /// already discarded their floating calendar-date identity. New code must
    /// use `AllDay` instead.
    LegacyAllDayUnknown {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
}

impl EventTimeInput {
    pub fn timed(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Self, String> {
        (end > start)
            .then_some(Self::Timed { start, end })
            .ok_or_else(|| "End must be after start".into())
    }

    pub fn all_day(start_date: NaiveDate, end_date_exclusive: NaiveDate) -> Result<Self, String> {
        (end_date_exclusive > start_date)
            .then_some(Self::AllDay {
                start_date,
                end_date_exclusive,
            })
            .ok_or_else(|| "All-day end date must be after start date".into())
    }

    /// Transitional adapter for existing callers that still produce EventKit's
    /// legacy UTC compatibility instants. It deliberately does not claim those
    /// instants identify a floating all-day range.
    pub fn legacy_all_day_unknown(
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Self, String> {
        (end > start)
            .then_some(Self::LegacyAllDayUnknown { start, end })
            .ok_or_else(|| "End must be after start".into())
    }

    pub fn is_all_day(&self) -> bool {
        !matches!(self, Self::Timed { .. })
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Timed { start, end } | Self::LegacyAllDayUnknown { start, end }
                if end > start =>
            {
                Ok(())
            }
            Self::AllDay {
                start_date,
                end_date_exclusive,
            } if end_date_exclusive > start_date => Ok(()),
            Self::Timed { .. } | Self::LegacyAllDayUnknown { .. } => {
                Err("End must be after start".into())
            }
            Self::AllDay { .. } => Err("All-day end date must be after start date".into()),
        }
    }

    pub fn as_timed_range(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match self {
            Self::Timed { start, end } if end > start => Some((*start, *end)),
            _ => None,
        }
    }

    pub fn as_all_day_range(&self) -> Option<(NaiveDate, NaiveDate)> {
        match self {
            Self::AllDay {
                start_date,
                end_date_exclusive,
            } if end_date_exclusive > start_date => Some((*start_date, *end_date_exclusive)),
            _ => None,
        }
    }

    pub fn as_legacy_range(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match self {
            Self::LegacyAllDayUnknown { start, end } if end > start => Some((*start, *end)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventDraft {
    pub id: Option<String>,
    pub calendar_id: String,
    pub title: String,
    pub time: EventTimeInput,
    pub location: String,
    pub notes: String,
    pub url: String,
    pub time_zone: String,
    pub availability: Availability,
    pub attendees: Vec<String>,
    pub alarms: Vec<Alarm>,
    pub recurrence: Vec<RecurrenceRule>,
}

impl<'de> Deserialize<'de> for EventDraft {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            id: Option<String>,
            calendar_id: String,
            title: String,
            #[serde(default)]
            time: Option<EventTimeInput>,
            #[serde(default)]
            start: Option<DateTime<Utc>>,
            #[serde(default)]
            end: Option<DateTime<Utc>>,
            #[serde(default)]
            all_day: bool,
            #[serde(default)]
            location: String,
            #[serde(default)]
            notes: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            time_zone: String,
            #[serde(default)]
            availability: Availability,
            #[serde(default)]
            attendees: Vec<String>,
            #[serde(default)]
            alarms: Vec<Alarm>,
            #[serde(default)]
            recurrence: Vec<RecurrenceRule>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let time = match wire.time {
            Some(time) => time,
            None => {
                let start = wire
                    .start
                    .ok_or_else(|| serde::de::Error::missing_field("time"))?;
                let end = wire
                    .end
                    .ok_or_else(|| serde::de::Error::missing_field("time"))?;
                if wire.all_day {
                    EventTimeInput::legacy_all_day_unknown(start, end)
                } else {
                    EventTimeInput::timed(start, end)
                }
                .map_err(serde::de::Error::custom)?
            }
        };
        Ok(Self {
            id: wire.id,
            calendar_id: wire.calendar_id,
            title: wire.title,
            time,
            location: wire.location,
            notes: wire.notes,
            url: wire.url,
            time_zone: wire.time_zone,
            availability: wire.availability,
            attendees: wire.attendees,
            alarms: wire.alarms,
            recurrence: wire.recurrence,
        })
    }
}

impl EventDraft {
    pub fn new(calendar_id: String, date: NaiveDate) -> Self {
        let now = Local::now();
        let next_hour = (now + Duration::hours(1))
            .with_minute(0)
            .unwrap()
            .with_second(0)
            .unwrap()
            .with_nanosecond(0)
            .unwrap();
        let start_local = if date == now.date_naive() {
            next_hour
        } else {
            date.and_hms_opt(9, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .single()
                .unwrap_or(next_hour)
        };
        Self {
            id: None,
            calendar_id,
            title: String::new(),
            time: EventTimeInput::timed(
                start_local.with_timezone(&Utc),
                (start_local + Duration::hours(1)).with_timezone(&Utc),
            )
            .expect("default event duration is positive"),
            location: String::new(),
            notes: String::new(),
            url: String::new(),
            time_zone: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
            availability: Availability::Busy,
            attendees: vec![],
            alarms: vec![],
            recurrence: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum EventSpan {
    #[default]
    ThisEvent,
    FutureEvents,
}

/// Provider-neutral scope for a mutation of a recurring event. This is kept
/// separate from the legacy editor `EventSpan` so new API callers must choose
/// a scope explicitly rather than inheriting a default.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RecurrenceMutationScope {
    ThisEvent,
    FutureEvents,
}

impl From<RecurrenceMutationScope> for EventSpan {
    fn from(scope: RecurrenceMutationScope) -> Self {
        match scope {
            RecurrenceMutationScope::ThisEvent => Self::ThisEvent,
            RecurrenceMutationScope::FutureEvents => Self::FutureEvents,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventMutationValidationError {
    RecurrenceScopeRequired,
    UnsupportedRecurrenceScope,
}

impl RecurrenceMutationScope {
    /// Decodes only the scopes the provider contract can safely carry. An
    /// entire-series request is deliberately rejected rather than downgraded.
    pub fn from_wire(value: &str) -> Result<Self, EventMutationValidationError> {
        match value {
            "thisEvent" => Ok(Self::ThisEvent),
            "futureEvents" => Ok(Self::FutureEvents),
            _ => Err(EventMutationValidationError::UnsupportedRecurrenceScope),
        }
    }
}

/// Prepared provider-neutral update request. Recurring events require an
/// explicit scope; callers of ordinary event updates leave it as `None`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEventRequest {
    pub event: EventDraft,
    #[serde(default)]
    pub alarm_mutation: AlarmMutation,
    #[serde(default)]
    pub time_mutation: EventTimeMutation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_scope: Option<RecurrenceMutationScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EventTimeMutation {
    /// Existing provider temporal state is untouched.
    Preserve,
    /// Replaces an existing all-day interval using floating calendar dates.
    ReplaceAllDay {
        start_date: NaiveDate,
        end_date_exclusive: NaiveDate,
    },
    /// Compatibility path for the existing timed-event update contract.
    #[default]
    ReplaceLegacy,
}

impl UpdateEventRequest {
    pub fn validate(&self) -> Result<(), EventMutationValidationError> {
        if !self.event.recurrence.is_empty() && self.recurrence_scope.is_none() {
            return Err(EventMutationValidationError::RecurrenceScopeRequired);
        }
        Ok(())
    }
}

/// Prepared provider-neutral delete request. Delete callers provide whether
/// the selected event is recurring because a delete request contains only the
/// stable event ID, not a copy of the event payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteEventRequest {
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_scope: Option<RecurrenceMutationScope>,
}

impl DeleteEventRequest {
    pub fn validate(&self, is_recurring: bool) -> Result<(), EventMutationValidationError> {
        if is_recurring && self.recurrence_scope.is_none() {
            return Err(EventMutationValidationError::RecurrenceScopeRequired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InvitationResponse {
    Accept,
    Decline,
    Maybe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum AuthorizationStatus {
    FullAccess,
    WriteOnly,
    Denied,
    Restricted,
    #[default]
    NotDetermined,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub calendars: Vec<CalendarInfo>,
    pub events: Vec<Event>,
    pub authorization: AuthorizationStatus,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            calendars: vec![],
            events: vec![],
            authorization: AuthorizationStatus::NotDetermined,
            updated_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_day_event_json(start: &str, end: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "all-day-1",
            "calendarId": "calendar-1",
            "title": "All-day",
            "start": "2026-09-09T22:00:00Z",
            "end": "2026-09-12T22:00:00Z",
            "allDay": true,
            "allDayStartDate": start,
            "allDayEndDateExclusive": end,
        })
    }

    #[test]
    fn all_day_date_metadata_uses_iso_dates_and_requires_a_valid_interval() {
        let event: Event =
            serde_json::from_value(all_day_event_json("2026-09-10", "2026-09-13")).unwrap();
        assert_eq!(
            event.all_day_date_range(),
            Some((
                NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            ))
        );
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["allDayStartDate"], "2026-09-10");
        assert_eq!(encoded["allDayEndDateExclusive"], "2026-09-13");

        let invalid: Event =
            serde_json::from_value(all_day_event_json("2026-09-10", "2026-09-10")).unwrap();
        assert_eq!(invalid.all_day_date_range(), None);
    }

    #[test]
    fn legacy_all_day_payload_does_not_guess_date_metadata() {
        let mut payload = all_day_event_json("2026-09-10", "2026-09-11");
        let object = payload.as_object_mut().unwrap();
        object.remove("allDayStartDate");
        object.remove("allDayEndDateExclusive");
        let event: Event = serde_json::from_value(payload).unwrap();
        assert_eq!(event.all_day_start_date, None);
        assert_eq!(event.all_day_end_date_exclusive, None);
        assert_eq!(event.all_day_date_range(), None);
    }

    #[test]
    fn trusted_all_day_membership_is_half_open_and_ignores_legacy_instants() {
        let event: Event =
            serde_json::from_value(all_day_event_json("2026-09-10", "2026-09-13")).unwrap();
        let date = |day| NaiveDate::from_ymd_opt(2026, 9, day).unwrap();
        assert!(!event.occurs_on(date(9)));
        assert!(event.occurs_on(date(10)));
        assert!(event.occurs_on(date(11)));
        assert!(event.occurs_on(date(12)));
        assert!(!event.occurs_on(date(13)));
        assert!(event.all_day_intersects_dates(date(10), date(13)));
        assert!(!event.all_day_intersects_dates(date(9), date(10)));
        assert!(event.all_day_intersects_dates(date(10), date(11)));
        assert!(event.all_day_intersects_dates(date(12), date(13)));
        assert!(!event.all_day_intersects_dates(date(13), date(14)));

        let mut alternate_legacy_instants = event.clone();
        alternate_legacy_instants.start = "2026-09-10T04:00:00Z".parse().unwrap();
        alternate_legacy_instants.end = "2026-09-13T03:59:59Z".parse().unwrap();
        assert_eq!(
            alternate_legacy_instants.display_start_date(),
            date(10),
            "trusted dates, not compatibility timestamps, define read-side identity"
        );
        assert!(alternate_legacy_instants.occurs_on(date(12)));

        let dst_start = NaiveDate::from_ymd_opt(2026, 3, 28).unwrap();
        let dst_end = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let dst = Event {
            all_day_start_date: Some(dst_start),
            all_day_end_date_exclusive: Some(dst_end),
            ..event
        };
        assert!(dst.all_day_intersects_dates(
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
            NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
        ));
    }

    #[test]
    fn invalid_or_missing_all_day_metadata_uses_legacy_compatibility_behavior() {
        let mut invalid: Event =
            serde_json::from_value(all_day_event_json("2026-09-10", "2026-09-10")).unwrap();
        invalid.start = "2026-09-09T22:00:00Z".parse().unwrap();
        invalid.end = "2026-09-10T22:00:00Z".parse().unwrap();
        assert_eq!(invalid.all_day_date_range(), None);
        assert_eq!(
            invalid.display_start_date(),
            invalid.start.with_timezone(&Local).date_naive()
        );
    }

    #[test]
    fn fetch_request_serializes_distinct_instant_and_calendar_date_ranges() {
        let instant_range = InstantRange {
            start: "2026-09-10T00:00:00Z".parse().unwrap(),
            end: "2026-09-11T00:00:00Z".parse().unwrap(),
        };
        let all_day_range = CalendarDateRange {
            start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
        };
        let combined = FetchRequest {
            instant_range,
            all_day_range: Some(all_day_range),
        };
        assert_eq!(
            serde_json::to_value(&combined).unwrap(),
            serde_json::json!({
                "instantRange": {
                    "start": "2026-09-10T00:00:00Z",
                    "end": "2026-09-11T00:00:00Z",
                },
                "allDayRange": {
                    "startDate": "2026-09-10",
                    "endDateExclusive": "2026-09-11",
                },
            })
        );
        assert_eq!(
            serde_json::from_value::<FetchRequest>(serde_json::to_value(&combined).unwrap())
                .unwrap(),
            combined
        );
    }

    #[test]
    fn timed_only_and_all_day_only_fetch_intents_are_explicit() {
        let instant_range = InstantRange {
            start: "2026-03-28T00:00:00Z".parse().unwrap(),
            end: "2026-03-31T00:00:00Z".parse().unwrap(),
        };
        let timed_only = FetchRequest {
            instant_range,
            all_day_range: None,
        };
        assert_eq!(
            serde_json::to_value(&timed_only).unwrap(),
            serde_json::json!({ "instantRange": { "start": "2026-03-28T00:00:00Z", "end": "2026-03-31T00:00:00Z" } })
        );

        let all_day_only = FetchRequest {
            instant_range: InstantRange {
                start: "1970-01-01T00:00:00Z".parse().unwrap(),
                end: "1970-01-01T00:00:01Z".parse().unwrap(),
            },
            all_day_range: Some(CalendarDateRange {
                start_date: NaiveDate::from_ymd_opt(2026, 3, 28).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&all_day_only).unwrap()["allDayRange"],
            serde_json::json!({ "startDate": "2026-03-28", "endDateExclusive": "2026-03-31" })
        );
    }

    #[test]
    fn rename_calendar_request_uses_stable_camel_case_wire_fields() {
        let request = RenameCalendarRequest {
            calendar_id: "calendar-work".into(),
            title: "Work Renamed".into(),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "calendarId": "calendar-work",
                "title": "Work Renamed",
            })
        );
    }

    #[test]
    fn set_calendar_color_request_uses_stable_camel_case_wire_fields() {
        let request = SetCalendarColorRequest {
            calendar_id: "calendar-work".into(),
            color: "#336699".into(),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "calendarId": "calendar-work",
                "color": "#336699",
            })
        );
    }

    #[test]
    fn delete_calendar_request_uses_stable_camel_case_wire_fields() {
        let request = DeleteCalendarRequest {
            calendar_id: "calendar-delete-test".into(),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({ "calendarId": "calendar-delete-test" })
        );
    }

    #[test]
    fn recurrence_mutation_scope_serializes_and_maps_to_event_span() {
        assert_eq!(
            serde_json::to_value(RecurrenceMutationScope::ThisEvent).unwrap(),
            serde_json::json!("thisEvent")
        );
        assert_eq!(
            serde_json::to_value(RecurrenceMutationScope::FutureEvents).unwrap(),
            serde_json::json!("futureEvents")
        );
        assert_eq!(
            EventSpan::from(RecurrenceMutationScope::ThisEvent),
            EventSpan::ThisEvent
        );
        assert_eq!(
            EventSpan::from(RecurrenceMutationScope::FutureEvents),
            EventSpan::FutureEvents
        );
        assert_eq!(
            RecurrenceMutationScope::from_wire("entireSeries"),
            Err(EventMutationValidationError::UnsupportedRecurrenceScope)
        );
    }

    #[test]
    fn mutation_requests_require_scope_only_for_recurring_events() {
        let ordinary = UpdateEventRequest {
            event: EventDraft::new("work".into(), NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()),
            alarm_mutation: AlarmMutation::Preserve,
            time_mutation: EventTimeMutation::ReplaceLegacy,
            recurrence_scope: None,
        };
        assert_eq!(ordinary.validate(), Ok(()));

        let mut recurring = ordinary.clone();
        recurring.event.recurrence = vec![RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 1,
            days_of_week: vec!["MO".into()],
            occurrence_count: None,
            end_date: None,
        }];
        assert_eq!(
            recurring.validate(),
            Err(EventMutationValidationError::RecurrenceScopeRequired)
        );
        recurring.recurrence_scope = Some(RecurrenceMutationScope::FutureEvents);
        assert_eq!(recurring.validate(), Ok(()));

        let delete = DeleteEventRequest {
            event_id: "event-1".into(),
            recurrence_scope: None,
        };
        assert_eq!(delete.validate(false), Ok(()));
        assert_eq!(
            delete.validate(true),
            Err(EventMutationValidationError::RecurrenceScopeRequired)
        );
    }

    #[test]
    fn timed_event_time_input_round_trips() {
        let time = EventTimeInput::timed(
            "2026-09-10T09:00:00Z".parse().unwrap(),
            "2026-09-10T10:00:00Z".parse().unwrap(),
        )
        .unwrap();
        let encoded = serde_json::to_value(&time).unwrap();
        assert_eq!(encoded["kind"], "timed");
        assert_eq!(
            serde_json::from_value::<EventTimeInput>(encoded).unwrap(),
            time
        );
        assert!(time.as_all_day_range().is_none());
        assert!(time.as_timed_range().is_some());
    }

    #[test]
    fn all_day_event_time_input_round_trips_without_utc_instants() {
        let time = EventTimeInput::all_day(
            NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
        )
        .unwrap();
        let encoded = serde_json::to_value(&time).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "allDay",
                "startDate": "2026-09-10",
                "endDateExclusive": "2026-09-13",
            })
        );
        assert!(!encoded.to_string().contains("T00:00:00"));
        assert_eq!(
            serde_json::from_value::<EventTimeInput>(encoded).unwrap(),
            time
        );
        assert!(time.is_all_day());
    }

    #[test]
    fn event_time_input_rejects_empty_or_reversed_ranges() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert!(EventTimeInput::all_day(date, date).is_err());
        assert!(EventTimeInput::all_day(date + Duration::days(1), date).is_err());
        let instant: DateTime<Utc> = "2026-09-10T09:00:00Z".parse().unwrap();
        assert!(EventTimeInput::timed(instant, instant).is_err());
    }

    #[test]
    fn old_all_day_draft_decodes_as_explicit_legacy_unknown_time() {
        let draft: EventDraft = serde_json::from_value(serde_json::json!({
            "calendarId": "work",
            "title": "Old all-day draft",
            "start": "2026-09-10T00:00:00Z",
            "end": "2026-09-11T00:00:00Z",
            "allDay": true,
        }))
        .unwrap();
        assert!(matches!(
            draft.time,
            EventTimeInput::LegacyAllDayUnknown { .. }
        ));
        assert!(draft.time.as_all_day_range().is_none());
    }
}
