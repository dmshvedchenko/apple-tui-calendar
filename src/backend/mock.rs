use super::{BackendError, BackendState, CalendarBackend};
use crate::model::*;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Utc};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Clone)]
pub struct MockBackend {
    store: Arc<Mutex<Store>>,
    changes: broadcast::Sender<()>,
    states: broadcast::Sender<BackendState>,
}

struct Store {
    calendars: Vec<CalendarInfo>,
    events: Vec<Event>,
    last_created_time: Option<EventTimeInput>,
    last_updated_time: Option<EventTimeInput>,
    last_update_time_mutation: Option<EventTimeMutation>,
    last_update_span: Option<EventSpan>,
    last_delete_span: Option<EventSpan>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::seeded()
    }
}

impl MockBackend {
    pub fn seeded() -> Self {
        let calendars = vec![
            CalendarInfo {
                id: "work".into(),
                source_id: "mock-work".into(),
                permissions: CalendarPermissions {
                    can_create_events: true,
                    can_modify_events: true,
                    can_modify_metadata: true,
                    can_delete: true,
                },
                title: "Work".into(),
                account: "Google Workspace".into(),
                provider: "CalDAV".into(),
                color: "#4C8DFF".into(),
                is_writable: true,
                enabled: true,
            },
            CalendarInfo {
                id: "personal".into(),
                source_id: "mock-personal".into(),
                permissions: CalendarPermissions {
                    can_create_events: true,
                    can_modify_events: true,
                    can_modify_metadata: true,
                    can_delete: true,
                },
                title: "Personal".into(),
                account: "iCloud".into(),
                provider: "iCloud".into(),
                color: "#35C759".into(),
                is_writable: true,
                enabled: true,
            },
            CalendarInfo {
                id: "holidays".into(),
                source_id: "mock-subscribed".into(),
                permissions: CalendarPermissions::default(),
                title: "Holidays".into(),
                account: "Subscriptions".into(),
                provider: "Subscribed".into(),
                color: "#FF3B30".into(),
                is_writable: false,
                enabled: true,
            },
            CalendarInfo {
                id: "calendar-delete-test".into(),
                source_id: "mock-local".into(),
                permissions: CalendarPermissions {
                    can_create_events: true,
                    can_modify_events: true,
                    can_modify_metadata: true,
                    can_delete: true,
                },
                title: "Delete Test".into(),
                account: "On My Mac".into(),
                provider: "Local".into(),
                color: "#8E8E93".into(),
                is_writable: true,
                enabled: true,
            },
            CalendarInfo {
                id: "shared".into(),
                source_id: "mock-exchange".into(),
                permissions: CalendarPermissions {
                    can_create_events: true,
                    can_modify_events: true,
                    can_modify_metadata: false,
                    can_delete: false,
                },
                title: "Shared Calendar".into(),
                account: "Team Exchange".into(),
                provider: "Exchange".into(),
                color: "#AF52DE".into(),
                is_writable: true,
                enabled: true,
            },
        ];
        let today = Local::now().date_naive();
        let make = |id: &str, calendar: &str, title: &str, day: i64, hour: u32, duration: i64| {
            let date = today + Duration::days(day);
            let start = Local
                .from_local_datetime(&date.and_time(NaiveTime::from_hms_opt(hour, 0, 0).unwrap()))
                .single()
                .unwrap()
                .with_timezone(&Utc);
            Event {
                id: id.into(),
                calendar_id: calendar.into(),
                title: title.into(),
                start,
                end: start + Duration::minutes(duration),
                all_day: false,
                all_day_start_date: None,
                all_day_end_date_exclusive: None,
                location: String::new(),
                notes: String::new(),
                url: String::new(),
                time_zone: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
                time_zone_provenance: TimeZoneProvenance::ExplicitEvent,
                availability: Availability::Busy,
                organizer: None,
                attendees: vec![],
                alarms: vec![],
                recurrence: vec![],
                has_recurrence: false,
                is_detached: false,
                invitation_status: InvitationStatus::Unknown,
            }
        };
        let mut standup = make("mock-standup", "work", "Team stand-up", 0, 9, 30);
        standup.location = "Video call".into();
        standup.alarms = vec![Alarm {
            relative_seconds: Some(-900),
            absolute_date: None,
            is_editable: true,
        }];
        standup.recurrence = vec![RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 1,
            days_of_week: vec![today.weekday().to_string()],
            occurrence_count: None,
            end_date: None,
        }];
        standup.has_recurrence = true;
        let all_day = |id: &str,
                       title: &str,
                       start: chrono::NaiveDate,
                       end_exclusive: chrono::NaiveDate,
                       trusted_dates: bool| Event {
            id: id.into(),
            calendar_id: "personal".into(),
            title: title.into(),
            start: start.and_hms_opt(0, 0, 0).unwrap().and_utc(),
            end: end_exclusive.and_hms_opt(0, 0, 0).unwrap().and_utc(),
            all_day: true,
            all_day_start_date: trusted_dates.then_some(start),
            all_day_end_date_exclusive: trusted_dates.then_some(end_exclusive),
            location: String::new(),
            notes: String::new(),
            url: String::new(),
            time_zone: "Europe/Berlin".into(),
            time_zone_provenance: TimeZoneProvenance::ExplicitEvent,
            availability: Availability::Busy,
            organizer: None,
            attendees: vec![],
            alarms: vec![],
            recurrence: vec![],
            has_recurrence: false,
            is_detached: false,
            invitation_status: InvitationStatus::Unknown,
        };
        let sep_10 = chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let mut shifted_all_day = all_day(
            "mock-all-day-shifted",
            "Timezone-shifted all-day fixture",
            sep_10,
            sep_10 + Duration::days(1),
            true,
        );
        // Deliberately incompatible legacy instants prove that trusted
        // calendar-date identity, rather than start/end timestamps, decides
        // all-day fetch and view membership.
        shifted_all_day.start = (sep_10 - Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        shifted_all_day.end = sep_10.and_hms_opt(0, 0, 0).unwrap().and_utc();
        let events = vec![
            standup,
            make("mock-review", "work", "Architecture review", 0, 13, 60),
            make("mock-gym", "personal", "Gym", 0, 18, 90),
            make("mock-planning", "work", "Project planning", 1, 10, 60),
            make("mock-dinner", "personal", "Dinner with friends", 2, 19, 120),
            all_day(
                "mock-all-day-one",
                "All-day fixture",
                sep_10,
                sep_10 + Duration::days(1),
                true,
            ),
            all_day(
                "mock-all-day-multi",
                "Multi-day fixture",
                sep_10,
                sep_10 + Duration::days(3),
                true,
            ),
            all_day(
                "mock-all-day-legacy",
                "Legacy all-day fixture",
                sep_10 + Duration::days(5),
                sep_10 + Duration::days(6),
                false,
            ),
            shifted_all_day,
        ];
        let (changes, _) = broadcast::channel(32);
        let (states, _) = broadcast::channel(1);
        let _ = states.send(BackendState::Connected);
        Self {
            store: Arc::new(Mutex::new(Store {
                calendars,
                events,
                last_created_time: None,
                last_updated_time: None,
                last_update_time_mutation: None,
                last_update_span: None,
                last_delete_span: None,
            })),
            changes,
            states,
        }
    }

    pub fn empty() -> Self {
        let (changes, _) = broadcast::channel(32);
        let (states, _) = broadcast::channel(1);
        let _ = states.send(BackendState::Connected);
        Self {
            store: Arc::new(Mutex::new(Store {
                calendars: vec![],
                events: vec![],
                last_created_time: None,
                last_updated_time: None,
                last_update_time_mutation: None,
                last_update_span: None,
                last_delete_span: None,
            })),
            changes,
            states,
        }
    }

    pub fn last_update_span(&self) -> Option<EventSpan> {
        self.store.lock().unwrap().last_update_span
    }

    pub fn last_delete_span(&self) -> Option<EventSpan> {
        self.store.lock().unwrap().last_delete_span
    }

    #[cfg(test)]
    pub fn last_created_time(&self) -> Option<EventTimeInput> {
        self.store.lock().unwrap().last_created_time.clone()
    }

    #[cfg(test)]
    pub fn last_updated_time(&self) -> Option<EventTimeInput> {
        self.store.lock().unwrap().last_updated_time.clone()
    }

    #[cfg(test)]
    pub fn last_update_time_mutation(&self) -> Option<EventTimeMutation> {
        self.store.lock().unwrap().last_update_time_mutation.clone()
    }

    #[cfg(test)]
    pub fn set_events_for_test(&self, events: Vec<Event>) {
        self.store.lock().unwrap().events = events;
    }
}

#[async_trait]
impl CalendarBackend for MockBackend {
    async fn authorization_status(&self) -> Result<AuthorizationStatus, BackendError> {
        Ok(AuthorizationStatus::FullAccess)
    }
    async fn request_access(&self) -> Result<AuthorizationStatus, BackendError> {
        Ok(AuthorizationStatus::FullAccess)
    }

    async fn calendars(&self) -> Result<Vec<CalendarInfo>, BackendError> {
        Ok(self.store.lock().unwrap().calendars.clone())
    }
    async fn calendar_capabilities(&self) -> Result<CalendarCapabilities, BackendError> {
        Ok(CalendarCapabilities {
            can_list_sources: true,
            can_create: true,
            can_update: true,
            can_delete: true,
            can_change_color: true,
        })
    }
    async fn calendar_sources(&self) -> Result<Vec<CalendarSource>, BackendError> {
        Ok(vec![
            CalendarSource {
                id: "mock-work".into(),
                title: "Google Workspace".into(),
                source_type: "caldav".into(),
                is_writable: true,
            },
            CalendarSource {
                id: "mock-personal".into(),
                title: "iCloud".into(),
                source_type: "icloud".into(),
                is_writable: true,
            },
            CalendarSource {
                id: "mock-subscribed".into(),
                title: "Subscriptions".into(),
                source_type: "subscribed".into(),
                is_writable: false,
            },
            CalendarSource {
                id: "mock-local".into(),
                title: "On My Mac".into(),
                source_type: "local".into(),
                is_writable: true,
            },
            CalendarSource {
                id: "mock-exchange".into(),
                title: "Team Exchange".into(),
                source_type: "exchange".into(),
                is_writable: true,
            },
            CalendarSource {
                id: "mock-denied".into(),
                title: "Restricted Account".into(),
                source_type: "other".into(),
                is_writable: false,
            },
        ])
    }
    async fn create_calendar(
        &self,
        request: CreateCalendarRequest,
    ) -> Result<CreateCalendarResponse, BackendError> {
        if request.title.trim().is_empty() {
            return Err(BackendError::Calendar(CalendarError::InvalidTitle));
        }
        if !request.color.starts_with('#')
            || request.color.len() != 7
            || u32::from_str_radix(&request.color[1..], 16).is_err()
        {
            return Err(BackendError::Calendar(CalendarError::InvalidColor));
        }
        if request.source_id == "mock-denied" {
            return Err(BackendError::Calendar(CalendarError::PermissionDenied));
        }
        if request.source_id != "mock-work" && request.source_id != "mock-personal" {
            return Err(BackendError::Calendar(CalendarError::SourceNotFound));
        }
        let calendar = CalendarInfo {
            id: "calendar-test-created-1".into(),
            source_id: request.source_id,
            title: request.title,
            account: "Mock".into(),
            provider: "Mock".into(),
            color: request.color,
            is_writable: true,
            permissions: CalendarPermissions {
                can_create_events: true,
                can_modify_events: true,
                can_modify_metadata: true,
                can_delete: true,
            },
            enabled: true,
        };
        let mut store = self.store.lock().unwrap();
        store.calendars.retain(|item| item.id != calendar.id);
        store.calendars.push(calendar.clone());
        drop(store);
        let _ = self.changes.send(());
        Ok(calendar)
    }
    async fn rename_calendar(
        &self,
        request: RenameCalendarRequest,
    ) -> Result<RenameCalendarResponse, BackendError> {
        if request.title.trim().is_empty() {
            return Err(BackendError::Calendar(CalendarError::InvalidTitle));
        }
        let mut store = self.store.lock().unwrap();
        let calendar = store
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == request.calendar_id)
            .ok_or(BackendError::Calendar(CalendarError::NotFound))?;
        if !calendar.permissions.can_modify_metadata {
            return Err(BackendError::Calendar(CalendarError::CannotModifyMetadata));
        }
        calendar.title = request.title.trim().to_owned();
        let renamed = calendar.clone();
        let _ = self.changes.send(());
        Ok(renamed)
    }
    async fn set_calendar_color(
        &self,
        request: SetCalendarColorRequest,
    ) -> Result<SetCalendarColorResponse, BackendError> {
        if !request.color.starts_with('#')
            || request.color.len() != 7
            || u32::from_str_radix(&request.color[1..], 16).is_err()
        {
            return Err(BackendError::Calendar(CalendarError::InvalidColor));
        }
        let mut store = self.store.lock().unwrap();
        let calendar = store
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == request.calendar_id)
            .ok_or(BackendError::Calendar(CalendarError::NotFound))?;
        if !calendar.permissions.can_modify_metadata {
            return Err(BackendError::Calendar(CalendarError::CannotModifyMetadata));
        }
        calendar.color = request.color;
        let updated = calendar.clone();
        let _ = self.changes.send(());
        Ok(updated)
    }
    async fn delete_calendar(
        &self,
        request: DeleteCalendarRequest,
    ) -> Result<DeleteCalendarResponse, BackendError> {
        let mut store = self.store.lock().unwrap();
        let index = store
            .calendars
            .iter()
            .position(|calendar| calendar.id == request.calendar_id)
            .ok_or(BackendError::Calendar(CalendarError::NotFound))?;
        if !store.calendars[index].permissions.can_delete {
            return Err(BackendError::Calendar(CalendarError::CannotDelete));
        }
        let deleted = store.calendars.remove(index);
        let _ = self.changes.send(());
        Ok(DeleteCalendarResponse {
            calendar_id: deleted.id,
        })
    }

    async fn events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        ids: &[String],
    ) -> Result<Vec<Event>, BackendError> {
        Ok(self
            .store
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| {
                let intersects =
                    event
                        .all_day_date_range()
                        .is_some_and(|(event_start, event_end)| {
                            // The mock receives UTC transport boundaries but models
                            // provider-normalized all-day identity as date ranges.
                            // It deliberately avoids `Local` and compatibility
                            // instants for trusted all-day fixtures.
                            event_start < end.date_naive() && event_end > start.date_naive()
                        })
                        || (event.start < end && event.end > start);
                intersects && (ids.is_empty() || ids.contains(&event.calendar_id))
            })
            .cloned()
            .collect())
    }

    async fn create_event(&self, draft: EventDraft) -> Result<Event, BackendError> {
        if draft.title.trim().is_empty() {
            return Err(BackendError::Invalid("title is required".into()));
        }
        if matches!(&draft.time, EventTimeInput::LegacyAllDayUnknown { .. }) {
            return Err(BackendError::Invalid(
                "legacy all-day drafts cannot be created without floating date identity".into(),
            ));
        }
        let mut store = self.store.lock().unwrap();
        if !store
            .calendars
            .iter()
            .any(|c| c.id == draft.calendar_id && c.is_writable)
        {
            return Err(BackendError::Invalid("select a writable calendar".into()));
        }
        let time = draft.time.clone();
        let event = draft_to_event(draft, Uuid::new_v4().to_string());
        store.last_created_time = Some(time);
        store.events.push(event.clone());
        let _ = self.changes.send(());
        Ok(event)
    }

    async fn update_event(
        &self,
        draft: EventDraft,
        span: EventSpan,
        alarms: AlarmMutation,
        time_mutation: EventTimeMutation,
    ) -> Result<Event, BackendError> {
        let id = draft
            .id
            .clone()
            .ok_or_else(|| BackendError::Invalid("event id is required".into()))?;
        let mut store = self.store.lock().unwrap();
        store.last_update_span = Some(span);
        store.last_updated_time = Some(draft.time.clone());
        store.last_update_time_mutation = Some(time_mutation.clone());
        let slot = store
            .events
            .iter_mut()
            .find(|event| event.id == id)
            .ok_or_else(|| BackendError::NotFound(id.clone()))?;
        let mut updated = draft_to_event(draft, id);
        match time_mutation {
            EventTimeMutation::Preserve => {
                updated.start = slot.start;
                updated.end = slot.end;
                updated.all_day = slot.all_day;
                updated.time_zone = slot.time_zone.clone();
                updated.time_zone_provenance = slot.time_zone_provenance;
                updated.all_day_start_date = slot.all_day_start_date;
                updated.all_day_end_date_exclusive = slot.all_day_end_date_exclusive;
            }
            EventTimeMutation::ReplaceAllDay {
                start_date,
                end_date_exclusive,
            } => {
                updated.all_day = true;
                updated.all_day_start_date = Some(start_date);
                updated.all_day_end_date_exclusive = Some(end_date_exclusive);
            }
            EventTimeMutation::ReplaceLegacy => {}
        }
        if matches!(alarms, AlarmMutation::Preserve) {
            updated.alarms = slot.alarms.clone();
        }
        *slot = updated.clone();
        let _ = self.changes.send(());
        Ok(updated)
    }

    async fn delete_event(&self, id: &str, span: EventSpan) -> Result<(), BackendError> {
        let mut store = self.store.lock().unwrap();
        store.last_delete_span = Some(span);
        let before = store.events.len();
        store.events.retain(|event| event.id != id);
        if before == store.events.len() {
            return Err(BackendError::NotFound(id.into()));
        }
        let _ = self.changes.send(());
        Ok(())
    }

    async fn respond_to_invitation(
        &self,
        id: &str,
        response: InvitationResponse,
    ) -> Result<(), BackendError> {
        let mut store = self.store.lock().unwrap();
        let event = store
            .events
            .iter_mut()
            .find(|event| event.id == id)
            .ok_or_else(|| BackendError::NotFound(id.into()))?;
        event.invitation_status = match response {
            InvitationResponse::Accept => InvitationStatus::Accepted,
            InvitationResponse::Decline => InvitationStatus::Declined,
            InvitationResponse::Maybe => InvitationStatus::Tentative,
        };
        let _ = self.changes.send(());
        Ok(())
    }

    fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }
    fn subscribe_backend_state(&self) -> broadcast::Receiver<BackendState> {
        self.states.subscribe()
    }
}

fn draft_to_event(draft: EventDraft, id: String) -> Event {
    let (start, end, all_day, all_day_start_date, all_day_end_date_exclusive) = match draft.time {
        EventTimeInput::Timed { start, end } => (start, end, false, None, None),
        EventTimeInput::AllDay {
            start_date,
            end_date_exclusive,
        } => {
            // The mock retains compatibility instants for range plumbing but
            // carries the trusted floating-date range as the authority.
            let start = start_date.and_hms_opt(0, 0, 0).unwrap().and_utc();
            let end = end_date_exclusive.and_hms_opt(0, 0, 0).unwrap().and_utc();
            (start, end, true, Some(start_date), Some(end_date_exclusive))
        }
        EventTimeInput::LegacyAllDayUnknown { start, end } => (start, end, true, None, None),
    };
    Event {
        id,
        calendar_id: draft.calendar_id,
        title: draft.title,
        start,
        end,
        all_day,
        all_day_start_date,
        all_day_end_date_exclusive,
        location: draft.location,
        notes: draft.notes,
        url: draft.url,
        time_zone: draft.time_zone,
        time_zone_provenance: TimeZoneProvenance::ExplicitEvent,
        availability: draft.availability,
        organizer: None,
        attendees: draft
            .attendees
            .into_iter()
            .map(|email| Attendee {
                name: String::new(),
                email,
                status: InvitationStatus::Pending,
                is_current_user: false,
                role: "required".into(),
                participant_type: "person".into(),
                schedule_status: "pending".into(),
            })
            .collect(),
        alarms: draft.alarms,
        has_recurrence: !draft.recurrence.is_empty(),
        recurrence: draft.recurrence,
        is_detached: false,
        invitation_status: InvitationStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn supports_crud_without_eventkit() {
        let backend = MockBackend::seeded();
        let mut draft = EventDraft::new("work".into(), Local::now().date_naive());
        draft.title = "Test event".into();
        let created = backend.create_event(draft).await.unwrap();
        assert!(
            backend
                .events(
                    created.start - Duration::hours(1),
                    created.end + Duration::hours(1),
                    &[]
                )
                .await
                .unwrap()
                .iter()
                .any(|e| e.id == created.id)
        );
        backend
            .delete_event(&created.id, EventSpan::ThisEvent)
            .await
            .unwrap();
        assert_eq!(backend.last_delete_span(), Some(EventSpan::ThisEvent));
    }

    #[tokio::test]
    async fn creates_all_day_events_from_floating_dates_without_utc_input() {
        let backend = MockBackend::seeded();
        let mut draft = EventDraft::new(
            "work".into(),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        );
        draft.title = "All-day test".into();
        draft.time = EventTimeInput::all_day(
            chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
        )
        .unwrap();
        let created = backend.create_event(draft).await.unwrap();
        assert_eq!(
            backend.last_created_time(),
            Some(EventTimeInput::AllDay {
                start_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            })
        );
        assert_eq!(
            created.all_day_date_range(),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            ))
        );
    }

    #[tokio::test]
    async fn preserves_future_event_span_for_recurring_mutation_requests() {
        let backend = MockBackend::seeded();
        let mut draft = EventDraft::new("work".into(), Local::now().date_naive());
        draft.title = "Recurring test".into();
        let created = backend.create_event(draft).await.unwrap();
        let mut update = EventDraft::new("work".into(), Local::now().date_naive());
        update.id = Some(created.id);
        update.title = "Recurring test updated".into();
        backend
            .update_event(
                update,
                EventSpan::FutureEvents,
                AlarmMutation::Replace(vec![]),
                EventTimeMutation::ReplaceLegacy,
            )
            .await
            .unwrap();
        assert_eq!(backend.last_update_span(), Some(EventSpan::FutureEvents));
    }

    #[tokio::test]
    async fn recurring_all_day_mutation_scopes_preserve_trusted_date_identity() {
        let backend = MockBackend::seeded();
        let start = chrono::NaiveDate::from_ymd_opt(2026, 3, 28).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let mut draft = EventDraft::new("personal".into(), start);
        draft.title = "Recurring all-day".into();
        draft.time = EventTimeInput::all_day(start, end).unwrap();
        draft.recurrence = vec![RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 1,
            days_of_week: vec![],
            occurrence_count: None,
            end_date: None,
        }];
        let created = backend.create_event(draft).await.unwrap();

        for span in [EventSpan::ThisEvent, EventSpan::FutureEvents] {
            let mut update = EventDraft::new(created.calendar_id.clone(), start);
            update.id = Some(created.id.clone());
            update.title = format!("Recurring all-day {span:?}");
            update.recurrence = created.recurrence.clone();
            let updated = backend
                .update_event(
                    update,
                    span,
                    AlarmMutation::Preserve,
                    EventTimeMutation::Preserve,
                )
                .await
                .unwrap();
            assert_eq!(updated.all_day_date_range(), Some((start, end)));
            assert!(updated.has_recurrence);
            assert_eq!(backend.last_update_span(), Some(span));
        }
    }

    #[tokio::test]
    async fn preserves_existing_alarms_when_an_update_requests_preservation() {
        let backend = MockBackend::seeded();
        let event = backend
            .events(
                Utc::now() - Duration::days(1),
                Utc::now() + Duration::days(1),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.id == "mock-standup")
            .unwrap();
        let mut draft = EventDraft::new(event.calendar_id.clone(), Local::now().date_naive());
        draft.id = Some(event.id.clone());
        draft.title = "Renamed stand-up".into();
        draft.time = EventTimeInput::timed(event.start, event.end).unwrap();
        draft.alarms = vec![];
        let updated = backend
            .update_event(
                draft,
                EventSpan::ThisEvent,
                AlarmMutation::Preserve,
                EventTimeMutation::ReplaceLegacy,
            )
            .await
            .unwrap();
        assert_eq!(updated.alarms, event.alarms);
    }

    #[tokio::test]
    async fn renames_writable_calendar_with_advertised_capability() {
        let backend = MockBackend::seeded();
        let capabilities = backend.calendar_capabilities().await.unwrap();
        assert!(capabilities.can_create);
        assert!(capabilities.can_update);
        assert!(capabilities.can_change_color);
        assert!(capabilities.can_delete);
        let renamed = backend
            .rename_calendar(RenameCalendarRequest {
                calendar_id: "work".into(),
                title: "Work Renamed".into(),
            })
            .await
            .unwrap();
        assert_eq!(renamed.title, "Work Renamed");
        assert!(matches!(
            backend
                .rename_calendar(RenameCalendarRequest {
                    calendar_id: "holidays".into(),
                    title: "Nope".into(),
                })
                .await,
            Err(BackendError::Calendar(CalendarError::CannotModifyMetadata))
        ));
    }

    #[tokio::test]
    async fn changes_writable_calendar_color_deterministically() {
        let backend = MockBackend::seeded();
        let updated = backend
            .set_calendar_color(SetCalendarColorRequest {
                calendar_id: "work".into(),
                color: "#336699".into(),
            })
            .await
            .unwrap();
        assert_eq!(updated.color, "#336699");
        assert!(matches!(
            backend
                .set_calendar_color(SetCalendarColorRequest {
                    calendar_id: "holidays".into(),
                    color: "#336699".into(),
                })
                .await,
            Err(BackendError::Calendar(CalendarError::CannotModifyMetadata))
        ));
        assert!(matches!(
            backend
                .set_calendar_color(SetCalendarColorRequest {
                    calendar_id: "work".into(),
                    color: "blue".into(),
                })
                .await,
            Err(BackendError::Calendar(CalendarError::InvalidColor))
        ));
    }

    #[tokio::test]
    async fn deletes_only_explicitly_deletable_calendars() {
        let backend = MockBackend::seeded();
        let deleted = backend
            .delete_calendar(DeleteCalendarRequest {
                calendar_id: "calendar-delete-test".into(),
            })
            .await
            .unwrap();
        assert_eq!(deleted.calendar_id, "calendar-delete-test");
        assert!(
            !backend
                .calendars()
                .await
                .unwrap()
                .iter()
                .any(|calendar| calendar.id == "calendar-delete-test")
        );
        assert!(matches!(
            backend
                .delete_calendar(DeleteCalendarRequest {
                    calendar_id: "holidays".into(),
                })
                .await,
            Err(BackendError::Calendar(CalendarError::CannotDelete))
        ));
        assert!(matches!(
            backend
                .delete_calendar(DeleteCalendarRequest {
                    calendar_id: "missing".into(),
                })
                .await,
            Err(BackendError::Calendar(CalendarError::NotFound))
        ));
    }

    #[tokio::test]
    async fn creates_calendar_and_exposes_it_in_subsequent_metadata() {
        let backend = MockBackend::seeded();
        let created = backend
            .create_calendar(CreateCalendarRequest {
                title: "Created in Mock".into(),
                color: "#336699".into(),
                source_id: "mock-work".into(),
            })
            .await
            .unwrap();
        assert_eq!(created.id, "calendar-test-created-1");
        assert!(
            backend
                .calendars()
                .await
                .unwrap()
                .iter()
                .any(|calendar| calendar.id == created.id)
        );
        assert!(matches!(
            backend
                .create_calendar(CreateCalendarRequest {
                    title: "Denied".into(),
                    color: "#336699".into(),
                    source_id: "mock-denied".into(),
                })
                .await,
            Err(BackendError::Calendar(CalendarError::PermissionDenied))
        ));
    }

    #[tokio::test]
    async fn exposes_trusted_and_legacy_all_day_fixture_metadata() {
        let backend = MockBackend::seeded();
        let events = backend
            .events(
                "2025-01-01T00:00:00Z".parse().unwrap(),
                "2027-01-01T00:00:00Z".parse().unwrap(),
                &[],
            )
            .await
            .unwrap();
        let one_day = events
            .iter()
            .find(|event| event.id == "mock-all-day-one")
            .unwrap();
        assert_eq!(
            one_day.all_day_date_range(),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            ))
        );
        let multi_day = events
            .iter()
            .find(|event| event.id == "mock-all-day-multi")
            .unwrap();
        assert_eq!(
            multi_day.all_day_date_range(),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            ))
        );
        assert_eq!(
            events
                .iter()
                .find(|event| event.id == "mock-all-day-legacy")
                .unwrap()
                .all_day_date_range(),
            None
        );
        assert!(events.iter().any(|event| {
            !event.all_day
                && event.all_day_start_date.is_none()
                && event.all_day_end_date_exclusive.is_none()
        }));
    }
}
