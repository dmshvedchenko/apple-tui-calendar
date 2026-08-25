use crate::{
    backend::{BackendError, BackendState, CalendarBackend},
    cache::Cache,
    config::Config,
    hit_test::{CalendarHitGeometry, CalendarHitTarget},
    input::{PointerAction, PointerEvent},
    model::*,
    quick_add::{self, QuickAddContext, QuickAddParseResult, QuickAddStatus},
    range::{RangeLoader, RangePriority, RangeReason, RangeRequest},
};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, Months, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;
use std::{fmt, sync::Arc, time::Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Day,
    Week,
    Month,
    Agenda,
}

/// Who currently owns the Day/Week timeline position.  Automatic positioning
/// is deliberately opt-in at navigation boundaries; a backend redraw must
/// never take the viewport away from someone who has scrolled it manually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineViewportOwner {
    #[default]
    Auto,
    Manual,
}

impl View {
    pub fn from_config(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "day" => Self::Day,
            "month" => Self::Month,
            "agenda" => Self::Agenda,
            _ => Self::Week,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "Day",
            Self::Week => "Week",
            Self::Month => "Month",
            Self::Agenda => "Agenda",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Calendars,
    CalendarManager,
    CalendarManagerDetails,
    CalendarCreate,
    CalendarRename,
    CalendarColor,
    CalendarDeleteConfirm,
    QuickAdd,
    Details,
    Search,
    Palette,
    DateJump,
    Form,
    DiscardConfirm,
    Delete,
    RecurringEditScope,
    RecurringDeleteScope,
    Help,
}

/// One UI overlay and the state to restore when that overlay is dismissed.
/// Frames contain only presentation state: event, cache, and backend state
/// remain on `App` and are never recreated while navigating modals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalFrame {
    pub current: Mode,
    pub return_to: Mode,
}

/// A short-lived, stable-ID snapshot of the current interaction context.
/// `selected_event` remains the sole stored UI selection index; this context
/// exists only while moving between views or replacing a snapshot, where an
/// index cannot safely survive a different ordering or visibility window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionContext {
    active_date: NaiveDate,
    selected_event_id: Option<String>,
    selected_event_date: Option<NaiveDate>,
}

/// The stable occurrence that owns a short-lived Details/editor workflow.
///
/// `selected_event` is intentionally still the application's only stored
/// selection index. This anchor is needed only while a modal is open: a
/// snapshot can reorder visible rows, and recurring EventKit mutations can
/// replace the concrete occurrence identity. Provider identity remains here
/// solely as canonical mutation lookup data; it is never used as UI focus.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractionContext {
    source_view: View,
    source_active_date: NaiveDate,
    occurrence_id: String,
    occurrence_local_date: NaiveDate,
    provider_id: Option<String>,
    calendar_id: String,
    canonical_occurrence_start: DateTime<Utc>,
}

/// Provider-neutral intent produced by keyboard controls, the command palette,
/// and (eventually) pointer-driven UI. The dispatcher below is the only UI
/// entry point that starts an event workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAction {
    CreateEvent,
    QuickAdd,
    EditEvent(String),
    DuplicateEvent(String),
    DeleteEvent(String),
    MoveEvent {
        event_id: String,
        target: EventMoveTarget,
    },
    Search,
    OpenDateJump,
    GoToDate(NaiveDate),
    Today,
    ChangeView(View),
    Refresh,
    RetryVisibleRange,
    ToggleSidebar,
    /// Internal history actions intentionally have no key binding yet. They
    /// reuse the ordinary worker commands after a provider-confirmed record.
    Undo,
    Redo,
}

/// A move target retains the event's temporal identity. Timed values are
/// instants; all-day values are floating calendar dates with an exclusive end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventMoveTarget {
    Timed {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    AllDay {
        start_date: NaiveDate,
        end_date_exclusive: NaiveDate,
    },
}

/// Lifecycle of a pointer-independent drag intent. A future pointer adapter
/// owns gesture recognition; this state only records what is being previewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DragState {
    #[default]
    Idle,
    Pressed,
    Dragging,
    Preview,
    Dropped,
}

/// UI-only state for a prospective event move. It never contacts a backend or
/// changes an event. A completed session yields `UserAction::MoveEvent` for
/// the normal dispatcher to validate and execute later.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DragSession {
    pub event_id: Option<String>,
    pub origin: Option<CalendarHitTarget>,
    pub current_target: Option<CalendarHitTarget>,
    pub state: DragState,
}

/// Presentation data derived from a valid drag preview. Timed previews retain
/// local-wall-clock display information; all-day previews retain only their
/// floating calendar-date ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragPreview {
    Timed {
        event_id: String,
        title: String,
        original_start: DateTime<Utc>,
        original_end: DateTime<Utc>,
        proposed_start: DateTime<Utc>,
        proposed_end: DateTime<Utc>,
        current_target: CalendarHitTarget,
    },
    AllDay {
        event_id: String,
        title: String,
        original_start_date: NaiveDate,
        original_end_date_exclusive: NaiveDate,
        proposed_start_date: NaiveDate,
        proposed_end_date_exclusive: NaiveDate,
        current_target: CalendarHitTarget,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragSessionError {
    NoActiveSession,
    InvalidOrigin,
    StaleEvent,
    ReadOnlyCalendar,
    LegacyAllDay,
    InvalidTarget,
    InvalidLocalTime,
    OutOfRange,
}

impl DragSessionError {
    fn message(self) -> &'static str {
        match self {
            Self::NoActiveSession => "No active drag session",
            Self::InvalidOrigin => "Drag must start on the selected event",
            Self::StaleEvent => "Event no longer exists",
            Self::ReadOnlyCalendar => "This calendar is read-only",
            Self::LegacyAllDay => "Legacy all-day events must be refreshed before moving",
            Self::InvalidTarget => "Drop target cannot move this event",
            Self::InvalidLocalTime => "Drop time is ambiguous or invalid in the local time zone",
            Self::OutOfRange => "Drop target is out of range",
        }
    }
}

impl DragSession {
    fn start(&mut self, event_id: String, origin: CalendarHitTarget) {
        self.event_id = Some(event_id);
        self.origin = Some(origin);
        self.current_target = None;
        self.state = DragState::Pressed;
    }

    fn update(&mut self, target: CalendarHitTarget, preview_is_valid: bool) {
        self.current_target = Some(target);
        self.state = if preview_is_valid {
            DragState::Preview
        } else {
            DragState::Dragging
        };
    }

    fn cancel(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceMutationAction {
    Edit,
    Delete,
    Move(EventMoveTarget),
}

#[derive(Debug)]
pub enum WorkerCommand {
    Refresh,
    RefreshRange(RangeRequest),
    SetCalendarEnabled(String, bool),
    CreateCalendar(CreateCalendarRequest),
    RenameCalendar(RenameCalendarRequest),
    SetCalendarColor(SetCalendarColorRequest),
    DeleteCalendar(DeleteCalendarRequest),
    Create(EventDraft),
    CreateWithSession {
        session: u64,
        event: EventDraft,
    },
    Update(
        EventDraft,
        EventSpan,
        Option<RecurrenceMutationScope>,
        AlarmMutation,
        EventTimeMutation,
    ),
    UpdateWithSession {
        session: u64,
        event: EventDraft,
        span: EventSpan,
        recurrence_scope: Option<RecurrenceMutationScope>,
        alarms: AlarmMutation,
        time_mutation: EventTimeMutation,
    },
    Delete(String, EventSpan, Option<RecurrenceMutationScope>),
    DeleteWithSession {
        session: u64,
        event_id: String,
        span: EventSpan,
        recurrence_scope: Option<RecurrenceMutationScope>,
    },
    OpenUrl(String),
    EnsureRange(RangeRequest),
}

#[derive(Debug)]
pub enum WorkerUpdate {
    Syncing(bool),
    BackendState(BackendState),
    Snapshot(Snapshot),
    CalendarCapabilities(CalendarCapabilities),
    CalendarSources(Vec<CalendarSource>),
    CalendarCreateSucceeded(CalendarInfo),
    CalendarCreateFailed(CalendarError),
    CalendarRenameSucceeded(CalendarInfo),
    CalendarRenameFailed(CalendarError),
    CalendarColorSucceeded(CalendarInfo),
    CalendarColorFailed(CalendarError),
    CalendarDeleteSucceeded(DeleteCalendarResponse),
    CalendarDeleteFailed(CalendarError),
    RangeStarted(u64),
    RangeLoaded(u64),
    RangeFailed(u64, String),
    MutationSaving,
    MutationSucceeded(MutationEffect),
    MutationFailed(String),
    MutationSavingFor(u64),
    MutationSucceededFor(u64, MutationEffect),
    MutationFailedFor(u64, String),
    Status(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationEffect {
    Created {
        event_id: String,
        interval: (DateTime<Utc>, DateTime<Utc>),
    },
    Updated {
        event_id: String,
        before_interval: (DateTime<Utc>, DateTime<Utc>),
        after_interval: (DateTime<Utc>, DateTime<Utc>),
        recurrence_scope: Option<RecurrenceMutationScope>,
    },
    Deleted {
        event_id: String,
        interval: (DateTime<Utc>, DateTime<Utc>),
        recurrence_scope: Option<RecurrenceMutationScope>,
    },
}

/// A provider-confirmed, safely reversible event mutation. Records are kept
/// deliberately small: recurring series operations are excluded because an
/// EventKit future-series mutation can split identities and has no atomic
/// reversal precondition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoRecord {
    Created {
        draft: EventDraft,
        created_event_id: String,
    },
    Updated {
        event_id: String,
        before: Box<ReversibleEventDraft>,
        after: Box<ReversibleEventDraft>,
    },
    Deleted {
        deleted: ReversibleEventDraft,
        restored_event_id: Option<String>,
    },
}

/// Provider-neutral data needed to replay a non-recurring event update. The
/// typed time and alarm intents ensure all-day date identity and protected
/// alarms retain their existing safety rules during history operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReversibleEventDraft {
    draft: EventDraft,
    alarm_mutation: AlarmMutation,
    time_mutation: EventTimeMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingHistory {
    Initial(UndoRecord),
    Undo(UndoRecord),
    Redo(UndoRecord),
}

/// This is intentionally a small foundation, not an unbounded event log.
const UNDO_HISTORY_LIMIT: usize = 20;

impl MutationEffect {
    fn event_id(&self) -> &str {
        match self {
            Self::Created { event_id, .. }
            | Self::Updated { event_id, .. }
            | Self::Deleted { event_id, .. } => event_id,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Created { .. } => "Event created",
            Self::Updated { .. } => "Event updated",
            Self::Deleted { .. } => "Event deleted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationState {
    Idle,
    Saving,
    Deleting,
    Failed,
}

/// State for the interval currently represented by the calendar view. This is
/// deliberately independent from global synchronization: cached events remain
/// usable while an EventKit request is in flight or has failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRangeState {
    Ready,
    Loading,
    Failed(VisibleRangeError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleRangeError {
    pub request_id: u64,
    pub range: RangeRequest,
    pub error: String,
    pub timestamp: DateTime<Utc>,
}

pub struct App {
    pub config: Config,
    pub snapshot: Snapshot,
    pub active_date: NaiveDate,
    pub view: View,
    pub mode: Mode,
    pub modal_stack: Vec<ModalFrame>,
    pub selected_event: usize,
    pub selected_calendar: usize,
    pub calendar_capabilities: CalendarCapabilities,
    pub calendar_sources: Vec<CalendarSource>,
    pub sidebar_visible: bool,
    pub timeline_start_minute: u16,
    pub timeline_viewport_owner: TimelineViewportOwner,
    pub drag_session: DragSession,
    pub search_query: String,
    pub search_selected: usize,
    pub form: Option<EventForm>,
    pub calendar_form: Option<CalendarForm>,
    pub calendar_rename_form: Option<CalendarRenameForm>,
    pub calendar_color_form: Option<CalendarColorForm>,
    pub calendar_delete_confirmation: Option<CalendarDeleteConfirmation>,
    pub quick_add_input: String,
    pub form_dirty: bool,
    pub mutation_state: MutationState,
    pub delete_span: EventSpan,
    pub delete_recurrence_scope: Option<RecurrenceMutationScope>,
    pub pending_recurring_mutation: Option<(String, RecurrenceMutationAction)>,
    /// The event chosen before opening the destructive confirmation. Keeping
    /// this stable ID avoids deleting whichever row happens to be selected
    /// after a refresh while the confirmation is open.
    pending_delete_event_id: Option<String>,
    pub palette_query: String,
    pub palette_selected: usize,
    pub detail_scroll: u16,
    pub help_scroll: u16,
    pub syncing: bool,
    pub backend_state: BackendState,
    pub should_quit: bool,
    pub status: Option<(String, bool, Instant)>,
    pub visible_range_request_id: u64,
    pub visible_range: Option<RangeRequest>,
    pub visible_range_state: VisibleRangeState,
    pending_selection: Option<String>,
    interaction_context: Option<InteractionContext>,
    next_mutation_session: u64,
    active_mutation_session: Option<u64>,
    pending_calendar_selection: Option<String>,
    pending_g: bool,
    undo_stack: Vec<UndoRecord>,
    redo_stack: Vec<UndoRecord>,
    pending_history: Option<PendingHistory>,
}

impl App {
    pub fn new(config: Config, snapshot: Snapshot) -> Self {
        let view = View::from_config(&config.default_view);
        Self {
            config,
            snapshot,
            active_date: Local::now().date_naive(),
            view,
            mode: Mode::Normal,
            modal_stack: vec![],
            selected_event: 0,
            selected_calendar: 0,
            calendar_capabilities: CalendarCapabilities::default(),
            calendar_sources: vec![],
            sidebar_visible: false,
            timeline_start_minute: 7 * 60,
            timeline_viewport_owner: TimelineViewportOwner::Auto,
            drag_session: DragSession::default(),
            search_query: String::new(),
            search_selected: 0,
            form: None,
            calendar_form: None,
            calendar_rename_form: None,
            calendar_color_form: None,
            calendar_delete_confirmation: None,
            quick_add_input: String::new(),
            form_dirty: false,
            mutation_state: MutationState::Idle,
            delete_span: EventSpan::ThisEvent,
            delete_recurrence_scope: None,
            pending_recurring_mutation: None,
            pending_delete_event_id: None,
            palette_query: String::new(),
            palette_selected: 0,
            detail_scroll: 0,
            help_scroll: 0,
            syncing: false,
            backend_state: BackendState::Starting,
            should_quit: false,
            status: None,
            visible_range_request_id: 0,
            visible_range: None,
            visible_range_state: VisibleRangeState::Ready,
            pending_selection: None,
            interaction_context: None,
            next_mutation_session: 0,
            active_mutation_session: None,
            pending_calendar_selection: None,
            pending_g: false,
            undo_stack: vec![],
            redo_stack: vec![],
            pending_history: None,
        }
    }

    /// Stages a reversible record when an event mutation enters the worker.
    /// The record is not visible to undo/redo until `MutationSucceeded`
    /// confirms the provider accepted that exact command.
    pub fn note_dispatched_mutation(&mut self, command: &WorkerCommand) {
        if self.pending_history.is_some() {
            return;
        }
        self.pending_history = self
            .undo_record_for_command(command)
            .map(PendingHistory::Initial);
    }

    fn undo_record_for_command(&self, command: &WorkerCommand) -> Option<UndoRecord> {
        match command {
            WorkerCommand::CreateWithSession { event, .. } => {
                return self.undo_record_for_command(&WorkerCommand::Create(event.clone()));
            }
            WorkerCommand::UpdateWithSession {
                event,
                span,
                recurrence_scope,
                alarms,
                time_mutation,
                ..
            } => {
                return self.undo_record_for_command(&WorkerCommand::Update(
                    event.clone(),
                    *span,
                    *recurrence_scope,
                    alarms.clone(),
                    time_mutation.clone(),
                ));
            }
            WorkerCommand::DeleteWithSession {
                event_id,
                span,
                recurrence_scope,
                ..
            } => {
                return self.undo_record_for_command(&WorkerCommand::Delete(
                    event_id.clone(),
                    *span,
                    *recurrence_scope,
                ));
            }
            _ => {}
        }
        match command {
            WorkerCommand::Create(draft) if draft.recurrence.is_empty() => {
                Some(UndoRecord::Created {
                    draft: draft.clone(),
                    created_event_id: String::new(),
                })
            }
            WorkerCommand::Update(
                draft,
                EventSpan::ThisEvent,
                None,
                alarm_mutation,
                time_mutation,
            ) => {
                let event = self
                    .snapshot
                    .events
                    .iter()
                    .find(|event| draft.id.as_deref() == Some(event.id.as_str()))?;
                if event.has_recurrence {
                    return None;
                }
                Some(UndoRecord::Updated {
                    event_id: event.id.clone(),
                    before: Box::new(Self::reversible_event_draft(
                        event,
                        Some(event.id.clone()),
                        false,
                    )?),
                    after: Box::new(ReversibleEventDraft {
                        draft: draft.clone(),
                        alarm_mutation: alarm_mutation.clone(),
                        time_mutation: time_mutation.clone(),
                    }),
                })
            }
            WorkerCommand::Delete(id, EventSpan::ThisEvent, None) => {
                let event = self.snapshot.events.iter().find(|event| event.id == *id)?;
                if event.has_recurrence
                    || AlarmEditorState::from_existing(&event.alarms).is_protected()
                {
                    return None;
                }
                Some(UndoRecord::Deleted {
                    deleted: Self::reversible_event_draft(event, None, true)?,
                    restored_event_id: None,
                })
            }
            _ => None,
        }
    }

    fn reversible_event_draft(
        event: &Event,
        id: Option<String>,
        for_create: bool,
    ) -> Option<ReversibleEventDraft> {
        let (time, time_mutation) = if event.all_day {
            let (start_date, end_date_exclusive) = event.all_day_date_range()?;
            (
                // Updates retain the existing provider compatibility payload,
                // while `ReplaceAllDay` is the authoritative floating-date
                // mutation. Restoring a deletion is a create, so it uses the
                // typed dates instead; neither path rebuilds dates from
                // compatibility instants.
                if for_create {
                    EventTimeInput::all_day(start_date, end_date_exclusive).ok()?
                } else {
                    EventTimeInput::legacy_all_day_unknown(event.start, event.end).ok()?
                },
                EventTimeMutation::ReplaceAllDay {
                    start_date,
                    end_date_exclusive,
                },
            )
        } else {
            (
                EventTimeInput::timed(event.start, event.end).ok()?,
                EventTimeMutation::ReplaceLegacy,
            )
        };
        let alarm_mutation = if AlarmEditorState::from_existing(&event.alarms).is_protected() {
            AlarmMutation::Preserve
        } else {
            AlarmMutation::Replace(event.alarms.clone())
        };
        Some(ReversibleEventDraft {
            draft: EventDraft {
                id,
                occurrence_id: Some(event.id.clone()),
                occurrence_start: Some(event.start),
                occurrence_calendar_id: Some(event.calendar_id.clone()),
                calendar_id: event.calendar_id.clone(),
                title: event.title.clone(),
                time,
                location: event.location.clone(),
                notes: event.notes.clone(),
                url: event.url.clone(),
                time_zone: event.time_zone.clone(),
                availability: event.availability,
                attendees: vec![],
                alarms: event.alarms.clone(),
                recurrence: event.recurrence.clone(),
            },
            alarm_mutation,
            time_mutation,
        })
    }

    fn history_event(&mut self, id: &str) -> Option<Event> {
        let event = self
            .snapshot
            .events
            .iter()
            .find(|event| event.id == id)
            .cloned();
        if event.is_none() {
            self.status = Some(("Event no longer exists".into(), true, Instant::now()));
        }
        event
    }

    fn history_calendar_is_writable(&mut self, calendar_id: &str) -> bool {
        let writable = self
            .calendar(calendar_id)
            .is_some_and(|calendar| calendar.is_writable);
        if !writable {
            self.status = Some(("This calendar is read-only".into(), true, Instant::now()));
        }
        writable
    }

    fn history_update_command(
        &mut self,
        event_id: &str,
        reversible: &ReversibleEventDraft,
    ) -> Option<WorkerCommand> {
        let event = self.history_event(event_id)?;
        if !self.history_calendar_is_writable(&event.calendar_id) {
            return None;
        }
        let mut draft = reversible.draft.clone();
        let provider_id = event.provider_id.clone()?;
        draft.id = Some(provider_id);
        draft.occurrence_id = Some(event.id.clone());
        draft.occurrence_start = Some(event.start);
        draft.occurrence_calendar_id = Some(event.calendar_id.clone());
        Some(WorkerCommand::Update(
            draft,
            EventSpan::ThisEvent,
            None,
            reversible.alarm_mutation.clone(),
            reversible.time_mutation.clone(),
        ))
    }

    fn execute_history(&mut self, undo: bool) -> Option<WorkerCommand> {
        if let Some(reason) = self.history_available_reason(undo) {
            self.status = Some((reason.into(), true, Instant::now()));
            return None;
        }
        let record = if undo {
            self.undo_stack.pop()?
        } else {
            self.redo_stack.pop()?
        };
        let command = match (&record, undo) {
            (
                UndoRecord::Created {
                    created_event_id, ..
                },
                true,
            ) => self.history_event(created_event_id).and_then(|event| {
                self.history_calendar_is_writable(&event.calendar_id)
                    .then_some(WorkerCommand::Delete(
                        created_event_id.clone(),
                        EventSpan::ThisEvent,
                        None,
                    ))
            }),
            (UndoRecord::Created { draft, .. }, false) => {
                if !self.history_calendar_is_writable(&draft.calendar_id) {
                    return None;
                }
                Some(WorkerCommand::Create(draft.clone()))
            }
            (
                UndoRecord::Updated {
                    event_id, before, ..
                },
                true,
            ) => self.history_update_command(event_id, before),
            (
                UndoRecord::Updated {
                    event_id, after, ..
                },
                false,
            ) => self.history_update_command(event_id, after),
            (UndoRecord::Deleted { deleted, .. }, true) => {
                if !self.history_calendar_is_writable(&deleted.draft.calendar_id) {
                    return None;
                }
                let mut draft = deleted.draft.clone();
                draft.id = None;
                Some(WorkerCommand::Create(draft))
            }
            (
                UndoRecord::Deleted {
                    restored_event_id, ..
                },
                false,
            ) => restored_event_id
                .as_deref()
                .and_then(|id| self.history_event(id))
                .and_then(|event| {
                    self.history_calendar_is_writable(&event.calendar_id)
                        .then_some(WorkerCommand::Delete(event.id, EventSpan::ThisEvent, None))
                }),
        };
        let Some(command) = command else {
            if undo {
                Self::push_history(&mut self.undo_stack, record);
            } else {
                Self::push_history(&mut self.redo_stack, record);
            }
            return None;
        };
        self.pending_history = Some(if undo {
            PendingHistory::Undo(record)
        } else {
            PendingHistory::Redo(record)
        });
        Some(command)
    }

    fn history_unavailable_reason(&self, undo: bool) -> Option<&'static str> {
        let record = if undo {
            self.undo_stack.last()
        } else {
            self.redo_stack.last()
        }?;
        let calendar_id = match (record, undo) {
            (
                UndoRecord::Created {
                    created_event_id, ..
                },
                true,
            ) => {
                let Some(event) = self
                    .snapshot
                    .events
                    .iter()
                    .find(|event| event.id == *created_event_id)
                else {
                    return Some("Provider state changed; event no longer exists");
                };
                event.calendar_id.as_str()
            }
            (UndoRecord::Created { draft, .. }, false) => draft.calendar_id.as_str(),
            (UndoRecord::Updated { event_id, .. }, _) => {
                let Some(event) = self
                    .snapshot
                    .events
                    .iter()
                    .find(|event| event.id == *event_id)
                else {
                    return Some("Provider state changed; event no longer exists");
                };
                event.calendar_id.as_str()
            }
            (UndoRecord::Deleted { deleted, .. }, true) => deleted.draft.calendar_id.as_str(),
            (
                UndoRecord::Deleted {
                    restored_event_id, ..
                },
                false,
            ) => {
                let Some(id) = restored_event_id.as_deref() else {
                    return Some("Provider state changed; event no longer exists");
                };
                let Some(event) = self.snapshot.events.iter().find(|event| event.id == id) else {
                    return Some("Provider state changed; event no longer exists");
                };
                event.calendar_id.as_str()
            }
        };
        (!self
            .calendar(calendar_id)
            .is_some_and(|calendar| calendar.is_writable))
        .then_some("This calendar is read-only")
    }

    fn history_available_reason(&self, undo: bool) -> Option<&'static str> {
        if self.pending_history.is_some() {
            return Some("A calendar change is still in progress");
        }
        let stack_empty = if undo {
            self.undo_stack.is_empty()
        } else {
            self.redo_stack.is_empty()
        };
        if stack_empty {
            return Some(if undo {
                "Nothing to undo"
            } else {
                "Nothing to redo"
            });
        }
        self.history_unavailable_reason(undo)
    }

    fn complete_history(&mut self, effect: &MutationEffect) {
        let Some(pending) = self.pending_history.take() else {
            return;
        };
        let (mut record, push_to_undo, clears_redo) = match pending {
            PendingHistory::Initial(record) => (record, true, true),
            PendingHistory::Undo(record) => (record, false, false),
            PendingHistory::Redo(record) => (record, true, false),
        };
        match (&mut record, effect) {
            (
                UndoRecord::Created {
                    created_event_id, ..
                },
                MutationEffect::Created { event_id, .. },
            ) => {
                *created_event_id = event_id.clone();
            }
            (
                UndoRecord::Updated {
                    event_id,
                    before,
                    after,
                },
                MutationEffect::Updated {
                    event_id: confirmed_id,
                    ..
                },
            ) => {
                *event_id = confirmed_id.clone();
                before.draft.occurrence_id = Some(confirmed_id.clone());
                after.draft.occurrence_id = Some(confirmed_id.clone());
            }
            (
                UndoRecord::Deleted {
                    restored_event_id, ..
                },
                MutationEffect::Created { event_id, .. },
            ) => {
                *restored_event_id = Some(event_id.clone());
            }
            _ => {}
        }
        if clears_redo {
            self.redo_stack.clear();
        }
        if push_to_undo {
            Self::push_history(&mut self.undo_stack, record);
        } else {
            Self::push_history(&mut self.redo_stack, record);
        }
    }

    fn fail_history(&mut self) {
        match self.pending_history.take() {
            Some(PendingHistory::Undo(record)) => Self::push_history(&mut self.undo_stack, record),
            Some(PendingHistory::Redo(record)) => Self::push_history(&mut self.redo_stack, record),
            Some(PendingHistory::Initial(_)) | None => {}
        }
    }

    fn push_history(stack: &mut Vec<UndoRecord>, record: UndoRecord) {
        if stack.len() == UNDO_HISTORY_LIMIT {
            stack.remove(0);
        }
        stack.push(record);
    }

    /// Tags an event mutation at the UI/worker boundary. Only updates carrying
    /// this nonce may alter the active editor: an old helper completion must
    /// never close or overwrite a newer editor session.
    pub fn begin_mutation_session(&mut self, command: WorkerCommand) -> WorkerCommand {
        if !matches!(
            &command,
            WorkerCommand::Create(_)
                | WorkerCommand::Update(_, _, _, _, _)
                | WorkerCommand::Delete(_, _, _)
        ) {
            return command;
        }
        self.next_mutation_session = self.next_mutation_session.wrapping_add(1).max(1);
        let session = self.next_mutation_session;
        self.active_mutation_session = Some(session);
        match command {
            WorkerCommand::Create(event) => WorkerCommand::CreateWithSession { session, event },
            WorkerCommand::Update(event, span, recurrence_scope, alarms, time_mutation) => {
                WorkerCommand::UpdateWithSession {
                    session,
                    event,
                    span,
                    recurrence_scope,
                    alarms,
                    time_mutation,
                }
            }
            WorkerCommand::Delete(event_id, span, recurrence_scope) => {
                WorkerCommand::DeleteWithSession {
                    session,
                    event_id,
                    span,
                    recurrence_scope,
                }
            }
            other => other,
        }
    }

    fn mutation_session_is_current(&self, session: u64) -> bool {
        self.active_mutation_session == Some(session)
    }

    fn apply_mutation_success(&mut self, effect: MutationEffect) {
        self.complete_history(&effect);
        self.mutation_state = MutationState::Idle;
        self.active_mutation_session = None;
        self.form = None;
        self.form_dirty = false;
        self.pending_recurring_mutation = None;
        self.pending_delete_event_id = None;
        if !matches!(effect, MutationEffect::Deleted { .. }) {
            // The backend response is authoritative, including a concrete
            // EventKit exception ID produced by a recurring ThisEvent edit.
            self.pending_selection = Some(effect.event_id().to_owned());
        } else {
            self.interaction_context = None;
        }
        if matches!(self.mode, Mode::Form | Mode::QuickAdd) {
            self.close_all_modals();
            self.interaction_context = None;
        }
        self.status = Some((effect.label().into(), false, Instant::now()));
    }

    fn apply_mutation_failure(&mut self, text: String) {
        self.fail_history();
        self.mutation_state = MutationState::Failed;
        self.active_mutation_session = None;
        self.status = Some((text, true, Instant::now()));
    }

    fn enter_modal(&mut self, mode: Mode) {
        self.modal_stack.push(ModalFrame {
            current: mode,
            return_to: self.mode,
        });
        self.mode = mode;
    }

    /// Turn the current overlay into its next step without adding another
    /// return level. Selecting a recurrence scope, for example, replaces the
    /// scope picker with the event form; cancelling the form returns to the
    /// original caller, not to a stale scope picker.
    fn replace_modal(&mut self, mode: Mode) {
        if let Some(frame) = self.modal_stack.last_mut() {
            frame.current = mode;
        } else {
            self.modal_stack.push(ModalFrame {
                current: mode,
                return_to: Mode::Normal,
            });
        }
        self.mode = mode;
    }

    fn leave_modal(&mut self) {
        self.mode = self
            .modal_stack
            .pop()
            .map(|frame| frame.return_to)
            .unwrap_or(Mode::Normal);
    }

    fn interaction_context_for(&self, event: &Event) -> InteractionContext {
        InteractionContext {
            source_view: self.view,
            source_active_date: self.active_date,
            occurrence_id: event.id.clone(),
            occurrence_local_date: event.display_start_date(),
            provider_id: event.provider_id.clone(),
            calendar_id: event.calendar_id.clone(),
            canonical_occurrence_start: event.start,
        }
    }

    fn ensure_interaction_context(&mut self, event: &Event) {
        if self
            .interaction_context
            .as_ref()
            .is_none_or(|context| context.occurrence_id != event.id)
        {
            self.interaction_context = Some(self.interaction_context_for(event));
        }
    }

    fn restore_interaction_context(&mut self) -> bool {
        let Some((view, date, id)) = self.interaction_context.as_ref().map(|context| {
            (
                context.source_view,
                context.source_active_date,
                context.occurrence_id.clone(),
            )
        }) else {
            return false;
        };
        self.view = view;
        self.active_date = date;
        self.select_visible_event_id(&id)
    }

    fn close_details(&mut self) {
        let context = self.interaction_context.clone();
        self.leave_modal();
        if let Some(context) = context {
            self.view = context.source_view;
            self.active_date = context.source_active_date;
            if !self.select_visible_event_id(&context.occurrence_id) {
                self.clear_event_selection();
            }
        }
        self.interaction_context = None;
        self.detail_scroll = 0;
    }

    fn finish_editor_cancel(&mut self) {
        self.form = None;
        self.form_dirty = false;
        self.mutation_state = MutationState::Idle;
        self.pending_recurring_mutation = None;
        self.leave_modal();
        // A Details-origin editor returns to its still-live Details frame.
        // A direct editor return restores the concrete occurrence to its
        // source view and then releases the short-lived anchor.
        if !matches!(self.mode, Mode::Details) {
            self.finish_interaction_return();
        }
    }

    fn finish_interaction_return(&mut self) {
        self.restore_interaction_context();
        self.interaction_context = None;
    }

    fn close_all_modals(&mut self) {
        self.modal_stack.clear();
        self.mode = Mode::Normal;
    }

    pub fn apply_update(&mut self, update: WorkerUpdate) {
        match update {
            WorkerUpdate::Syncing(value) => self.syncing = value,
            WorkerUpdate::BackendState(state) => self.backend_state = state,
            WorkerUpdate::CalendarCapabilities(capabilities) => {
                self.calendar_capabilities = capabilities
            }
            WorkerUpdate::CalendarSources(sources) => self.calendar_sources = sources,
            WorkerUpdate::Snapshot(snapshot) => {
                let details_target_id = self
                    .interaction_context
                    .as_ref()
                    .filter(|_| {
                        matches!(self.mode, Mode::Details)
                            || self
                                .modal_stack
                                .iter()
                                .any(|frame| frame.current == Mode::Details)
                    })
                    .map(|context| context.occurrence_id.clone())
                    .or_else(|| {
                        matches!(self.mode, Mode::Details)
                            .then(|| self.selected_event_ref().map(|event| event.id.clone()))
                            .flatten()
                    });
                let selected_id = self
                    .pending_selection
                    .take()
                    .or_else(|| {
                        self.interaction_context
                            .as_ref()
                            .map(|context| context.occurrence_id.clone())
                    })
                    .or_else(|| self.selected_event_ref().map(|event| event.id.clone()));
                let selected_calendar_id = self.pending_calendar_selection.take().or_else(|| {
                    self.snapshot
                        .calendars
                        .get(self.selected_calendar)
                        .map(|calendar| calendar.id.clone())
                });
                let form_calendar_id = self.form.as_ref().and_then(|form| {
                    self.snapshot
                        .calendars
                        .get(form.calendar_index)
                        .map(|calendar| calendar.id.clone())
                });
                self.snapshot = snapshot;
                if let (Some(form), Some(id)) = (self.form.as_mut(), form_calendar_id) {
                    if let Some(index) = self
                        .snapshot
                        .calendars
                        .iter()
                        .position(|calendar| calendar.id == id)
                    {
                        form.calendar_index = index;
                    } else {
                        // Keep the draft intact but make its destination
                        // invalid so a later save cannot silently target a
                        // different calendar after external deletion.
                        form.calendar_index = self.snapshot.calendars.len();
                        self.status = Some((
                            "The draft calendar was removed; choose another calendar".into(),
                            true,
                            Instant::now(),
                        ));
                    }
                }
                if let Some(id) = selected_calendar_id {
                    if let Some(index) = self
                        .snapshot
                        .calendars
                        .iter()
                        .position(|calendar| calendar.id == id)
                    {
                        self.selected_calendar = index;
                    } else {
                        self.selected_calendar = self
                            .selected_calendar
                            .min(self.snapshot.calendars.len().saturating_sub(1));
                        if matches!(
                            self.mode,
                            Mode::CalendarManager | Mode::CalendarManagerDetails
                        ) {
                            self.status = Some((
                                "Selected calendar was removed".into(),
                                false,
                                Instant::now(),
                            ));
                        }
                    }
                } else {
                    self.selected_calendar = self
                        .selected_calendar
                        .min(self.snapshot.calendars.len().saturating_sub(1));
                }
                if let Some(id) = selected_id {
                    if !self.select_visible_event_id(&id) {
                        self.clear_event_selection();
                        self.status =
                            Some(("Selected event was removed".into(), false, Instant::now()));
                    }
                } else {
                    self.clamp_selection();
                }
                // Details are a view of one stable event identity. If an
                // authoritative refresh removed that identity, close instead
                // of reusing the fallback row and showing another event as if
                // it were the original details target.
                if let Some(id) = details_target_id
                    && !self.visible_events().iter().any(|event| event.id == id)
                {
                    self.close_all_modals();
                    self.interaction_context = None;
                    self.detail_scroll = 0;
                }
            }
            WorkerUpdate::RangeStarted(id) if id == self.visible_range_request_id => {
                self.visible_range_state = VisibleRangeState::Loading;
            }
            WorkerUpdate::RangeLoaded(id) if id == self.visible_range_request_id => {
                self.visible_range_state = VisibleRangeState::Ready;
            }
            WorkerUpdate::RangeFailed(id, error) if id == self.visible_range_request_id => {
                let range = self.visible_range.unwrap_or_else(|| RangeRequest {
                    id,
                    start: self.view_range().0,
                    end: self.view_range().1,
                    all_day_range: {
                        let (start_date, end_date_exclusive) = self.view_date_range();
                        Some(CalendarDateRange {
                            start_date,
                            end_date_exclusive,
                        })
                    },
                    reason: RangeReason::VisibleWeek,
                    priority: RangePriority::Interactive,
                });
                self.visible_range_state = VisibleRangeState::Failed(VisibleRangeError {
                    request_id: id,
                    range,
                    error: error.clone(),
                    timestamp: Utc::now(),
                });
                self.status = Some((error, true, Instant::now()));
            }
            WorkerUpdate::RangeStarted(_)
            | WorkerUpdate::RangeLoaded(_)
            | WorkerUpdate::RangeFailed(_, _) => {}
            WorkerUpdate::MutationSaving => self.mutation_state = MutationState::Saving,
            WorkerUpdate::MutationSucceeded(effect) => self.apply_mutation_success(effect),
            WorkerUpdate::MutationFailed(text) => self.apply_mutation_failure(text),
            WorkerUpdate::MutationSavingFor(session)
                if self.mutation_session_is_current(session) =>
            {
                self.mutation_state = MutationState::Saving;
            }
            WorkerUpdate::MutationSucceededFor(session, effect)
                if self.mutation_session_is_current(session) =>
            {
                self.apply_mutation_success(effect);
            }
            WorkerUpdate::MutationFailedFor(session, text)
                if self.mutation_session_is_current(session) =>
            {
                self.apply_mutation_failure(text);
            }
            WorkerUpdate::MutationSavingFor(_)
            | WorkerUpdate::MutationSucceededFor(_, _)
            | WorkerUpdate::MutationFailedFor(_, _) => {
                // A previous request completed after its editor was cancelled
                // or replaced. Its provider result is still reconciled by the
                // worker, but it cannot mutate this newer UI session.
            }
            WorkerUpdate::CalendarCreateSucceeded(calendar) => {
                self.mutation_state = MutationState::Idle;
                self.calendar_form = None;
                self.pending_calendar_selection = Some(calendar.id);
                self.mode = Mode::CalendarManager;
                self.status = Some(("Calendar created".into(), false, Instant::now()));
            }
            WorkerUpdate::CalendarCreateFailed(error) => {
                self.mutation_state = MutationState::Failed;
                self.status = Some((calendar_error_message(error).into(), true, Instant::now()));
            }
            WorkerUpdate::CalendarRenameSucceeded(calendar) => {
                self.mutation_state = MutationState::Idle;
                self.calendar_rename_form = None;
                self.pending_calendar_selection = Some(calendar.id);
                self.mode = Mode::CalendarManager;
                self.status = Some(("Calendar renamed".into(), false, Instant::now()));
            }
            WorkerUpdate::CalendarRenameFailed(error) => {
                self.mutation_state = MutationState::Failed;
                self.status = Some((
                    calendar_rename_error_message(error).into(),
                    true,
                    Instant::now(),
                ));
            }
            WorkerUpdate::CalendarColorSucceeded(calendar) => {
                self.mutation_state = MutationState::Idle;
                self.calendar_color_form = None;
                self.pending_calendar_selection = Some(calendar.id);
                self.mode = Mode::CalendarManager;
                self.status = Some(("Calendar color updated".into(), false, Instant::now()));
            }
            WorkerUpdate::CalendarColorFailed(error) => {
                self.mutation_state = MutationState::Failed;
                self.status = Some((
                    calendar_color_error_message(error).into(),
                    true,
                    Instant::now(),
                ));
            }
            WorkerUpdate::CalendarDeleteSucceeded(_) => {
                self.mutation_state = MutationState::Idle;
                self.calendar_delete_confirmation = None;
                self.mode = Mode::CalendarManager;
                self.status = Some(("Calendar deleted".into(), false, Instant::now()));
            }
            WorkerUpdate::CalendarDeleteFailed(error) => {
                self.mutation_state = MutationState::Failed;
                self.mode = Mode::CalendarManager;
                self.calendar_delete_confirmation = None;
                self.status = Some((
                    calendar_delete_error_message(error).into(),
                    true,
                    Instant::now(),
                ));
            }
            WorkerUpdate::Status(text) => self.status = Some((text, false, Instant::now())),
            WorkerUpdate::Error(text) => self.status = Some((text, true, Instant::now())),
        }
    }

    pub fn visible_events(&self) -> Vec<&Event> {
        let (start, end) = self.view_range();
        let (visible_start_date, visible_end_date) = self.view_date_range();
        let enabled = self
            .snapshot
            .calendars
            .iter()
            .filter(|c| c.enabled)
            .map(|c| c.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut events = self
            .snapshot
            .events
            .iter()
            .filter(|event| {
                let intersects = if event.all_day_date_range().is_some() {
                    event.all_day_intersects_dates(visible_start_date, visible_end_date)
                } else {
                    event.start < end && event.end > start
                };
                intersects && enabled.contains(event.calendar_id.as_str())
            })
            .collect::<Vec<_>>();
        events.sort_by_key(|e| (e.start, e.end));
        events
    }

    pub fn search_results(&self) -> Vec<&Event> {
        let query = SearchQuery::parse(&self.search_query);
        if query.is_empty() {
            return vec![];
        }
        let mut events = self
            .snapshot
            .events
            .iter()
            .filter(|event| {
                query
                    .calendar
                    .as_ref()
                    .is_none_or(|filter| event.calendar_id == *filter)
                    && query.location.as_ref().is_none_or(|filter| {
                        normalized_search_text(&event.location).contains(filter)
                    })
                    && query
                        .recurring
                        .is_none_or(|value| event.has_recurrence == value)
                    && query.all_day.is_none_or(|value| event.all_day == value)
                    && event_matches_search_dates(
                        event,
                        query.from,
                        query.to,
                        event.display_start_date(),
                    )
                    && query.attendee.as_ref().is_none_or(|filter| {
                        let attendee_text = normalized_search_text(
                            &event
                                .attendees
                                .iter()
                                .map(|attendee| format!("{} {}", attendee.name, attendee.email))
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                        attendee_text.contains(filter)
                    })
                    && (query.terms.is_empty() || {
                        let calendar = self
                            .snapshot
                            .calendars
                            .iter()
                            .find(|calendar| calendar.id == event.calendar_id);
                        let text = event.searchable_text(calendar);
                        query.terms.iter().all(|word| text.contains(word))
                    })
            })
            .collect::<Vec<_>>();
        events.sort_by_key(|event| search_rank(event, &query, self.active_date));
        events
    }

    pub fn selected_event_ref(&self) -> Option<&Event> {
        match self.mode {
            Mode::Search => self.search_results().get(self.search_selected).copied(),
            _ => self
                .visible_events()
                .get(self.selected_event)
                .copied()
                // Month actions must target an event from the active calendar
                // date, never a stale selection carried from another cell.
                .filter(|event| {
                    !matches!(self.view, View::Month) || event.occurs_on(self.active_date)
                }),
        }
    }

    /// Details are anchored to an explicit occurrence rather than whichever
    /// visible row currently occupies `selected_event`. This matters while a
    /// refresh reorders rows and for Month events represented behind overflow.
    pub fn details_event_ref(&self) -> Option<&Event> {
        let context = self.interaction_context.as_ref()?;
        self.visible_events()
            .into_iter()
            .find(|event| event.id == context.occurrence_id)
    }

    fn selection_context(&self) -> SelectionContext {
        let selected = self.selected_event_ref();
        SelectionContext {
            active_date: self.active_date,
            selected_event_id: selected.map(|event| event.id.clone()),
            selected_event_date: selected.map(Event::display_start_date),
        }
    }

    fn select_visible_event_id(&mut self, id: &str) -> bool {
        let Some(index) = self
            .visible_events()
            .iter()
            .position(|event| event.id == id)
        else {
            return false;
        };
        self.selected_event = index;
        true
    }

    fn clear_event_selection(&mut self) {
        // `usize::MAX` is an intentionally out-of-range index. Keeping the
        // existing index field avoids a parallel selection state while making
        // `selected_event_ref()` reliably return None rather than a neighbour.
        self.selected_event = usize::MAX;
    }

    fn restore_selection_context(&mut self, context: SelectionContext) {
        self.active_date = context.selected_event_date.unwrap_or(context.active_date);
        if let Some(id) = context.selected_event_id {
            if !self.select_visible_event_id(&id) {
                self.clear_event_selection();
            }
        } else {
            self.clear_event_selection();
        }
    }

    pub fn calendar(&self, id: &str) -> Option<&CalendarInfo> {
        self.snapshot
            .calendars
            .iter()
            .find(|calendar| calendar.id == id)
    }

    pub fn view_range(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        let (start_date, end_date) = self.view_date_range();
        (local_midnight(start_date), local_midnight(end_date))
    }

    fn view_date_range(&self) -> (NaiveDate, NaiveDate) {
        match self.view {
            View::Day => (self.active_date, self.active_date + Duration::days(1)),
            // A week is a rolling seven-day window anchored at the active
            // calendar date. This keeps one-day navigation visibly in sync
            // with both rendering and the EnsureRange request; H/L still
            // moves the same window by a full seven-day period.
            View::Week => (self.active_date, self.active_date + Duration::days(7)),
            View::Month => {
                let first = self.active_date.with_day(1).unwrap();
                let offset = if self.config.week_start.eq_ignore_ascii_case("sunday") {
                    first.weekday().num_days_from_sunday()
                } else {
                    first.weekday().num_days_from_monday()
                };
                let start = first - Duration::days(offset.into());
                (start, start + Duration::days(42))
            }
            View::Agenda => (self.active_date, self.active_date + Duration::days(60)),
        }
    }

    pub fn visible_range_request(&mut self) -> RangeRequest {
        let (start, end) = self.view_range();
        let (start_date, end_date_exclusive) = self.view_date_range();
        self.visible_range_request_id = self.visible_range_request_id.wrapping_add(1).max(1);
        let request = RangeRequest {
            id: self.visible_range_request_id,
            start,
            end,
            all_day_range: Some(CalendarDateRange {
                start_date,
                end_date_exclusive,
            }),
            reason: match self.view {
                View::Day => RangeReason::VisibleDay,
                View::Week => RangeReason::VisibleWeek,
                View::Month => RangeReason::VisibleMonth,
                View::Agenda => RangeReason::AgendaPage,
            },
            priority: RangePriority::Interactive,
        };
        self.visible_range = Some(request);
        self.visible_range_state = VisibleRangeState::Loading;
        request
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Esc && self.drag_session.event_id.is_some() {
            self.cancel_drag_session();
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Calendars => self.handle_calendars(key),
            Mode::CalendarManager => self.handle_calendar_manager(key),
            Mode::CalendarManagerDetails => self.handle_calendar_manager_details(key),
            Mode::CalendarCreate => self.handle_calendar_create(key),
            Mode::CalendarRename => self.handle_calendar_rename(key),
            Mode::CalendarColor => self.handle_calendar_color(key),
            Mode::CalendarDeleteConfirm => self.handle_calendar_delete_confirm(key),
            Mode::QuickAdd => self.handle_quick_add(key),
            Mode::Details => self.handle_details(key),
            Mode::Search => self.handle_search(key),
            Mode::Palette => self.handle_palette(key),
            Mode::DateJump => self.handle_date_jump(key),
            Mode::Form => self.handle_form(key),
            Mode::DiscardConfirm => self.handle_discard_confirm(key),
            Mode::Delete => self.handle_delete(key),
            Mode::RecurringEditScope | Mode::RecurringDeleteScope => {
                self.handle_recurring_scope(key)
            }
            Mode::Help => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                        self.leave_modal();
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.help_scroll = self.help_scroll.saturating_add(1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                    }
                    KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(8),
                    KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(8),
                    KeyCode::Home => self.help_scroll = 0,
                    KeyCode::End => self.help_scroll = u16::MAX,
                    _ => {}
                }
                None
            }
        }
    }

    /// Accepts provider-neutral pointer input. Press, release, move, and
    /// cancellation are intentionally inert until a future drag adapter is
    /// introduced; wheel scrolling preserves existing navigation behavior.
    pub fn handle_pointer(&mut self, pointer: PointerEvent) -> Option<UserAction> {
        self.handle_pointer_with_hit_test(pointer, None)
    }

    /// Connects a provider-neutral pointer event to a snapshot of calendar
    /// geometry. This starts and previews drags only; a release returns a move
    /// intent for a caller to dispatch later and never executes it itself.
    pub fn handle_pointer_with_hit_test(
        &mut self,
        pointer: PointerEvent,
        geometry: Option<&CalendarHitGeometry>,
    ) -> Option<UserAction> {
        match pointer.action {
            PointerAction::Press
                if pointer.button == Some(crate::input::PointerButton::Primary) =>
            {
                let position = pointer.position?;
                let Some(CalendarHitTarget::ExistingEvent { event_id }) =
                    geometry.map(|geometry| geometry.hit_test(position.x, position.y))
                else {
                    return None;
                };
                // Pointer and keyboard focus must resolve the same concrete
                // occurrence before a drag is considered. A read-only event
                // can therefore still be selected even when dragging it is
                // correctly rejected below.
                self.select_visible_event_id(&event_id);
                self.start_drag_session(
                    event_id.clone(),
                    CalendarHitTarget::ExistingEvent { event_id },
                );
                None
            }
            PointerAction::Move if self.drag_session.event_id.is_some() => {
                if let (Some(position), Some(geometry)) = (pointer.position, geometry) {
                    self.update_drag_preview(geometry.hit_test(position.x, position.y));
                }
                None
            }
            PointerAction::Release if self.drag_session.event_id.is_some() => {
                if let (Some(position), Some(geometry)) = (pointer.position, geometry) {
                    self.update_drag_preview(geometry.hit_test(position.x, position.y));
                }
                let action = self.drop_drag_session();
                if action.is_none() {
                    self.cancel_drag_session();
                }
                action
            }
            PointerAction::Cancel => {
                self.cancel_drag_session();
                None
            }
            _ => {
                self.handle_pointer_navigation(pointer);
                None
            }
        }
    }

    fn handle_pointer_navigation(&mut self, pointer: PointerEvent) {
        match pointer.action {
            PointerAction::Scroll { delta_y, .. }
                if delta_y > 0 && matches!(self.view, View::Day | View::Week) =>
            {
                self.scroll_timeline(60)
            }
            PointerAction::Scroll { delta_y, .. }
                if delta_y < 0 && matches!(self.view, View::Day | View::Week) =>
            {
                self.scroll_timeline(-60)
            }
            PointerAction::Scroll { delta_y, .. } if delta_y > 0 => self.move_selection(1),
            PointerAction::Scroll { delta_y, .. } if delta_y < 0 => self.move_selection(-1),
            _ => {}
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if self.pending_g {
            self.pending_g = false;
            match key.code {
                KeyCode::Char('g') => {
                    return self.execute_action(UserAction::Today);
                }
                KeyCode::Char('d') => {
                    return self.execute_action(UserAction::ChangeView(View::Day));
                }
                KeyCode::Char('w') => {
                    return self.execute_action(UserAction::ChangeView(View::Week));
                }
                KeyCode::Char('m') => {
                    return self.execute_action(UserAction::ChangeView(View::Month));
                }
                KeyCode::Char('a') => {
                    return self.execute_action(UserAction::ChangeView(View::Agenda));
                }
                KeyCode::Char('c') => {
                    self.open_calendar_manager();
                    return None;
                }
                _ => {}
            }
            return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab if matches!(self.view, View::Month) => self.move_month_event_selection(1),
            KeyCode::BackTab if matches!(self.view, View::Month) => {
                self.move_month_event_selection(-1)
            }
            KeyCode::Char('j')
                if !key.modifiers.contains(KeyModifiers::ALT)
                    && matches!(self.view, View::Month | View::Week) =>
            {
                self.navigate_date(7);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Char('k')
                if !key.modifiers.contains(KeyModifiers::ALT)
                    && matches!(self.view, View::Month | View::Week) =>
            {
                self.navigate_date(-7);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Down if matches!(self.view, View::Month) => {
                self.navigate_date(7);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Up if matches!(self.view, View::Month) => {
                self.navigate_date(-7);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Char('j') if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_selection(1)
            }
            KeyCode::Char('k') if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_selection(-1)
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('h') if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.navigate_date(-1);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Char('l') if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.navigate_date(1);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Left => {
                self.navigate_date(-1);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Right => {
                self.navigate_date(1);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Char('H') => {
                self.navigate_period(-1);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::Char('L') => {
                self.navigate_period(1);
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            KeyCode::PageUp => self.scroll_timeline(-120),
            KeyCode::PageDown => self.scroll_timeline(120),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('t') => return self.execute_action(UserAction::Today),
            KeyCode::Char('c') => {
                self.sidebar_visible = !self.sidebar_visible;
                self.mode = if self.sidebar_visible {
                    Mode::Calendars
                } else {
                    Mode::Normal
                };
            }
            KeyCode::Char('[') => {
                if !self.snapshot.calendars.is_empty() {
                    self.sidebar_visible = true;
                    self.mode = Mode::Calendars;
                    self.selected_calendar = self.selected_calendar.saturating_sub(1);
                }
            }
            KeyCode::Char(']') => {
                if !self.snapshot.calendars.is_empty() {
                    self.sidebar_visible = true;
                    self.mode = Mode::Calendars;
                    self.selected_calendar =
                        (self.selected_calendar + 1).min(self.snapshot.calendars.len() - 1);
                }
            }
            KeyCode::Char('/') => return self.execute_action(UserAction::Search),
            KeyCode::Char(':') => {
                self.palette_query.clear();
                self.palette_selected = 0;
                self.enter_modal(Mode::Palette);
            }
            KeyCode::Char('n') => return self.execute_action(UserAction::CreateEvent),
            KeyCode::Char('a') => return self.execute_action(UserAction::QuickAdd),
            KeyCode::Char('D') => {
                return self.selected_event_action(UserAction::DuplicateEvent);
            }
            KeyCode::Char('e') => return self.selected_event_action(UserAction::EditEvent),
            KeyCode::Char('d') => return self.selected_event_action(UserAction::DeleteEvent),
            KeyCode::Char('u') => return self.execute_action(UserAction::Undo),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.execute_action(UserAction::Redo);
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::ALT) => {
                return self.move_selected_event(-1, 0);
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::ALT) => {
                return self.move_selected_event(1, 0);
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::ALT) => {
                return self.move_selected_event(0, i64::from(self.config.event.move_step_minutes));
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::ALT) => {
                return self
                    .move_selected_event(0, -i64::from(self.config.event.move_step_minutes));
            }
            KeyCode::Char('J') => {
                return self.resize_selected_event(i64::from(self.config.event.move_step_minutes));
            }
            KeyCode::Char('K') => {
                return self.resize_selected_event(-i64::from(self.config.event.move_step_minutes));
            }
            KeyCode::Char('r') => return self.execute_action(UserAction::Refresh),
            KeyCode::Char('R') => return self.execute_action(UserAction::RetryVisibleRange),
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.enter_modal(Mode::Help);
            }
            KeyCode::Enter if self.selected_event_ref().is_some() => {
                self.detail_scroll = 0;
                if let Some(event) = self.selected_event_ref().cloned() {
                    self.interaction_context = Some(self.interaction_context_for(&event));
                }
                self.enter_modal(Mode::Details);
            }
            _ => {}
        }
        None
    }

    fn handle_calendars(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('c') => {
                self.sidebar_visible = false;
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.snapshot.calendars.is_empty() {
                    self.selected_calendar =
                        (self.selected_calendar + 1).min(self.snapshot.calendars.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_calendar = self.selected_calendar.saturating_sub(1)
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(calendar) = self.snapshot.calendars.get_mut(self.selected_calendar) {
                    calendar.enabled = !calendar.enabled;
                    return Some(WorkerCommand::SetCalendarEnabled(
                        calendar.id.clone(),
                        calendar.enabled,
                    ));
                }
            }
            _ => {}
        }
        None
    }

    fn open_calendar_manager(&mut self) {
        self.pending_g = false;
        self.sidebar_visible = false;
        self.selected_calendar = self
            .selected_calendar
            .min(self.snapshot.calendars.len().saturating_sub(1));
        self.mode = Mode::CalendarManager;
    }

    fn handle_calendar_manager(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.snapshot.calendars.is_empty() {
                    self.selected_calendar =
                        (self.selected_calendar + 1).min(self.snapshot.calendars.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_calendar = self.selected_calendar.saturating_sub(1)
            }
            KeyCode::Enter => self.mode = Mode::CalendarManagerDetails,
            KeyCode::Char('c') if self.calendar_capabilities.can_create => {
                self.open_calendar_create()
            }
            KeyCode::Char('e') => self.open_calendar_rename(),
            KeyCode::Char('C') => self.open_calendar_color(),
            KeyCode::Char('d') => self.open_calendar_delete_confirm(),
            _ => {}
        }
        None
    }

    fn handle_calendar_manager_details(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            self.mode = Mode::CalendarManager;
        }
        None
    }

    fn open_calendar_create(&mut self) {
        if self.calendar_sources.is_empty() {
            self.status = Some((
                "Calendar sources are unavailable; refresh and try again".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        self.calendar_form = Some(CalendarForm::new());
        self.mutation_state = MutationState::Idle;
        self.mode = Mode::CalendarCreate;
    }

    fn handle_calendar_create(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            if self.mutation_state == MutationState::Saving {
                return None;
            }
            let result = self
                .calendar_form
                .as_ref()
                .ok_or_else(|| "Calendar form is unavailable".to_string())
                .and_then(|form| form.to_request(&self.calendar_sources));
            return match result {
                Ok(request) => {
                    self.mutation_state = MutationState::Saving;
                    Some(WorkerCommand::CreateCalendar(request))
                }
                Err(error) => {
                    self.status = Some((error, true, Instant::now()));
                    None
                }
            };
        }
        let Some(form) = self.calendar_form.as_mut() else {
            self.mode = Mode::CalendarManager;
            return None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::CalendarManager;
                self.calendar_form = None;
                self.mutation_state = MutationState::Idle;
            }
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Left => form.adjust(-1, &self.calendar_sources),
            KeyCode::Right | KeyCode::Char(' ') if form.is_choice() => {
                form.adjust(1, &self.calendar_sources)
            }
            KeyCode::Backspace => form.backspace(),
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.push(character)
            }
            _ => {}
        }
        None
    }

    fn open_calendar_rename(&mut self) {
        if !self.calendar_capabilities.can_update {
            return;
        }
        let Some(calendar) = self.snapshot.calendars.get(self.selected_calendar) else {
            return;
        };
        if !calendar.permissions.can_modify_metadata {
            self.status = Some((
                "This calendar's metadata cannot be modified".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        self.calendar_rename_form = Some(CalendarRenameForm::new(calendar));
        self.mutation_state = MutationState::Idle;
        self.mode = Mode::CalendarRename;
    }

    fn handle_calendar_rename(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            if self.mutation_state == MutationState::Saving {
                return None;
            }
            let result = self
                .calendar_rename_form
                .as_ref()
                .ok_or_else(|| "Calendar rename form is unavailable".to_string())
                .and_then(CalendarRenameForm::to_request);
            return match result {
                Ok(request) => {
                    self.mutation_state = MutationState::Saving;
                    Some(WorkerCommand::RenameCalendar(request))
                }
                Err(error) => {
                    self.status = Some((error, true, Instant::now()));
                    None
                }
            };
        }
        let Some(form) = self.calendar_rename_form.as_mut() else {
            self.mode = Mode::CalendarManager;
            return None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::CalendarManager;
                self.calendar_rename_form = None;
                self.mutation_state = MutationState::Idle;
            }
            KeyCode::Backspace => {
                form.title.pop();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.title.push(character)
            }
            _ => {}
        }
        None
    }

    fn open_calendar_color(&mut self) {
        if !self.calendar_capabilities.can_change_color {
            return;
        }
        let Some(calendar) = self.snapshot.calendars.get(self.selected_calendar) else {
            return;
        };
        if !calendar.permissions.can_modify_metadata {
            self.status = Some((
                "Calendar metadata is read-only".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        self.calendar_color_form = Some(CalendarColorForm::new(calendar));
        self.mutation_state = MutationState::Idle;
        self.mode = Mode::CalendarColor;
    }

    fn handle_calendar_color(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            if self.mutation_state == MutationState::Saving {
                return None;
            }
            let result = self
                .calendar_color_form
                .as_ref()
                .ok_or_else(|| "Calendar color form is unavailable".to_string())
                .and_then(CalendarColorForm::to_request);
            return match result {
                Ok(request) => {
                    self.mutation_state = MutationState::Saving;
                    Some(WorkerCommand::SetCalendarColor(request))
                }
                Err(error) => {
                    self.status = Some((error, true, Instant::now()));
                    None
                }
            };
        }
        let Some(form) = self.calendar_color_form.as_mut() else {
            self.mode = Mode::CalendarManager;
            return None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::CalendarManager;
                self.calendar_color_form = None;
                self.mutation_state = MutationState::Idle;
            }
            KeyCode::Backspace => {
                form.color.pop();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.color.push(character)
            }
            _ => {}
        }
        None
    }

    fn open_calendar_delete_confirm(&mut self) {
        if !self.calendar_capabilities.can_delete {
            self.status = Some((
                "Calendar deletion is not supported".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        let Some(calendar) = self.snapshot.calendars.get(self.selected_calendar) else {
            return;
        };
        if !calendar.permissions.can_delete {
            self.status = Some((
                "This calendar cannot be deleted".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        self.calendar_delete_confirmation = Some(CalendarDeleteConfirmation {
            calendar_id: calendar.id.clone(),
            title: calendar.title.clone(),
        });
        self.mutation_state = MutationState::Idle;
        self.mode = Mode::CalendarDeleteConfirm;
    }

    fn handle_calendar_delete_confirm(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if self.mutation_state == MutationState::Deleting {
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                self.calendar_delete_confirmation = None;
                self.mutation_state = MutationState::Idle;
                self.mode = Mode::CalendarManager;
            }
            KeyCode::Char('y') => {
                let Some(confirmation) = self.calendar_delete_confirmation.as_ref() else {
                    self.mode = Mode::CalendarManager;
                    return None;
                };
                self.mutation_state = MutationState::Deleting;
                return Some(WorkerCommand::DeleteCalendar(DeleteCalendarRequest {
                    calendar_id: confirmation.calendar_id.clone(),
                }));
            }
            _ => {}
        }
        None
    }

    fn begin_quick_add(&mut self) {
        self.quick_add_input.clear();
        self.mutation_state = MutationState::Idle;
        self.enter_modal(Mode::QuickAdd);
    }

    pub fn quick_add_preview(&self) -> QuickAddParseResult {
        quick_add::parse(
            &self.quick_add_input,
            QuickAddContext {
                reference: Local::now(),
                selected_date: self.active_date,
                event: &self.config.event,
                calendars: &self.snapshot.calendars,
            },
        )
    }

    fn handle_quick_add(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            if self.mutation_state == MutationState::Saving {
                return None;
            }
            let parsed = self.quick_add_preview();
            let Some(draft) = parsed.draft else {
                self.status = Some((
                    parsed
                        .warnings
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "Quick Add needs more information".into()),
                    true,
                    Instant::now(),
                ));
                return None;
            };
            if parsed.status != QuickAddStatus::Ready {
                return None;
            }
            if !self
                .snapshot
                .calendars
                .iter()
                .find(|calendar| calendar.id == draft.calendar_id)
                .is_some_and(|calendar| calendar.permissions.can_create_events)
            {
                self.status = Some(("Calendar is read-only".into(), true, Instant::now()));
                return None;
            }
            self.mutation_state = MutationState::Saving;
            return Some(WorkerCommand::Create(EventDraft {
                id: None,
                occurrence_id: None,
                occurrence_start: None,
                occurrence_calendar_id: None,
                calendar_id: draft.calendar_id,
                title: draft.title,
                time: draft.time,
                location: draft.location,
                notes: draft.notes,
                url: String::new(),
                time_zone: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
                availability: Availability::Busy,
                attendees: vec![],
                alarms: draft.alarms,
                recurrence: draft.recurrence,
            }));
        }
        if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
            let parsed = self.quick_add_preview();
            let Some(draft) = parsed.draft else {
                self.status = Some((
                    "Fix Quick Add before editing details".into(),
                    true,
                    Instant::now(),
                ));
                return None;
            };
            let calendar_index = self
                .snapshot
                .calendars
                .iter()
                .position(|c| c.id == draft.calendar_id)?;
            let (start, end, all_day) = match draft.time {
                EventTimeInput::Timed { start, end } => (
                    start
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                    end.with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                    false,
                ),
                EventTimeInput::AllDay {
                    start_date,
                    end_date_exclusive,
                } => {
                    let inclusive_end = end_date_exclusive
                        .checked_sub_signed(Duration::days(1))
                        .expect("validated all-day range has an inclusive end");
                    (start_date.to_string(), inclusive_end.to_string(), true)
                }
                EventTimeInput::LegacyAllDayUnknown { .. } => {
                    self.status = Some((
                        "Quick Add cannot edit a legacy all-day draft".into(),
                        true,
                        Instant::now(),
                    ));
                    return None;
                }
            };
            self.form = Some(EventForm {
                editor_mode: EditorMode::Create,
                id: None,
                occurrence_id: None,
                occurrence_start: None,
                occurrence_calendar_id: None,
                title: draft.title,
                calendar_index,
                start,
                end,
                all_day,
                location: draft.location,
                notes: draft.notes,
                url: String::new(),
                alarm_state: AlarmEditorState::from_existing(&draft.alarms),
                recurrence: RecurrenceEditorState::from_rules(&draft.recurrence),
                weekday_cursor: WeekdayCursor::Monday,
                weekday_selection: BTreeSet::new(),
                time_zone: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
                time_zone_provenance: TimeZoneProvenance::ExplicitEvent,
                availability: Availability::Busy,
                span: EventSpan::ThisEvent,
                recurrence_scope: None,
                selected: 0,
                all_day_editor: None,
                create_all_day_timed_backup: None,
            });
            self.form_dirty = true;
            self.replace_modal(Mode::Form);
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                self.leave_modal();
                self.mutation_state = MutationState::Idle;
            }
            KeyCode::Backspace => {
                self.quick_add_input.pop();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quick_add_input.push(character)
            }
            _ => {}
        }
        None
    }

    fn handle_details(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_details(),
            KeyCode::Char('e') => return self.details_event_action(UserAction::EditEvent),
            KeyCode::Char('D') => return self.details_event_action(UserAction::DuplicateEvent),
            KeyCode::Char('d') => return self.details_event_action(UserAction::DeleteEvent),
            KeyCode::Down | KeyCode::Char('j') => {
                self.detail_scroll = self.detail_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(8),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(8),
            KeyCode::Char('o') => return self.open_detail_link(),
            _ => {}
        }
        None
    }

    fn handle_search(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Esc => {
                self.leave_modal();
                self.search_query.clear();
            }
            KeyCode::Enter => {
                let open_details = key.modifiers.contains(KeyModifiers::SHIFT);
                if let Some((date, id)) = self
                    .search_results()
                    .get(self.search_selected)
                    .map(|event| (event.display_start_date(), event.id.clone()))
                {
                    self.active_date = date;
                    self.view = View::Agenda;
                    if !self.select_visible_event_id(&id) {
                        self.clear_event_selection();
                    }
                    if open_details {
                        if let Some(event) = self
                            .visible_events()
                            .into_iter()
                            .find(|event| event.id == id)
                            .cloned()
                        {
                            self.interaction_context = Some(self.interaction_context_for(&event));
                        }
                        self.enter_modal(Mode::Details);
                    } else {
                        self.leave_modal();
                    }
                    return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                let count = self.search_results().len();
                if count > 0 {
                    self.search_selected = (self.search_selected + 1).min(count - 1);
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.search_selected = self.search_selected.saturating_sub(1)
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.search_selected = 0;
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_query.push(character);
                self.search_selected = 0;
            }
            _ => {}
        }
        None
    }

    fn handle_palette(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        let entries = self.palette_entries();
        match key.code {
            KeyCode::Esc => self.leave_modal(),
            KeyCode::Down | KeyCode::Tab => {
                if !entries.is_empty() {
                    self.palette_selected = (self.palette_selected + 1).min(entries.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.palette_selected = self.palette_selected.saturating_sub(1)
            }
            KeyCode::Backspace => {
                self.palette_query.pop();
                self.palette_selected = 0;
            }
            KeyCode::Enter => {
                if let Some(entry) = entries.get(self.palette_selected).copied() {
                    if entry.enabled {
                        return self.execute_palette(entry.command);
                    }
                    self.status =
                        Some((entry.unavailable_reason().to_owned(), true, Instant::now()));
                }
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette_query.push(character);
                self.palette_selected = 0;
            }
            _ => {}
        }
        None
    }

    fn handle_date_jump(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Esc => self.leave_modal(),
            KeyCode::Enter => match parse_date_jump(&self.palette_query) {
                Some(date) => return self.execute_action(UserAction::GoToDate(date)),
                None => {
                    self.status = Some((
                        "Use YYYY-MM-DD, DD.MM.YYYY, today, tomorrow, or yesterday".into(),
                        true,
                        Instant::now(),
                    ))
                }
            },
            KeyCode::Backspace => {
                self.palette_query.pop();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette_query.push(character)
            }
            _ => {}
        }
        None
    }

    fn handle_form(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            if self.mutation_state == MutationState::Saving {
                return None;
            }
            let form = self.form.as_ref().unwrap();
            let result = form.to_draft(&self.snapshot.calendars);
            return match result {
                Ok((draft, span, alarm_mutation)) => {
                    let time_mutation = match form.time_mutation() {
                        Ok(mutation) => mutation,
                        Err(error) => {
                            self.status = Some((error, true, Instant::now()));
                            return None;
                        }
                    };
                    let updating = draft.id.is_some();
                    self.mutation_state = MutationState::Saving;
                    Some(if updating {
                        WorkerCommand::Update(
                            draft,
                            span,
                            self.form.as_ref().and_then(|form| form.recurrence_scope),
                            alarm_mutation,
                            time_mutation,
                        )
                    } else {
                        WorkerCommand::Create(draft)
                    })
                }
                Err(error) => {
                    self.status = Some((error, true, Instant::now()));
                    None
                }
            };
        }
        let Some(form) = self.form.as_mut() else {
            self.leave_modal();
            return None;
        };
        if form.field() == FormField::Alarms
            && form.all_day
            && !form.alarm_state.allows_all_day_actions()
        {
            self.status = Some((
                "Reminders are unavailable for all-day events".into(),
                true,
                Instant::now(),
            ));
            return None;
        }
        if form.field() == FormField::Alarms
            && let Some(result) = form.alarm_state.handle_key(key, &form.time_zone)
        {
            match result {
                Ok(changed) => self.form_dirty |= changed,
                Err(error) => self.status = Some((error, true, Instant::now())),
            }
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                if self.form_dirty {
                    self.enter_modal(Mode::DiscardConfirm);
                } else {
                    self.finish_editor_cancel();
                }
            }
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Left => {
                let changes_value = form.is_toggle();
                form.adjust(-1, &self.snapshot.calendars);
                self.form_dirty |= changes_value;
            }
            KeyCode::Right => {
                let changes_value = form.is_toggle();
                form.adjust(1, &self.snapshot.calendars);
                self.form_dirty |= changes_value;
            }
            KeyCode::Char(' ') if form.field() == FormField::Weekdays => {
                form.toggle_weekday();
                self.form_dirty = true;
            }
            KeyCode::Char(' ') if form.is_toggle() => {
                form.adjust(1, &self.snapshot.calendars);
                self.form_dirty = true;
            }
            KeyCode::Backspace => {
                self.form_dirty |= form.backspace();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.form_dirty |= form.push(character);
            }
            _ => {}
        }
        None
    }

    fn handle_discard_confirm(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.leave_modal();
                return self.handle_form(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.form = None;
                self.form_dirty = false;
                self.mutation_state = MutationState::Idle;
                self.pending_recurring_mutation = None;
                self.leave_modal();
                self.finish_editor_cancel();
            }
            KeyCode::Esc => self.leave_modal(),
            _ => {}
        }
        None
    }

    fn handle_delete(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.pending_recurring_mutation = None;
                self.pending_delete_event_id = None;
                self.leave_modal();
                if !matches!(self.mode, Mode::Details) {
                    self.finish_interaction_return();
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let id = self.pending_delete_event_id.take();
                self.pending_recurring_mutation = None;
                self.close_all_modals();
                if let Some(id) =
                    id.filter(|id| self.visible_events().iter().any(|event| event.id == *id))
                {
                    return Some(WorkerCommand::Delete(
                        id,
                        self.delete_span,
                        self.delete_recurrence_scope,
                    ));
                }
                self.status = Some(("Event no longer exists".into(), true, Instant::now()));
            }
            _ => {}
        }
        None
    }

    fn begin_new_form(&mut self) {
        let writable = self
            .snapshot
            .calendars
            .iter()
            .position(|calendar| calendar.is_writable);
        match writable {
            Some(index) => {
                self.form = Some(EventForm::new(
                    index,
                    &self.snapshot.calendars,
                    self.active_date,
                    self.suggested_start_minute(),
                    self.config.event.default_duration_minutes,
                ));
                self.form_dirty = false;
                self.mutation_state = MutationState::Idle;
                self.enter_modal(Mode::Form);
            }
            None => {
                self.status = Some((
                    "No writable calendars are available".into(),
                    true,
                    Instant::now(),
                ))
            }
        }
    }

    fn begin_edit_event(&mut self, event: Event) {
        if !self
            .calendar(&event.calendar_id)
            .is_some_and(|c| c.is_writable)
        {
            self.status = Some(("This calendar is read-only".into(), true, Instant::now()));
            return;
        }
        if event.has_recurrence {
            self.ensure_interaction_context(&event);
            self.pending_recurring_mutation = Some((event.id, RecurrenceMutationAction::Edit));
            self.enter_modal(Mode::RecurringEditScope);
            return;
        }
        self.ensure_interaction_context(&event);
        self.form = Some(EventForm::from_event(&event, &self.snapshot.calendars));
        self.form_dirty = false;
        self.mutation_state = MutationState::Idle;
        self.enter_modal(Mode::Form);
    }

    fn begin_duplicate_event(&mut self, event: Event) {
        if !self
            .calendar(&event.calendar_id)
            .is_some_and(|calendar| calendar.is_writable)
        {
            self.status = Some(("This calendar is read-only".into(), true, Instant::now()));
            return;
        }
        if AlarmEditorState::from_existing(&event.alarms).is_protected() {
            self.status = Some((
                "This event has custom/protected alarms and cannot be duplicated".into(),
                true,
                Instant::now(),
            ));
            return;
        }
        self.ensure_interaction_context(&event);
        self.form = Some(EventForm::duplicate_from(&event, &self.snapshot.calendars));
        self.form_dirty = false;
        self.mutation_state = MutationState::Idle;
        self.enter_modal(Mode::Form);
    }

    /// Starts a UI-only drag session. This records no mutation and makes no
    /// backend call; the event is checked again when the session is dropped.
    pub fn start_drag_session(&mut self, event_id: String, origin: CalendarHitTarget) -> bool {
        let matches_origin = matches!(
            &origin,
            CalendarHitTarget::ExistingEvent {
                event_id: origin_id
            } if origin_id == &event_id
        );
        if !matches_origin {
            self.set_drag_error(DragSessionError::InvalidOrigin);
            return false;
        }
        if self.drag_event(&event_id).is_err() {
            return false;
        }
        self.drag_session.start(event_id, origin);
        true
    }

    /// Updates the target shown by a future pointer adapter. Invalid targets
    /// remain visible as a dragging state, but can never produce a move intent.
    pub fn update_drag_preview(&mut self, target: CalendarHitTarget) -> bool {
        let Some(event_id) = self.drag_session.event_id.clone() else {
            self.set_drag_error(DragSessionError::NoActiveSession);
            return false;
        };
        let Ok(event) = self.drag_event(&event_id) else {
            return false;
        };
        let valid = Self::drag_move_target(&event, &target).is_ok();
        self.drag_session.update(target, valid);
        valid
    }

    /// Cancels a prospective drag without changing application or event state.
    pub fn cancel_drag_session(&mut self) {
        self.drag_session.cancel();
    }

    /// Returns render-only preview data for a current valid drag. This does
    /// not update session state, selection, event data, or backend state.
    pub fn drag_preview(&self) -> Option<DragPreview> {
        if self.drag_session.state != DragState::Preview {
            return None;
        }
        let event_id = self.drag_session.event_id.as_ref()?;
        let current_target = self.drag_session.current_target.clone()?;
        let event = self
            .visible_events()
            .into_iter()
            .find(|event| event.id == *event_id)?;
        if !self
            .calendar(&event.calendar_id)
            .is_some_and(|calendar| calendar.is_writable)
        {
            return None;
        }
        let target = Self::drag_move_target(event, &current_target).ok()?;
        match target {
            EventMoveTarget::Timed { start, end } => Some(DragPreview::Timed {
                event_id: event.id.clone(),
                title: event.title.clone(),
                original_start: event.start,
                original_end: event.end,
                proposed_start: start,
                proposed_end: end,
                current_target,
            }),
            EventMoveTarget::AllDay {
                start_date,
                end_date_exclusive,
            } => {
                let (original_start_date, original_end_date_exclusive) =
                    event.all_day_date_range()?;
                Some(DragPreview::AllDay {
                    event_id: event.id.clone(),
                    title: event.title.clone(),
                    original_start_date,
                    original_end_date_exclusive,
                    proposed_start_date: start_date,
                    proposed_end_date_exclusive: end_date_exclusive,
                    current_target,
                })
            }
        }
    }

    /// Turns the current valid preview into the existing move intent. This is
    /// intentionally separate from `execute_action`: callers decide when to
    /// dispatch the returned action through the normal workflow.
    pub fn drop_drag_session(&mut self) -> Option<UserAction> {
        let Some(event_id) = self.drag_session.event_id.clone() else {
            self.set_drag_error(DragSessionError::NoActiveSession);
            return None;
        };
        let Some(target) = self.drag_session.current_target.clone() else {
            self.set_drag_error(DragSessionError::InvalidTarget);
            return None;
        };
        let event = self.drag_event(&event_id).ok()?;
        let target = match Self::drag_move_target(&event, &target) {
            Ok(target) => target,
            Err(error) => {
                self.set_drag_error(error);
                return None;
            }
        };
        self.drag_session.state = DragState::Dropped;
        Some(UserAction::MoveEvent { event_id, target })
    }

    fn set_drag_error(&mut self, error: DragSessionError) {
        self.status = Some((error.message().into(), true, Instant::now()));
    }

    fn drag_event(&mut self, event_id: &str) -> Result<Event, DragSessionError> {
        let Some(event) = self
            .visible_events()
            .into_iter()
            .find(|event| event.id == event_id)
            .cloned()
        else {
            self.set_drag_error(DragSessionError::StaleEvent);
            return Err(DragSessionError::StaleEvent);
        };
        if !self
            .calendar(&event.calendar_id)
            .is_some_and(|calendar| calendar.is_writable)
        {
            self.set_drag_error(DragSessionError::ReadOnlyCalendar);
            return Err(DragSessionError::ReadOnlyCalendar);
        }
        if event.all_day && event.all_day_date_range().is_none() {
            self.set_drag_error(DragSessionError::LegacyAllDay);
            return Err(DragSessionError::LegacyAllDay);
        }
        Ok(event)
    }

    fn drag_move_target(
        event: &Event,
        hit_target: &CalendarHitTarget,
    ) -> Result<EventMoveTarget, DragSessionError> {
        if event.all_day {
            let date = match hit_target {
                CalendarHitTarget::AllDayRow { date }
                | CalendarHitTarget::EmptyCalendarCell { date } => *date,
                _ => return Err(DragSessionError::InvalidTarget),
            };
            let (old_start, old_end_exclusive) = event
                .all_day_date_range()
                .ok_or(DragSessionError::LegacyAllDay)?;
            let span = old_end_exclusive.signed_duration_since(old_start);
            let end_date_exclusive = date
                .checked_add_signed(span)
                .ok_or(DragSessionError::OutOfRange)?;
            return Ok(EventMoveTarget::AllDay {
                start_date: date,
                end_date_exclusive,
            });
        }

        let CalendarHitTarget::TimedSlot { date, minute } = hit_target else {
            return Err(DragSessionError::InvalidTarget);
        };
        let time = NaiveTime::from_num_seconds_from_midnight_opt(u32::from(*minute) * 60, 0)
            .ok_or(DragSessionError::InvalidTarget)?;
        let start = Local
            .from_local_datetime(&date.and_time(time))
            .single()
            .ok_or(DragSessionError::InvalidLocalTime)?
            .with_timezone(&Utc);
        Ok(EventMoveTarget::Timed {
            start,
            end: start + (event.end - event.start),
        })
    }

    fn move_selected_event(&mut self, day_delta: i64, minute_delta: i64) -> Option<WorkerCommand> {
        let Some(event) = self.selected_event_ref().cloned() else {
            self.status = Some(("Select an event first".into(), true, Instant::now()));
            return None;
        };
        let target = if event.all_day {
            let Some((start_date, end_date_exclusive)) = event.all_day_date_range() else {
                self.status = Some((
                    "Legacy all-day events must be refreshed before moving".into(),
                    true,
                    Instant::now(),
                ));
                return None;
            };
            let Some(start_date) = start_date.checked_add_signed(Duration::days(day_delta)) else {
                self.status = Some(("All-day move is out of range".into(), true, Instant::now()));
                return None;
            };
            let Some(end_date_exclusive) =
                end_date_exclusive.checked_add_signed(Duration::days(day_delta))
            else {
                self.status = Some(("All-day move is out of range".into(), true, Instant::now()));
                return None;
            };
            EventMoveTarget::AllDay {
                start_date,
                end_date_exclusive,
            }
        } else {
            let delta = Duration::days(day_delta) + Duration::minutes(minute_delta);
            EventMoveTarget::Timed {
                start: event.start + delta,
                end: event.end + delta,
            }
        };
        self.execute_action(UserAction::MoveEvent {
            event_id: event.id,
            target,
        })
    }

    fn resize_selected_event(&mut self, duration_delta: i64) -> Option<WorkerCommand> {
        let Some(event) = self.selected_event_ref().cloned() else {
            self.status = Some(("Select an event first".into(), true, Instant::now()));
            return None;
        };
        if event.has_recurrence || event.all_day {
            self.status = Some((
                "Use the event editor for recurring or all-day events".into(),
                true,
                Instant::now(),
            ));
            return None;
        }
        self.move_selected_event(0, 0)
            .and_then(|command| match command {
                WorkerCommand::Update(mut draft, span, scope, alarms, mutation) => {
                    let EventTimeInput::Timed { start, end } = draft.time else {
                        unreachable!("timed resize creates a timed movement request")
                    };
                    let end = end + Duration::minutes(duration_delta);
                    if end <= start {
                        self.status = Some((
                            "Event duration must remain positive".into(),
                            true,
                            Instant::now(),
                        ));
                        return None;
                    }
                    draft.time = EventTimeInput::timed(start, end)
                        .expect("positive resize remains a valid timed interval");
                    Some(WorkerCommand::Update(draft, span, scope, alarms, mutation))
                }
                _ => None,
            })
    }

    fn begin_move_event(
        &mut self,
        event: Event,
        target: EventMoveTarget,
        recurrence_scope: Option<RecurrenceMutationScope>,
    ) -> Option<WorkerCommand> {
        if !self
            .calendar(&event.calendar_id)
            .is_some_and(|calendar| calendar.is_writable)
        {
            self.status = Some(("This calendar is read-only".into(), true, Instant::now()));
            return None;
        }
        if event.has_recurrence && recurrence_scope.is_none() {
            self.pending_recurring_mutation =
                Some((event.id, RecurrenceMutationAction::Move(target)));
            self.enter_modal(Mode::RecurringEditScope);
            return None;
        }

        let (time, time_mutation) = match target {
            EventMoveTarget::Timed { start, end } => {
                if event.all_day {
                    self.status = Some((
                        "All-day events require a calendar-date move target".into(),
                        true,
                        Instant::now(),
                    ));
                    return None;
                }
                if end <= start || end - start != event.end - event.start {
                    self.status = Some((
                        "Timed moves must preserve event duration".into(),
                        true,
                        Instant::now(),
                    ));
                    return None;
                }
                (
                    EventTimeInput::timed(start, end)
                        .expect("validated timed movement has a positive duration"),
                    EventTimeMutation::ReplaceLegacy,
                )
            }
            EventMoveTarget::AllDay {
                start_date,
                end_date_exclusive,
            } => {
                let Some((old_start, old_end_exclusive)) = event.all_day_date_range() else {
                    self.status = Some((
                        "Legacy all-day events must be refreshed before moving".into(),
                        true,
                        Instant::now(),
                    ));
                    return None;
                };
                if end_date_exclusive <= start_date
                    || end_date_exclusive.signed_duration_since(start_date)
                        != old_end_exclusive.signed_duration_since(old_start)
                {
                    self.status = Some((
                        "All-day moves must preserve the calendar-date span".into(),
                        true,
                        Instant::now(),
                    ));
                    return None;
                }
                (
                    EventTimeInput::legacy_all_day_unknown(event.start, event.end)
                        .expect("provider event retains a positive compatibility interval"),
                    EventTimeMutation::ReplaceAllDay {
                        start_date,
                        end_date_exclusive,
                    },
                )
            }
        };
        let span = match recurrence_scope {
            Some(RecurrenceMutationScope::FutureEvents) => EventSpan::FutureEvents,
            _ => EventSpan::ThisEvent,
        };
        Some(WorkerCommand::Update(
            EventDraft {
                id: event.provider_id.clone(),
                occurrence_id: Some(event.id),
                occurrence_start: Some(event.start),
                occurrence_calendar_id: Some(event.calendar_id.clone()),
                calendar_id: event.calendar_id,
                title: event.title,
                time,
                location: event.location,
                notes: event.notes,
                url: event.url,
                time_zone: event.time_zone,
                availability: event.availability,
                attendees: vec![],
                alarms: event.alarms,
                recurrence: event.recurrence,
            },
            span,
            recurrence_scope,
            AlarmMutation::Preserve,
            time_mutation,
        ))
    }

    fn begin_delete_event(&mut self, event: Event) {
        if !self
            .calendar(&event.calendar_id)
            .is_some_and(|c| c.is_writable)
        {
            self.status = Some(("This calendar is read-only".into(), true, Instant::now()));
            return;
        }
        if event.has_recurrence {
            self.ensure_interaction_context(&event);
            self.pending_recurring_mutation = Some((event.id, RecurrenceMutationAction::Delete));
            self.enter_modal(Mode::RecurringDeleteScope);
            return;
        }
        self.ensure_interaction_context(&event);
        self.delete_span = EventSpan::ThisEvent;
        self.delete_recurrence_scope = None;
        self.pending_delete_event_id = Some(event.id.clone());
        self.enter_modal(Mode::Delete);
    }

    fn handle_recurring_scope(&mut self, key: KeyEvent) -> Option<WorkerCommand> {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.pending_recurring_mutation = None;
            self.leave_modal();
            if !matches!(self.mode, Mode::Details) {
                self.finish_interaction_return();
            }
            return None;
        }
        let span = match key.code {
            KeyCode::Char('1') => EventSpan::ThisEvent,
            KeyCode::Char('2') => EventSpan::FutureEvents,
            _ => return None,
        };
        let Some((event_id, action)) = self.pending_recurring_mutation.take() else {
            self.leave_modal();
            return None;
        };
        let Some(event) = self
            .visible_events()
            .into_iter()
            .find(|event| event.id == event_id)
            .cloned()
        else {
            self.status = Some(("Event no longer exists".into(), true, Instant::now()));
            self.leave_modal();
            return None;
        };
        match action {
            RecurrenceMutationAction::Edit => {
                let mut form = EventForm::from_event(&event, &self.snapshot.calendars);
                form.span = span;
                form.recurrence_scope = Some(match span {
                    EventSpan::ThisEvent => RecurrenceMutationScope::ThisEvent,
                    EventSpan::FutureEvents => RecurrenceMutationScope::FutureEvents,
                });
                self.form = Some(form);
                self.form_dirty = false;
                self.replace_modal(Mode::Form);
            }
            RecurrenceMutationAction::Delete => {
                self.delete_span = span;
                self.delete_recurrence_scope = Some(match span {
                    EventSpan::ThisEvent => RecurrenceMutationScope::ThisEvent,
                    EventSpan::FutureEvents => RecurrenceMutationScope::FutureEvents,
                });
                self.pending_delete_event_id = Some(event.id.clone());
                self.replace_modal(Mode::Delete);
            }
            RecurrenceMutationAction::Move(target) => {
                self.leave_modal();
                return self.begin_move_event(
                    event,
                    target,
                    Some(match span {
                        EventSpan::ThisEvent => RecurrenceMutationScope::ThisEvent,
                        EventSpan::FutureEvents => RecurrenceMutationScope::FutureEvents,
                    }),
                );
            }
        }
        None
    }

    fn open_detail_link(&mut self) -> Option<WorkerCommand> {
        let event = self.details_event_ref()?;
        let target = if !event.url.trim().is_empty() {
            event.url.clone()
        } else if !event.location.trim().is_empty() {
            format!(
                "https://maps.apple.com/?q={}",
                encode_url_component(&event.location)
            )
        } else {
            self.status = Some((
                "This event has no URL or location to open".into(),
                true,
                Instant::now(),
            ));
            return None;
        };
        Some(WorkerCommand::OpenUrl(target))
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.visible_events().len();
        if count == 0 {
            self.clear_event_selection();
        } else if self.selected_event >= count {
            self.selected_event = if delta < 0 { count - 1 } else { 0 };
        } else if delta < 0 {
            self.selected_event = self.selected_event.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected_event = (self.selected_event + delta as usize).min(count - 1);
        }
    }

    fn move_month_event_selection(&mut self, delta: isize) {
        let visible = self.visible_events();
        let indices = visible
            .iter()
            .enumerate()
            .filter_map(|(index, event)| event.occurs_on(self.active_date).then_some(index))
            .collect::<Vec<_>>();
        let Some(first) = indices.first().copied() else {
            self.clear_event_selection();
            self.status = Some(("No events on the active date".into(), false, Instant::now()));
            return;
        };
        let current = indices
            .iter()
            .position(|index| *index == self.selected_event);
        let selected = match current {
            Some(current) => {
                let count = indices.len() as isize;
                let next = (current as isize + delta).rem_euclid(count) as usize;
                indices[next]
            }
            None if delta < 0 => *indices.last().unwrap_or(&first),
            None => first,
        };
        self.selected_event = selected;
    }

    fn navigate_date(&mut self, direction: i32) {
        self.active_date += Duration::days(direction as i64);
        self.clear_event_selection();
        self.reset_timeline_viewport_if_needed();
    }

    fn navigate_period(&mut self, direction: i32) {
        self.active_date = match self.view {
            View::Month if direction < 0 => self
                .active_date
                .checked_sub_months(Months::new(1))
                .unwrap_or(self.active_date),
            View::Month => self
                .active_date
                .checked_add_months(Months::new(1))
                .unwrap_or(self.active_date),
            View::Week => self.active_date + Duration::days(7 * direction as i64),
            _ => self.active_date + Duration::days(direction as i64),
        };
        self.clear_event_selection();
        self.reset_timeline_viewport_if_needed();
    }

    fn set_view(&mut self, view: View) {
        let context = self.selection_context();
        self.view = view;
        self.restore_selection_context(context);
    }

    fn selected_event_action(
        &mut self,
        action: impl FnOnce(String) -> UserAction,
    ) -> Option<WorkerCommand> {
        let Some(event) = self.selected_event_ref() else {
            self.status = Some(("Select an event first".into(), true, Instant::now()));
            return None;
        };
        self.execute_action(action(event.id.clone()))
    }

    fn details_event_action(
        &mut self,
        action: impl FnOnce(String) -> UserAction,
    ) -> Option<WorkerCommand> {
        let Some(event) = self.details_event_ref().cloned() else {
            self.status = Some(("Event no longer exists".into(), true, Instant::now()));
            return None;
        };
        self.execute_action(action(event.id))
    }

    fn action_event(&mut self, id: &str) -> Option<Event> {
        let event = self
            .visible_events()
            .into_iter()
            .find(|event| event.id == id)
            .cloned();
        if event.is_none() {
            self.status = Some(("Event no longer exists".into(), true, Instant::now()));
        }
        event
    }

    pub fn execute_action(&mut self, action: UserAction) -> Option<WorkerCommand> {
        match action {
            UserAction::CreateEvent => self.begin_new_form(),
            UserAction::QuickAdd => self.begin_quick_add(),
            UserAction::EditEvent(id) => {
                let event = self.action_event(&id)?;
                self.begin_edit_event(event);
            }
            UserAction::DuplicateEvent(id) => {
                let event = self.action_event(&id)?;
                self.begin_duplicate_event(event);
            }
            UserAction::DeleteEvent(id) => {
                let event = self.action_event(&id)?;
                self.begin_delete_event(event);
            }
            UserAction::MoveEvent { event_id, target } => {
                let event = self.action_event(&event_id)?;
                return self.begin_move_event(event, target, None);
            }
            UserAction::Search => {
                self.search_query.clear();
                self.search_selected = 0;
                self.enter_modal(Mode::Search);
            }
            UserAction::OpenDateJump => {
                self.palette_query.clear();
                self.enter_modal(Mode::DateJump);
            }
            UserAction::GoToDate(date) => {
                self.active_date = date;
                self.clear_event_selection();
                self.reset_timeline_viewport_if_needed();
                self.close_all_modals();
                self.status = Some((
                    format!("Loading {}…", date.format("%B %Y")),
                    false,
                    Instant::now(),
                ));
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            UserAction::Today => {
                self.active_date = Local::now().date_naive();
                self.clear_event_selection();
                self.reset_timeline_viewport_if_needed();
                self.close_all_modals();
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            UserAction::ChangeView(view) => {
                self.set_view(view);
                self.close_all_modals();
                return Some(WorkerCommand::EnsureRange(self.visible_range_request()));
            }
            UserAction::Refresh => {
                self.close_all_modals();
                return Some(WorkerCommand::Refresh);
            }
            UserAction::RetryVisibleRange => {
                self.close_all_modals();
                return Some(WorkerCommand::RefreshRange(self.visible_range_request()));
            }
            UserAction::ToggleSidebar => {
                self.leave_modal();
                self.sidebar_visible = !self.sidebar_visible;
                self.mode = if self.sidebar_visible {
                    Mode::Calendars
                } else {
                    Mode::Normal
                };
            }
            UserAction::Undo => return self.execute_history(true),
            UserAction::Redo => return self.execute_history(false),
        }
        None
    }
    fn clamp_selection(&mut self) {
        if self.selected_event < self.visible_events().len() {
            return;
        }
        self.clear_event_selection();
    }

    pub fn scroll_timeline(&mut self, delta: i32) {
        let next = (i32::from(self.timeline_start_minute) + delta).clamp(0, 24 * 60 - 60);
        self.timeline_start_minute = next as u16;
        self.timeline_viewport_owner = TimelineViewportOwner::Manual;
    }

    fn reset_timeline_viewport_if_needed(&mut self) {
        if matches!(self.view, View::Day | View::Week) {
            self.timeline_viewport_owner = TimelineViewportOwner::Auto;
        }
    }

    /// Updates the automatic Day/Week viewport using the real number of
    /// timeline rows supplied by the renderer.  Callers must not use this for
    /// ordinary redraws when the user owns the viewport.
    pub fn refresh_auto_timeline_viewport(&mut self, rows: u16) {
        if self.timeline_viewport_owner == TimelineViewportOwner::Auto
            && matches!(self.view, View::Day | View::Week)
        {
            self.timeline_start_minute = self.smart_timeline_start_minute_at(rows, Local::now());
        }
    }

    /// Pure focus policy shared by Day and Week.  It bins normal timed events
    /// into the existing 30-minute display rows, ignoring all-day events and
    /// long background-style blocks (six hours or more).  The densest window
    /// wins; ties retain one hour of context before the first useful event.
    pub fn smart_timeline_start_minute_at(&self, rows: u16, now: DateTime<Local>) -> u16 {
        const ROW_MINUTES: u16 = 30;
        const LONG_EVENT_MINUTES: u16 = 6 * 60;
        const ROWS_PER_DAY: usize = (crate::layout::MINUTES_PER_DAY / ROW_MINUTES) as usize;

        if !matches!(self.view, View::Day | View::Week) || rows == 0 {
            return self.timeline_start_minute;
        }
        let visible_rows = rows.min(ROWS_PER_DAY as u16);
        let window_minutes = visible_rows.saturating_mul(ROW_MINUTES);
        let max_start = crate::layout::MINUTES_PER_DAY.saturating_sub(window_minutes);
        let (first_day, end_day) = self.view_date_range();
        let days = (end_day - first_day).num_days().max(0) as usize;
        let mut activity = [0_u16; ROWS_PER_DAY];
        let mut earliest = None;
        let mut nearby_today = None;
        let now_day = now.date_naive();
        let now_minute = (now.hour() * 60 + now.minute()) as u16;

        for event in self.visible_events() {
            if event.all_day {
                continue;
            }
            for offset in 0..days {
                let day = first_day + Duration::days(offset as i64);
                let Some(item) = crate::layout::item_for_day(0, event.start, event.end, false, day)
                else {
                    continue;
                };
                if item.end_minute.saturating_sub(item.start_minute) >= LONG_EVENT_MINUTES {
                    continue;
                }
                earliest = Some(
                    earliest.map_or(item.start_minute, |value: u16| value.min(item.start_minute)),
                );
                let first_row = usize::from(item.start_minute / ROW_MINUTES);
                let last_row = usize::from(item.end_minute.saturating_sub(1) / ROW_MINUTES);
                for row in activity
                    .iter_mut()
                    .take(last_row.min(ROWS_PER_DAY - 1) + 1)
                    .skip(first_row)
                {
                    *row = (*row).saturating_add(1);
                }
                if day == now_day
                    && item.start_minute <= now_minute.saturating_add(120)
                    && item.end_minute.saturating_add(120) >= now_minute
                {
                    nearby_today = Some(
                        nearby_today
                            .map_or(item.start_minute, |value: u16| value.min(item.start_minute)),
                    );
                }
            }
        }

        // Today keeps the clock in view when an appointment is active or
        // imminent.  Historical and future dates never consult the clock.
        if let Some(nearby) = nearby_today {
            let target = nearby.min(now_minute).saturating_sub(window_minutes / 3);
            return ((target / ROW_MINUTES) * ROW_MINUTES).min(max_start);
        }

        let Some(first_useful) = earliest else {
            return if self.active_date == now_day {
                now_minute.saturating_sub(window_minutes / 3).min(max_start)
            } else {
                (9 * 60).min(max_start)
            };
        };
        let preferred = first_useful.saturating_sub(60).min(max_start);
        let window_rows = usize::from(visible_rows);
        let max_row = usize::from(max_start / ROW_MINUTES);
        let mut best = (0_u16, u16::MAX, 0_u16);
        for start_row in 0..=max_row {
            let score = activity[start_row..(start_row + window_rows).min(ROWS_PER_DAY)]
                .iter()
                .copied()
                .sum::<u16>();
            let start = (start_row as u16) * ROW_MINUTES;
            let distance = start.abs_diff(preferred);
            // Maximize activity, then prefer useful context above the first
            // short appointment rather than an arbitrary midnight tie.
            if score > best.0 || (score == best.0 && distance < best.1) {
                best = (score, distance, start);
            }
        }
        best.2
    }

    pub fn suggested_start_minute(&self) -> u16 {
        self.selected_event_ref()
            .filter(|event| {
                !event.all_day && event.start.with_timezone(&Local).date_naive() == self.active_date
            })
            .map(|event| {
                let time = event.start.with_timezone(&Local);
                (time.hour() * 60 + time.minute()) as u16
            })
            .unwrap_or_else(|| {
                if self.active_date != Local::now().date_naive() {
                    parse_clock_minute(&self.config.event.default_start_time).unwrap_or(9 * 60)
                } else {
                    let now = Local::now();
                    round_minute(
                        (now.hour() * 60 + now.minute()) as u16,
                        self.config.event.time_rounding_minutes,
                    )
                }
            })
    }

    pub fn palette_entries(&self) -> Vec<PaletteEntry> {
        let query = self.palette_query.to_ascii_lowercase();
        PaletteCommand::ALL
            .into_iter()
            .filter(|command| query.is_empty() || command.matches_query(&query))
            .map(|command| PaletteEntry {
                command,
                enabled: self.palette_command_enabled(command).is_none(),
                unavailable_reason: self.palette_command_enabled(command),
            })
            .collect()
    }

    fn palette_command_enabled(&self, command: PaletteCommand) -> Option<&'static str> {
        match command {
            PaletteCommand::NewEvent | PaletteCommand::QuickAdd => self
                .snapshot
                .calendars
                .iter()
                .any(|calendar| calendar.is_writable)
                .then_some(())
                .is_none()
                .then_some("No writable calendars are available"),
            PaletteCommand::EditEvent | PaletteCommand::DeleteEvent => {
                self.selected_event_mutation_unavailable_reason()
            }
            PaletteCommand::DuplicateEvent => self
                .selected_event_mutation_unavailable_reason()
                .or_else(|| {
                    self.selected_event_ref()
                        .is_some_and(|event| {
                            AlarmEditorState::from_existing(&event.alarms).is_protected()
                        })
                        .then_some(
                            "This event has custom/protected alarms and cannot be duplicated",
                        )
                }),
            PaletteCommand::Undo => self.history_available_reason(true),
            PaletteCommand::Redo => self.history_available_reason(false),
            _ => None,
        }
    }

    fn selected_event_mutation_unavailable_reason(&self) -> Option<&'static str> {
        let Some(event) = self.selected_event_ref() else {
            return Some("Select an event first");
        };
        (!self
            .calendar(&event.calendar_id)
            .is_some_and(|calendar| calendar.is_writable))
        .then_some("This calendar is read-only")
    }

    fn execute_palette(&mut self, command: PaletteCommand) -> Option<WorkerCommand> {
        let action = match command {
            PaletteCommand::Today => UserAction::Today,
            PaletteCommand::GoToDate => UserAction::OpenDateJump,
            PaletteCommand::NewEvent => UserAction::CreateEvent,
            PaletteCommand::QuickAdd => UserAction::QuickAdd,
            PaletteCommand::EditEvent => return self.selected_event_action(UserAction::EditEvent),
            PaletteCommand::DuplicateEvent => {
                return self.selected_event_action(UserAction::DuplicateEvent);
            }
            PaletteCommand::DeleteEvent => {
                return self.selected_event_action(UserAction::DeleteEvent);
            }
            PaletteCommand::Undo => UserAction::Undo,
            PaletteCommand::Redo => UserAction::Redo,
            PaletteCommand::Search => UserAction::Search,
            PaletteCommand::Refresh => UserAction::Refresh,
            PaletteCommand::RetryVisibleRange => UserAction::RetryVisibleRange,
            PaletteCommand::ToggleSidebar => UserAction::ToggleSidebar,
            PaletteCommand::Day => UserAction::ChangeView(View::Day),
            PaletteCommand::Week => UserAction::ChangeView(View::Week),
            PaletteCommand::Month => UserAction::ChangeView(View::Month),
            PaletteCommand::Agenda => UserAction::ChangeView(View::Agenda),
        };
        self.execute_action(action)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    Today,
    GoToDate,
    NewEvent,
    QuickAdd,
    EditEvent,
    DuplicateEvent,
    DeleteEvent,
    Undo,
    Redo,
    Search,
    Refresh,
    RetryVisibleRange,
    ToggleSidebar,
    Day,
    Week,
    Month,
    Agenda,
}

impl PaletteCommand {
    pub const ALL: [Self; 17] = [
        Self::Today,
        Self::GoToDate,
        Self::NewEvent,
        Self::QuickAdd,
        Self::EditEvent,
        Self::DuplicateEvent,
        Self::DeleteEvent,
        Self::Undo,
        Self::Redo,
        Self::Search,
        Self::Refresh,
        Self::RetryVisibleRange,
        Self::ToggleSidebar,
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Agenda,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "Go to today",
            Self::GoToDate => "Go to date",
            Self::NewEvent => "New event",
            Self::QuickAdd => "Quick Add Event",
            Self::EditEvent => "Edit selected event",
            Self::DuplicateEvent => "Duplicate selected event",
            Self::DeleteEvent => "Delete selected event",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Search => "Search events",
            Self::Refresh => "Refresh calendars",
            Self::RetryVisibleRange => "Retry visible range",
            Self::ToggleSidebar => "Toggle calendar sidebar",
            Self::Day => "Day view",
            Self::Week => "Week view",
            Self::Month => "Month view",
            Self::Agenda => "Agenda view",
        }
    }

    /// The displayed equivalent keybinding, when the command has one. This
    /// small metadata source is shared by palette presentation and Help so a
    /// renamed command cannot quietly drift from its discovery affordances.
    pub fn key_hint(self) -> &'static str {
        match self {
            Self::Today => "gg / t",
            Self::GoToDate => "",
            Self::NewEvent => "n",
            Self::QuickAdd => "a",
            Self::EditEvent => "e",
            Self::DuplicateEvent => "D",
            Self::DeleteEvent => "d",
            Self::Undo => "u",
            Self::Redo => "Ctrl-R",
            Self::Search => "/",
            Self::Refresh => "r",
            Self::RetryVisibleRange => "R",
            Self::ToggleSidebar => "c",
            Self::Day => "gd",
            Self::Week => "gw",
            Self::Month => "gm",
            Self::Agenda => "ga",
        }
    }

    fn matches_query(self, query: &str) -> bool {
        self.label().to_ascii_lowercase().contains(query)
            || match self {
                Self::Today => "today",
                Self::GoToDate => "jump date",
                Self::NewEvent => "create event",
                Self::QuickAdd => "add event",
                Self::EditEvent => "edit",
                Self::DuplicateEvent => "duplicate copy",
                Self::DeleteEvent => "delete remove",
                Self::Undo => "undo revert",
                Self::Redo => "redo repeat",
                Self::Search => "find search",
                Self::Refresh => "reload refresh",
                Self::RetryVisibleRange => "retry range",
                Self::ToggleSidebar => "calendar sidebar",
                Self::Day => "view day",
                Self::Week => "view week",
                Self::Month => "view month",
                Self::Agenda => "view agenda",
            }
            .contains(query)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEntry {
    pub command: PaletteCommand,
    pub enabled: bool,
    unavailable_reason: Option<&'static str>,
}

impl PaletteEntry {
    pub fn unavailable_reason(self) -> &'static str {
        self.unavailable_reason
            .unwrap_or("This command is unavailable")
    }
}

pub fn parse_date_jump(input: &str) -> Option<NaiveDate> {
    let value = input.trim();
    let today = Local::now().date_naive();
    match value.to_ascii_lowercase().as_str() {
        "today" => Some(today),
        "tomorrow" => Some(today + Duration::days(1)),
        "yesterday" => Some(today - Duration::days(1)),
        _ => NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .or_else(|| NaiveDate::parse_from_str(value, "%d.%m.%Y").ok())
            .or_else(|| {
                NaiveDate::parse_from_str(&format!("{value} {}", today.year()), "%b %d %Y").ok()
            }),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub terms: Vec<String>,
    /// Stable calendar ID, never a display title.
    pub calendar: Option<String>,
    pub location: Option<String>,
    pub attendee: Option<String>,
    pub recurring: Option<bool>,
    pub all_day: Option<bool>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

impl SearchQuery {
    pub fn parse(input: &str) -> Self {
        let mut query = Self::default();
        for token in search_tokens(input) {
            let lower = normalized_search_text(&token);
            if let Some(value) = lower.strip_prefix("/calendar:") {
                if !value.is_empty() {
                    query.calendar = Some(value.into());
                }
            } else if let Some(value) = lower.strip_prefix("/location:") {
                if !value.is_empty() {
                    query.location = Some(value.into());
                }
            } else if let Some(value) = lower.strip_prefix("/attendee:") {
                if !value.is_empty() {
                    query.attendee = Some(value.into());
                }
            } else if let Some(value) = lower.strip_prefix("/recurring:") {
                query.recurring = parse_search_bool(value);
            } else if let Some(value) = lower.strip_prefix("/all-day:") {
                query.all_day = parse_search_bool(value);
            } else if let Some(value) = token.strip_prefix("/from:") {
                query.from = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok();
            } else if let Some(value) = token.strip_prefix("/to:") {
                query.to = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok();
            } else if !token.starts_with('/') {
                query.terms.push(lower);
            }
        }
        query
    }

    fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.calendar.is_none()
            && self.location.is_none()
            && self.attendee.is_none()
            && self.recurring.is_none()
            && self.all_day.is_none()
            && self.from.is_none()
            && self.to.is_none()
    }
}

fn parse_search_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn normalized_search_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn search_rank(
    event: &Event,
    query: &SearchQuery,
    active_date: NaiveDate,
) -> (u8, i64, NaiveDate, String) {
    let text = query.terms.join(" ");
    let title = normalized_search_text(&event.title);
    let field_rank = if text.is_empty() {
        4
    } else if title == text {
        0
    } else if title.starts_with(&text) {
        1
    } else if title.contains(&text) {
        2
    } else {
        3
    };
    (
        field_rank,
        (event.display_start_date() - active_date).num_days().abs(),
        event.display_start_date(),
        event.id.clone(),
    )
}

fn search_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in input.chars() {
        match character {
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_clock_minute(value: &str) -> Option<u16> {
    let time = chrono::NaiveTime::parse_from_str(value, "%H:%M").ok()?;
    Some((time.hour() * 60 + time.minute()) as u16)
}

fn round_minute(minute: u16, step: u16) -> u16 {
    let step = step.max(1);
    (((minute + step / 2) / step) * step).min(23 * 60 + 59)
}

fn encode_url_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn local_midnight(date: NaiveDate) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap()
        .with_timezone(&Utc)
}

/// Date filters retain their legacy start-date behavior for timed and old
/// all-day payloads. Trusted all-day metadata instead matches the event's
/// calendar-date coverage, without deriving a date through `Local`.
fn event_matches_search_dates(
    event: &Event,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    legacy_start_date: NaiveDate,
) -> bool {
    if let Some((start, end_exclusive)) = event.all_day_date_range() {
        return match (from, to) {
            (Some(from), Some(to)) => {
                let query_end_exclusive = to.checked_add_signed(Duration::days(1)).unwrap_or(to);
                start < query_end_exclusive && end_exclusive > from
            }
            (Some(from), None) => end_exclusive > from,
            (None, Some(to)) => start <= to,
            (None, None) => true,
        };
    }
    from.is_none_or(|date| legacy_start_date >= date)
        && to.is_none_or(|date| legacy_start_date <= date)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Title,
    Calendar,
    Start,
    End,
    AllDay,
    Location,
    Notes,
    Url,
    Alarms,
    Recurrence,
    RecurrenceInterval,
    Weekdays,
    RecurrenceEnds,
    RecurrenceEndDate,
    RecurrenceOccurrences,
    TimeZone,
    Availability,
    Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarFormField {
    Title,
    Color,
    Source,
}

impl CalendarFormField {
    pub const ALL: [Self; 3] = [Self::Title, Self::Color, Self::Source];

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Color => "Color",
            Self::Source => "Source",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CalendarForm {
    pub title: String,
    pub color: String,
    pub source_index: Option<usize>,
    pub selected: usize,
}

impl Default for CalendarForm {
    fn default() -> Self {
        Self::new()
    }
}

impl CalendarForm {
    pub fn new() -> Self {
        Self {
            title: String::new(),
            color: "#5E9EFF".into(),
            // A source is deliberately not preselected: a calendar must be
            // created in an explicitly chosen stable source ID.
            source_index: None,
            selected: 0,
        }
    }

    pub fn field(&self) -> CalendarFormField {
        CalendarFormField::ALL[self.selected]
    }

    pub fn next_field(&mut self) {
        self.selected = (self.selected + 1) % CalendarFormField::ALL.len();
    }

    pub fn previous_field(&mut self) {
        self.selected =
            (self.selected + CalendarFormField::ALL.len() - 1) % CalendarFormField::ALL.len();
    }

    pub fn is_choice(&self) -> bool {
        self.field() == CalendarFormField::Source
    }

    pub fn value(&self, sources: &[CalendarSource]) -> String {
        match self.field() {
            CalendarFormField::Title => self.title.clone(),
            CalendarFormField::Color => self.color.clone(),
            CalendarFormField::Source => self
                .source_index
                .and_then(|index| sources.get(index))
                .map(|source| format!("{} ({})", source.title, source.source_type))
                .unwrap_or_else(|| "Select a source".into()),
        }
    }

    pub fn push(&mut self, character: char) {
        match self.field() {
            CalendarFormField::Title => self.title.push(character),
            CalendarFormField::Color => self.color.push(character),
            CalendarFormField::Source => {}
        }
    }

    pub fn backspace(&mut self) {
        match self.field() {
            CalendarFormField::Title => {
                self.title.pop();
            }
            CalendarFormField::Color => {
                self.color.pop();
            }
            CalendarFormField::Source => {}
        }
    }

    pub fn adjust(&mut self, direction: i32, sources: &[CalendarSource]) {
        if !self.is_choice() || sources.is_empty() {
            return;
        }
        let current = self
            .source_index
            .unwrap_or_else(|| if direction < 0 { sources.len() - 1 } else { 0 });
        self.source_index = Some(if direction < 0 {
            (current + sources.len() - 1) % sources.len()
        } else if self.source_index.is_none() {
            current
        } else {
            (current + 1) % sources.len()
        });
    }

    pub fn to_request(&self, sources: &[CalendarSource]) -> Result<CreateCalendarRequest, String> {
        if self.title.trim().is_empty() {
            return Err("Calendar title is required".into());
        }
        if self.color.len() != 7
            || !self.color.starts_with('#')
            || u32::from_str_radix(&self.color[1..], 16).is_err()
        {
            return Err("Calendar color must use #RRGGBB".into());
        }
        let source = self
            .source_index
            .and_then(|index| sources.get(index))
            .ok_or("Select a calendar source")?;
        Ok(CreateCalendarRequest {
            title: self.title.trim().into(),
            color: self.color.clone(),
            source_id: source.id.clone(),
        })
    }
}

fn calendar_error_message(error: CalendarError) -> &'static str {
    match error {
        CalendarError::InvalidTitle => "Calendar title is required",
        CalendarError::InvalidColor => "Calendar color must use #RRGGBB",
        CalendarError::SourceNotFound => "Selected calendar source was not found",
        CalendarError::PermissionDenied => "Calendar access was denied",
        CalendarError::Unsupported => "Calendar creation is not supported",
        CalendarError::SourceUnavailable => "Selected calendar source is unavailable",
        _ => "Calendar could not be created",
    }
}

#[derive(Debug, Clone)]
pub struct CalendarRenameForm {
    pub calendar_id: String,
    pub title: String,
}

impl CalendarRenameForm {
    pub fn new(calendar: &CalendarInfo) -> Self {
        Self {
            calendar_id: calendar.id.clone(),
            title: calendar.title.clone(),
        }
    }

    pub fn to_request(&self) -> Result<RenameCalendarRequest, String> {
        if self.title.trim().is_empty() {
            return Err("Calendar title is required".into());
        }
        Ok(RenameCalendarRequest {
            calendar_id: self.calendar_id.clone(),
            title: self.title.trim().into(),
        })
    }
}

fn calendar_rename_error_message(error: CalendarError) -> &'static str {
    match error {
        CalendarError::InvalidTitle => "Calendar title is required",
        CalendarError::NotFound => "Calendar was not found",
        CalendarError::PermissionDenied => "Calendar access was denied",
        CalendarError::CannotModifyMetadata => "This calendar's metadata cannot be modified",
        CalendarError::Unsupported => "Calendar renaming is not supported",
        _ => "Calendar could not be renamed",
    }
}

#[derive(Debug, Clone)]
pub struct CalendarColorForm {
    pub calendar_id: String,
    pub color: String,
}

impl CalendarColorForm {
    pub fn new(calendar: &CalendarInfo) -> Self {
        Self {
            calendar_id: calendar.id.clone(),
            color: calendar.color.clone(),
        }
    }

    pub fn to_request(&self) -> Result<SetCalendarColorRequest, String> {
        if self.color.len() != 7
            || !self.color.starts_with('#')
            || u32::from_str_radix(&self.color[1..], 16).is_err()
        {
            return Err("Calendar color must use #RRGGBB".into());
        }
        Ok(SetCalendarColorRequest {
            calendar_id: self.calendar_id.clone(),
            color: self.color.to_ascii_uppercase(),
        })
    }
}

fn calendar_color_error_message(error: CalendarError) -> &'static str {
    match error {
        CalendarError::InvalidColor => "Calendar color must use #RRGGBB",
        CalendarError::NotFound => "Calendar was not found",
        CalendarError::PermissionDenied => "Calendar access was denied",
        CalendarError::CannotModifyMetadata => "Calendar metadata is read-only",
        CalendarError::Unsupported => "Calendar color changes are not supported",
        _ => "Calendar color could not be updated",
    }
}

fn calendar_delete_error_message(error: CalendarError) -> &'static str {
    match error {
        CalendarError::NotFound => "Calendar no longer exists",
        CalendarError::PermissionDenied | CalendarError::CannotDelete | CalendarError::ReadOnly => {
            "Calendar deletion is not permitted"
        }
        CalendarError::Unsupported => "Calendar deletion is not supported",
        _ => "Calendar could not be deleted",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarDeleteConfirmation {
    pub calendar_id: String,
    pub title: String,
}

/// One editor serves every mutation flow. Keeping its origin explicit avoids
/// treating a duplicate as an ordinary create merely because it has no ID yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorMode {
    Create,
    Edit { event_id: String },
    Duplicate { source_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlarmEditorState {
    NoAlarms { data: EditableAlarmData },
    EditableBasic { data: EditableAlarmData },
    ProtectedExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditableAlarm {
    /// The signed offset follows `EKAlarm.relativeOffset`: zero is at event
    /// time and negative values are before it.
    Relative { relative_seconds: i64 },
    /// A canonical instant. Future UI work must separately choose how to
    /// collect a wall-clock value and resolve DST; this model never does so.
    Absolute { date_time: DateTime<Utc> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmValidationError {
    InvalidPayload,
    UnsupportedRelativeOffset,
    InvalidDate,
    InvalidTime,
    InvalidTimeZone,
    NonexistentLocalTime,
    AmbiguousLocalTime,
}

impl fmt::Display for AlarmValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPayload => write!(formatter, "Invalid alarm payload"),
            Self::UnsupportedRelativeOffset => {
                write!(formatter, "Relative alarms must be at or before the event")
            }
            Self::InvalidDate => write!(formatter, "Date must use YYYY-MM-DD"),
            Self::InvalidTime => write!(formatter, "Time must use HH:MM"),
            Self::InvalidTimeZone => write!(formatter, "Event time zone is invalid"),
            Self::NonexistentLocalTime => write!(formatter, "This local time does not exist"),
            Self::AmbiguousLocalTime => write!(formatter, "This local time is ambiguous"),
        }
    }
}

impl EditableAlarm {
    fn from_alarm(alarm: &Alarm) -> Result<Self, AlarmValidationError> {
        if !alarm.is_editable {
            return Err(AlarmValidationError::InvalidPayload);
        }
        match (alarm.relative_seconds, alarm.absolute_date) {
            (Some(relative_seconds), None)
                if relative_seconds <= 0 && relative_seconds % 60 == 0 =>
            {
                Ok(Self::Relative { relative_seconds })
            }
            (Some(_), None) => Err(AlarmValidationError::UnsupportedRelativeOffset),
            (None, Some(date_time)) => Ok(Self::Absolute { date_time }),
            _ => Err(AlarmValidationError::InvalidPayload),
        }
    }

    fn to_alarm(&self) -> Result<Alarm, AlarmValidationError> {
        match self {
            Self::Relative { relative_seconds } if *relative_seconds <= 0 => Ok(Alarm {
                relative_seconds: Some(*relative_seconds),
                absolute_date: None,
                is_editable: true,
            }),
            Self::Relative { .. } => Err(AlarmValidationError::UnsupportedRelativeOffset),
            Self::Absolute { date_time } => Ok(Alarm {
                relative_seconds: None,
                absolute_date: Some(*date_time),
                is_editable: true,
            }),
        }
    }

    fn display_in_timezone(&self, time_zone: &str) -> String {
        match self {
            Self::Relative { relative_seconds } => relative_alarm_label(*relative_seconds),
            Self::Absolute { date_time } => format_absolute_alarm(*date_time, time_zone),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableAlarmData {
    original: Vec<Alarm>,
    alarms: Vec<EditableAlarm>,
    selected: usize,
    interaction: AlarmInteraction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AlarmInteraction {
    Browse,
    Preset {
        selected: usize,
    },
    Custom {
        buffer: String,
    },
    Edit {
        buffer: String,
    },
    AbsoluteEdit {
        date_buffer: String,
        time_buffer: String,
        field: AbsoluteAlarmField,
    },
    AbsoluteAmbiguous {
        first: DateTime<Utc>,
        second: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbsoluteAlarmField {
    Date,
    Time,
}

const ALARM_PRESETS: [Option<i64>; 9] = [
    Some(0),
    Some(-5 * 60),
    Some(-10 * 60),
    Some(-15 * 60),
    Some(-30 * 60),
    Some(-60 * 60),
    Some(-2 * 60 * 60),
    Some(-24 * 60 * 60),
    None,
];

const ALARM_PRESET_LABELS: [&str; 9] = [
    "At event time",
    "5 minutes before",
    "10 minutes before",
    "15 minutes before",
    "30 minutes before",
    "1 hour before",
    "2 hours before",
    "1 day before",
    "Custom",
];

impl AlarmEditorState {
    fn new_event() -> Self {
        Self::EditableBasic {
            data: EditableAlarmData {
                original: vec![],
                alarms: vec![EditableAlarm::Relative {
                    relative_seconds: -15 * 60,
                }],
                selected: 0,
                interaction: AlarmInteraction::Browse,
            },
        }
    }

    fn from_existing(alarms: &[Alarm]) -> Self {
        if alarms.is_empty() {
            Self::NoAlarms {
                data: EditableAlarmData {
                    original: vec![],
                    alarms: vec![],
                    selected: 0,
                    interaction: AlarmInteraction::Browse,
                },
            }
        } else if let Ok(entries) = alarms
            .iter()
            .map(EditableAlarm::from_alarm)
            .collect::<Result<Vec<_>, _>>()
        {
            Self::EditableBasic {
                data: EditableAlarmData {
                    original: alarms.to_vec(),
                    alarms: entries,
                    selected: 0,
                    interaction: AlarmInteraction::Browse,
                },
            }
        } else {
            Self::ProtectedExisting
        }
    }

    fn from_event_alarms(alarms: &[Alarm], all_day: bool, provenance: TimeZoneProvenance) -> Self {
        if all_day
            && (alarms.iter().any(|alarm| alarm.relative_seconds.is_some())
                || (alarms.iter().any(|alarm| alarm.absolute_date.is_some())
                    && provenance != TimeZoneProvenance::ExplicitEvent))
        {
            Self::ProtectedExisting
        } else {
            Self::from_existing(alarms)
        }
    }

    fn display_in_timezone(&self, time_zone: &str) -> String {
        match self {
            Self::NoAlarms { data } | Self::EditableBasic { data } => data.display(time_zone),
            Self::ProtectedExisting => "Custom / protected alarm".into(),
        }
    }

    /// Handles only keys that belong to the structured alarm manager. `None`
    /// lets the surrounding form keep its normal navigation and escape rules.
    fn handle_key(&mut self, key: KeyEvent, time_zone: &str) -> Option<Result<bool, String>> {
        let data = match self {
            Self::NoAlarms { data } | Self::EditableBasic { data } => data,
            Self::ProtectedExisting => return None,
        };
        match &mut data.interaction {
            AlarmInteraction::Browse => match key.code {
                KeyCode::Char('a') => {
                    data.interaction = AlarmInteraction::Preset { selected: 0 };
                    Some(Ok(false))
                }
                KeyCode::Char('d') if !data.alarms.is_empty() => {
                    data.alarms.remove(data.selected);
                    data.selected = data.selected.min(data.alarms.len().saturating_sub(1));
                    Some(Ok(true))
                }
                KeyCode::Enter if !data.alarms.is_empty() => match data.alarms[data.selected] {
                    EditableAlarm::Relative { relative_seconds } => {
                        data.interaction = AlarmInteraction::Edit {
                            buffer: relative_duration_text(relative_seconds),
                        };
                        Some(Ok(false))
                    }
                    EditableAlarm::Absolute { date_time } => {
                        match absolute_edit_buffers(date_time, time_zone) {
                            Ok((date_buffer, time_buffer)) => {
                                data.interaction = AlarmInteraction::AbsoluteEdit {
                                    date_buffer,
                                    time_buffer,
                                    field: AbsoluteAlarmField::Date,
                                };
                                Some(Ok(false))
                            }
                            Err(error) => Some(Err(error.to_string())),
                        }
                    }
                },
                KeyCode::Char('j') | KeyCode::Down if !data.alarms.is_empty() => {
                    data.selected = (data.selected + 1) % data.alarms.len();
                    Some(Ok(false))
                }
                KeyCode::Char('k') | KeyCode::Up if !data.alarms.is_empty() => {
                    data.selected = (data.selected + data.alarms.len() - 1) % data.alarms.len();
                    Some(Ok(false))
                }
                _ => None,
            },
            AlarmInteraction::Preset { selected } => match key.code {
                KeyCode::Esc => {
                    data.interaction = AlarmInteraction::Browse;
                    Some(Ok(false))
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    *selected = (*selected + 1) % ALARM_PRESETS.len();
                    Some(Ok(false))
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    *selected = (*selected + ALARM_PRESETS.len() - 1) % ALARM_PRESETS.len();
                    Some(Ok(false))
                }
                KeyCode::Enter => match ALARM_PRESETS[*selected] {
                    Some(relative_seconds) => {
                        data.alarms
                            .push(EditableAlarm::Relative { relative_seconds });
                        data.selected = data.alarms.len() - 1;
                        data.interaction = AlarmInteraction::Browse;
                        Some(Ok(true))
                    }
                    None => {
                        data.interaction = AlarmInteraction::Custom {
                            buffer: String::new(),
                        };
                        Some(Ok(false))
                    }
                },
                _ => Some(Ok(false)),
            },
            AlarmInteraction::Custom { buffer } | AlarmInteraction::Edit { buffer } => {
                match key.code {
                    KeyCode::Esc => {
                        data.interaction = AlarmInteraction::Browse;
                        Some(Ok(false))
                    }
                    KeyCode::Backspace => Some(Ok(buffer.pop().is_some())),
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buffer.push(character);
                        Some(Ok(true))
                    }
                    KeyCode::Enter => {
                        let relative_seconds = match parse_relative_alarm_duration(buffer) {
                            Ok(seconds) => seconds,
                            Err(error) => return Some(Err(error)),
                        };
                        let editing = matches!(data.interaction, AlarmInteraction::Edit { .. });
                        if editing {
                            data.alarms[data.selected] =
                                EditableAlarm::Relative { relative_seconds };
                        } else {
                            data.alarms
                                .push(EditableAlarm::Relative { relative_seconds });
                            data.selected = data.alarms.len() - 1;
                        }
                        data.interaction = AlarmInteraction::Browse;
                        Some(Ok(true))
                    }
                    _ => Some(Ok(false)),
                }
            }
            AlarmInteraction::AbsoluteEdit {
                date_buffer,
                time_buffer,
                field,
            } => match key.code {
                KeyCode::Esc => {
                    data.interaction = AlarmInteraction::Browse;
                    Some(Ok(false))
                }
                KeyCode::Tab => {
                    *field = match *field {
                        AbsoluteAlarmField::Date => AbsoluteAlarmField::Time,
                        AbsoluteAlarmField::Time => AbsoluteAlarmField::Date,
                    };
                    Some(Ok(false))
                }
                KeyCode::Backspace => Some(Ok(match field {
                    AbsoluteAlarmField::Date => date_buffer.pop().is_some(),
                    AbsoluteAlarmField::Time => time_buffer.pop().is_some(),
                })),
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match field {
                        AbsoluteAlarmField::Date => date_buffer.push(character),
                        AbsoluteAlarmField::Time => time_buffer.push(character),
                    };
                    Some(Ok(true))
                }
                KeyCode::Enter => match resolve_absolute_alarm(date_buffer, time_buffer, time_zone)
                {
                    Ok(AbsoluteResolution::Single(date_time)) => {
                        data.alarms[data.selected] = EditableAlarm::Absolute { date_time };
                        data.interaction = AlarmInteraction::Browse;
                        Some(Ok(true))
                    }
                    Ok(AbsoluteResolution::Ambiguous(first, second)) => {
                        data.interaction = AlarmInteraction::AbsoluteAmbiguous { first, second };
                        Some(Ok(false))
                    }
                    Err(error) => Some(Err(error.to_string())),
                },
                _ => Some(Ok(false)),
            },
            AlarmInteraction::AbsoluteAmbiguous { first, second } => match key.code {
                KeyCode::Esc => {
                    data.interaction = AlarmInteraction::Browse;
                    Some(Ok(false))
                }
                KeyCode::Char('1') => {
                    data.alarms[data.selected] = EditableAlarm::Absolute { date_time: *first };
                    data.interaction = AlarmInteraction::Browse;
                    Some(Ok(true))
                }
                KeyCode::Char('2') => {
                    data.alarms[data.selected] = EditableAlarm::Absolute { date_time: *second };
                    data.interaction = AlarmInteraction::Browse;
                    Some(Ok(true))
                }
                _ => Some(Ok(false)),
            },
        }
    }

    fn draft_alarms_and_mutation(
        &self,
        updating: bool,
    ) -> Result<(Vec<Alarm>, AlarmMutation), String> {
        match self {
            Self::ProtectedExisting if updating => Ok((vec![], AlarmMutation::Preserve)),
            Self::ProtectedExisting => Err("Custom / protected alarms cannot be duplicated".into()),
            Self::NoAlarms { data } | Self::EditableBasic { data } => {
                if !matches!(data.interaction, AlarmInteraction::Browse) {
                    return Err("Finish or cancel alarm editing before saving".into());
                }
                let alarms = data.to_alarms().map_err(|error| error.to_string())?;
                validate_replacement_alarms(&alarms)?;
                Ok((
                    alarms.clone(),
                    if updating && alarms == data.original {
                        AlarmMutation::Preserve
                    } else {
                        AlarmMutation::Replace(alarms)
                    },
                ))
            }
        }
    }

    fn is_protected(&self) -> bool {
        matches!(self, Self::ProtectedExisting)
    }

    fn allows_all_day_actions(&self) -> bool {
        matches!(self, Self::EditableBasic { data } if !data.alarms.is_empty() && data.alarms.iter().all(|alarm| matches!(alarm, EditableAlarm::Absolute { .. })))
    }
}

impl EditableAlarmData {
    fn to_alarms(&self) -> Result<Vec<Alarm>, AlarmValidationError> {
        self.alarms.iter().map(EditableAlarm::to_alarm).collect()
    }

    fn display(&self, time_zone: &str) -> String {
        match &self.interaction {
            AlarmInteraction::Browse if self.alarms.is_empty() => "None · a add alarm".into(),
            AlarmInteraction::Browse => self
                .alarms
                .iter()
                .enumerate()
                .map(|(index, alarm)| {
                    format!(
                        "{}{}",
                        if index == self.selected { "> " } else { "" },
                        alarm.display_in_timezone(time_zone)
                    )
                })
                .collect::<Vec<_>>()
                .join(" · "),
            AlarmInteraction::Preset { selected } => format!(
                "Add: {} (j/k choose · Enter select · Esc cancel)",
                ALARM_PRESET_LABELS[*selected]
            ),
            AlarmInteraction::Custom { buffer } => {
                format!("Custom: {buffer} (5m, 2h, 1d · Enter add · Esc cancel)")
            }
            AlarmInteraction::Edit { buffer } => {
                format!("Edit: {buffer} (5m, 2h, 1d · Enter save · Esc cancel)")
            }
            AlarmInteraction::AbsoluteEdit {
                date_buffer,
                time_buffer,
                field,
            } => format!(
                "Absolute {}: {} {} (Tab field · Enter save · Esc cancel)",
                if *field == AbsoluteAlarmField::Date {
                    "date"
                } else {
                    "time"
                },
                date_buffer,
                time_buffer
            ),
            AlarmInteraction::AbsoluteAmbiguous { first, second } => format!(
                "Ambiguous local time: 1 {} · 2 {} · Esc cancel",
                first.format("%z"),
                second.format("%z")
            ),
        }
    }
}

fn parse_relative_alarm_duration(text: &str) -> Result<i64, String> {
    let alarm = parse_alarms(text)?;
    match alarm.as_slice() {
        [
            Alarm {
                relative_seconds: Some(seconds),
                absolute_date: None,
                ..
            },
        ] if *seconds <= 0 => Ok(*seconds),
        _ => Err("Custom alarm must be one relative duration such as 5m, 2h, or 1d".into()),
    }
}

fn relative_duration_text(relative_seconds: i64) -> String {
    let seconds = relative_seconds.unsigned_abs();
    if seconds == 0 {
        "0m".into()
    } else if seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}m", seconds / 60)
    }
}

fn relative_alarm_label(relative_seconds: i64) -> String {
    if relative_seconds == 0 {
        "At event time".into()
    } else {
        let duration = relative_duration_text(relative_seconds);
        format!("{duration} before")
    }
}

/// Absolute alarms stay canonical UTC instants. Event start/end fields are
/// local wall-clock strings resolved with `Local` in `parse_local`; future
/// absolute-alarm controls must explicitly choose a time zone and resolve DST
/// gaps/overlaps before producing this value.
fn format_absolute_alarm(date_time: DateTime<Utc>, time_zone: &str) -> String {
    let rendered = alarm_time_zone(time_zone)
        .map(|zone| {
            date_time
                .with_timezone(&zone)
                .format("%-d %b %Y, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| date_time.format("%-d %b %Y, %H:%M UTC").to_string());
    format!("{rendered} (absolute)",)
}

#[derive(Debug, PartialEq, Eq)]
enum AbsoluteResolution {
    Single(DateTime<Utc>),
    Ambiguous(DateTime<Utc>, DateTime<Utc>),
}

fn alarm_time_zone(name: &str) -> Result<Tz, AlarmValidationError> {
    name.parse::<Tz>()
        .map_err(|_| AlarmValidationError::InvalidTimeZone)
}

fn absolute_edit_buffers(
    date_time: DateTime<Utc>,
    time_zone: &str,
) -> Result<(String, String), AlarmValidationError> {
    let zone = alarm_time_zone(time_zone)?;
    let local = date_time.with_timezone(&zone);
    Ok((
        local.format("%Y-%m-%d").to_string(),
        local.format("%H:%M").to_string(),
    ))
}

fn resolve_absolute_alarm(
    date_buffer: &str,
    time_buffer: &str,
    time_zone: &str,
) -> Result<AbsoluteResolution, AlarmValidationError> {
    let date = NaiveDate::parse_from_str(date_buffer.trim(), "%Y-%m-%d")
        .map_err(|_| AlarmValidationError::InvalidDate)?;
    let time = NaiveTime::parse_from_str(time_buffer.trim(), "%H:%M")
        .map_err(|_| AlarmValidationError::InvalidTime)?;
    let zone = alarm_time_zone(time_zone)?;
    match zone.from_local_datetime(&date.and_time(time)) {
        LocalResult::Single(value) => Ok(AbsoluteResolution::Single(value.with_timezone(&Utc))),
        LocalResult::None => Err(AlarmValidationError::NonexistentLocalTime),
        LocalResult::Ambiguous(first, second) => Ok(AbsoluteResolution::Ambiguous(
            first.with_timezone(&Utc),
            second.with_timezone(&Utc),
        )),
    }
}

fn validate_replacement_alarms(alarms: &[Alarm]) -> Result<(), String> {
    if alarms.iter().all(|alarm| {
        matches!(
            (alarm.relative_seconds, alarm.absolute_date),
            (Some(_), None) | (None, Some(_))
        )
    }) {
        Ok(())
    } else {
        Err("Invalid alarm payload".into())
    }
}

impl FormField {
    pub const ALL: [Self; 17] = [
        Self::Title,
        Self::Calendar,
        Self::Start,
        Self::End,
        Self::AllDay,
        Self::Location,
        Self::Notes,
        Self::Url,
        Self::Alarms,
        Self::Recurrence,
        Self::RecurrenceInterval,
        Self::Weekdays,
        Self::RecurrenceEnds,
        Self::RecurrenceEndDate,
        Self::RecurrenceOccurrences,
        Self::TimeZone,
        Self::Availability,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Calendar => "Calendar",
            Self::Start => "Start",
            Self::End => "End",
            Self::AllDay => "All day",
            Self::Location => "Location",
            Self::Notes => "Notes",
            Self::Url => "URL",
            Self::Alarms => "Alerts",
            Self::Recurrence => "Repeat",
            Self::RecurrenceInterval => "Interval",
            Self::Weekdays => "Days",
            Self::RecurrenceEnds => "Ends",
            Self::RecurrenceEndDate => "End date",
            Self::RecurrenceOccurrences => "Occurrences",
            Self::TimeZone => "Time zone",
            Self::Availability => "Availability",
            Self::Scope => "Edit scope",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceEditorState {
    None,
    Structured(RecurrenceEditorData),
    /// The exact provider-neutral payload is retained so opening and saving an
    /// event cannot silently simplify a rule the structured controls do not
    /// yet understand.
    Unsupported {
        original_rules: Vec<RecurrenceRule>,
        summary: String,
    },
}

/// Owns provider-neutral recurrence data plus future editor-only state.  UI
/// code should use the accessors below rather than depending on the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceEditorData {
    pub rule: RecurrenceRule,
    pub end_mode: RecurrenceEndMode,
    pub end_date_buffer: String,
    /// Parsed editor state only. The provider-neutral rule is not changed
    /// until the existing save conversion is invoked.
    pub validated_end_date: Option<NaiveDate>,
    pub occurrence_count_buffer: String,
    /// Parsed editor state only; see `validated_end_date` above.
    pub validated_occurrence_count: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurrenceEndMode {
    Never,
    OnDate,
    AfterOccurrences,
    InvalidExisting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurrenceValidationError {
    InvalidInterval,
    MissingWeeklyDay,
    InvalidEndDate,
    EndDateBeforeStart,
    InvalidOccurrenceCount,
    InvalidExistingEndCondition,
}

impl fmt::Display for RecurrenceValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInterval => "Repeat interval must be at least 1",
            Self::MissingWeeklyDay => "Select at least one weekday for weekly repeats",
            Self::InvalidEndDate => "Repeat end date must use YYYY-MM-DD",
            Self::EndDateBeforeStart => "Repeat end date must be on or after the event start date",
            Self::InvalidOccurrenceCount => "Repeat occurrence count must be a positive integer",
            Self::InvalidExistingEndCondition => {
                "Resolve the existing recurrence end condition before saving"
            }
        })
    }
}

impl RecurrenceEditorData {
    fn from_rule(rule: RecurrenceRule) -> Self {
        let end_mode = match (&rule.end_date, rule.occurrence_count) {
            (None, None) => RecurrenceEndMode::Never,
            (Some(_), None) => RecurrenceEndMode::OnDate,
            (None, Some(_)) => RecurrenceEndMode::AfterOccurrences,
            (Some(_), Some(_)) => RecurrenceEndMode::InvalidExisting,
        };
        Self {
            end_date_buffer: rule
                .end_date
                .map(|date| date.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            validated_end_date: rule.end_date.as_ref().map(DateTime::date_naive),
            occurrence_count_buffer: rule
                .occurrence_count
                .map(|value| value.to_string())
                .unwrap_or_default(),
            validated_occurrence_count: rule.occurrence_count.filter(|value| *value > 0),
            rule,
            end_mode,
        }
    }
    pub fn to_rule(
        &self,
        event_start: NaiveDate,
    ) -> Result<RecurrenceRule, RecurrenceValidationError> {
        let mut rule = self.rule.clone();
        if rule.interval == 0 {
            return Err(RecurrenceValidationError::InvalidInterval);
        }
        if rule.frequency == RecurrenceFrequency::Weekly && rule.days_of_week.is_empty() {
            return Err(RecurrenceValidationError::MissingWeeklyDay);
        }
        match self.end_mode {
            RecurrenceEndMode::Never => {
                rule.end_date = None;
                rule.occurrence_count = None;
            }
            RecurrenceEndMode::OnDate => {
                let date = parse_recurrence_end_date(&self.end_date_buffer)
                    .map_err(|_| RecurrenceValidationError::InvalidEndDate)?;
                if date < event_start {
                    return Err(RecurrenceValidationError::EndDateBeforeStart);
                }
                rule.end_date = Some(DateTime::<Utc>::from_naive_utc_and_offset(
                    date.and_hms_opt(0, 0, 0)
                        .ok_or(RecurrenceValidationError::InvalidEndDate)?,
                    Utc,
                ));
                rule.occurrence_count = None;
            }
            RecurrenceEndMode::AfterOccurrences => {
                let count = self
                    .occurrence_count_buffer
                    .parse::<u32>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or(RecurrenceValidationError::InvalidOccurrenceCount)?;
                rule.occurrence_count = Some(count);
                rule.end_date = None;
            }
            RecurrenceEndMode::InvalidExisting => {
                return Err(RecurrenceValidationError::InvalidExistingEndCondition);
            }
        }
        Ok(rule)
    }

    /// Changes only editor state. The provider rule and the two input buffers
    /// are retained until the end-value editing and conversion workflow is
    /// introduced, so switching modes cannot discard in-progress input.
    fn cycle_end_mode(&mut self, direction: i32) {
        let modes = [
            RecurrenceEndMode::Never,
            RecurrenceEndMode::OnDate,
            RecurrenceEndMode::AfterOccurrences,
        ];
        let current = match self.end_mode {
            RecurrenceEndMode::Never => 0,
            RecurrenceEndMode::OnDate => 1,
            RecurrenceEndMode::AfterOccurrences => 2,
            // Choosing a direction is an explicit resolution of the invalid
            // existing condition; it never silently picks a mode on load.
            RecurrenceEndMode::InvalidExisting if direction < 0 => 0,
            RecurrenceEndMode::InvalidExisting => 2,
        };
        self.end_mode = modes[(current + direction).rem_euclid(modes.len() as i32) as usize];
    }

    fn refresh_end_date_validation(&mut self, event_start: NaiveDate) {
        self.validated_end_date = (self.end_mode == RecurrenceEndMode::OnDate)
            .then(|| parse_recurrence_end_date(&self.end_date_buffer).ok())
            .flatten()
            .filter(|date| *date >= event_start);
    }

    fn refresh_occurrence_count_validation(&mut self) {
        self.validated_occurrence_count = (self.end_mode == RecurrenceEndMode::AfterOccurrences)
            .then(|| self.occurrence_count_buffer.trim().parse::<u32>().ok())
            .flatten()
            .filter(|count| *count > 0);
    }
}

fn parse_recurrence_end_date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
        .map_err(|_| "Repeat end date must use YYYY-MM-DD".into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WeekdayCursor {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl WeekdayCursor {
    pub const ALL: [Self; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];
    pub fn code(self) -> &'static str {
        match self {
            Self::Monday => "MO",
            Self::Tuesday => "TU",
            Self::Wednesday => "WE",
            Self::Thursday => "TH",
            Self::Friday => "FR",
            Self::Saturday => "SA",
            Self::Sunday => "SU",
        }
    }
    fn from_code(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|day| day.code() == value)
    }
    pub fn move_by(self, direction: i32) -> Self {
        let index = Self::ALL.iter().position(|day| *day == self).unwrap_or(0);
        Self::ALL[(index as i32 + direction).rem_euclid(7) as usize]
    }
}

impl RecurrenceEditorState {
    pub fn none() -> Self {
        Self::None
    }
    pub fn from_rules(rules: &[RecurrenceRule]) -> Self {
        match rules {
            [] => Self::None,
            [rule]
                if rule.interval > 0
                    && (rule.frequency != RecurrenceFrequency::Weekly
                        || !rule.days_of_week.is_empty()) =>
            {
                Self::Structured(RecurrenceEditorData::from_rule(rule.clone()))
            }
            _ => Self::Unsupported {
                original_rules: rules.to_vec(),
                summary: humanize_recurrence(rules),
            },
        }
    }
    pub fn to_rules(
        &self,
        event_start: NaiveDate,
    ) -> Result<Vec<RecurrenceRule>, RecurrenceValidationError> {
        match self {
            Self::None => Ok(vec![]),
            Self::Structured(data) => Ok(vec![data.to_rule(event_start)?]),
            Self::Unsupported { original_rules, .. } => Ok(original_rules.clone()),
        }
    }
    pub fn summary(&self) -> String {
        match self {
            Self::None => "Does not repeat".into(),
            Self::Structured(data) => {
                let summary = humanize_recurrence(std::slice::from_ref(&data.rule));
                match data.end_mode {
                    RecurrenceEndMode::OnDate if data.validated_end_date.is_some() => {
                        let date = data.validated_end_date.unwrap();
                        format!("{summary} Until {}", date.format("%-d %b %Y"))
                    }
                    RecurrenceEndMode::AfterOccurrences
                        if data.validated_occurrence_count.is_some() =>
                    {
                        let count = data.validated_occurrence_count.unwrap();
                        format!("{summary} For {count} occurrences")
                    }
                    _ => summary,
                }
            }
            Self::Unsupported { summary, .. } => format!("Custom / unsupported: {summary}"),
        }
    }
    pub fn picker_label(&self) -> &'static str {
        match self {
            Self::None => "Never",
            Self::Structured(data) => match data.rule.frequency {
                RecurrenceFrequency::Daily => "Daily",
                RecurrenceFrequency::Weekly => "Weekly",
                RecurrenceFrequency::Monthly => "Monthly",
                RecurrenceFrequency::Yearly => "Yearly",
            },
            Self::Unsupported { .. } => "Custom / Unsupported recurrence",
        }
    }
    fn cycle_mode(&mut self, direction: i32, event_date: NaiveDate) {
        if matches!(self, Self::Unsupported { .. }) {
            return;
        }
        let modes = [
            None,
            Some(RecurrenceFrequency::Daily),
            Some(RecurrenceFrequency::Weekly),
            Some(RecurrenceFrequency::Monthly),
            Some(RecurrenceFrequency::Yearly),
        ];
        let current = match self {
            Self::None => 0,
            Self::Structured(data) => modes
                .iter()
                .position(|mode| *mode == Some(data.rule.frequency))
                .unwrap_or(0),
            Self::Unsupported { .. } => return,
        };
        let next = (current as i32 + direction).rem_euclid(modes.len() as i32) as usize;
        *self = match modes[next] {
            None => Self::None,
            Some(frequency) => Self::Structured(RecurrenceEditorData::from_rule(RecurrenceRule {
                frequency,
                interval: 1,
                days_of_week: if frequency == RecurrenceFrequency::Weekly {
                    vec![weekday_code(event_date.weekday()).into()]
                } else {
                    vec![]
                },
                occurrence_count: None,
                end_date: None,
            })),
        };
    }
}

fn weekday_code(day: chrono::Weekday) -> &'static str {
    match day {
        chrono::Weekday::Mon => "MO",
        chrono::Weekday::Tue => "TU",
        chrono::Weekday::Wed => "WE",
        chrono::Weekday::Thu => "TH",
        chrono::Weekday::Fri => "FR",
        chrono::Weekday::Sat => "SA",
        chrono::Weekday::Sun => "SU",
    }
}

#[derive(Debug, Clone)]
pub struct EventForm {
    pub editor_mode: EditorMode,
    pub id: Option<String>,
    occurrence_id: Option<String>,
    occurrence_start: Option<DateTime<Utc>>,
    occurrence_calendar_id: Option<String>,
    pub title: String,
    pub calendar_index: usize,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub location: String,
    pub notes: String,
    pub url: String,
    pub alarm_state: AlarmEditorState,
    pub recurrence: RecurrenceEditorState,
    pub weekday_cursor: WeekdayCursor,
    pub weekday_selection: BTreeSet<WeekdayCursor>,
    pub time_zone: String,
    pub time_zone_provenance: TimeZoneProvenance,
    pub availability: Availability,
    pub span: EventSpan,
    /// Set only by the recurring-mutation scope dialog. It is intentionally
    /// independent of the edited recurrence rule, which may itself change.
    pub recurrence_scope: Option<RecurrenceMutationScope>,
    pub selected: usize,
    all_day_editor: Option<AllDayEditorState>,
    /// Timed buffers are retained only while a newly-created form is toggled
    /// into all-day mode, so toggling back restores exact timed input without
    /// treating those instants as all-day date identity.
    create_all_day_timed_backup: Option<(String, String)>,
}

#[derive(Debug, Clone)]
struct AllDayEditorState {
    original_start: NaiveDate,
    original_end_exclusive: NaiveDate,
}

impl EventForm {
    pub fn new(
        calendar_index: usize,
        calendars: &[CalendarInfo],
        date: NaiveDate,
        start_minute: u16,
        duration_minutes: u16,
    ) -> Self {
        let draft = EventDraft::new(calendars[calendar_index].id.clone(), date);
        let start_local = date
            .and_hms_opt(
                (start_minute / 60).min(23) as u32,
                (start_minute % 60).into(),
                0,
            )
            .and_then(|value| value.and_local_timezone(Local).single())
            .unwrap_or_else(|| {
                draft
                    .time
                    .as_timed_range()
                    .map(|(start, _)| start.with_timezone(&Local))
                    .expect("new event draft is timed")
            });
        let end_local = start_local + Duration::minutes(i64::from(duration_minutes.max(1)));
        Self {
            editor_mode: EditorMode::Create,
            id: None,
            occurrence_id: None,
            occurrence_start: None,
            occurrence_calendar_id: None,
            title: String::new(),
            calendar_index,
            start: start_local.format("%Y-%m-%d %H:%M").to_string(),
            end: end_local.format("%Y-%m-%d %H:%M").to_string(),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            url: String::new(),
            alarm_state: AlarmEditorState::new_event(),
            recurrence: RecurrenceEditorState::none(),
            weekday_cursor: WeekdayCursor::Monday,
            weekday_selection: BTreeSet::new(),
            time_zone: draft.time_zone,
            time_zone_provenance: TimeZoneProvenance::ExplicitEvent,
            availability: Availability::Busy,
            span: EventSpan::ThisEvent,
            recurrence_scope: None,
            selected: 0,
            all_day_editor: None,
            create_all_day_timed_backup: None,
        }
    }

    pub fn from_event(event: &Event, calendars: &[CalendarInfo]) -> Self {
        let trusted_all_day = event.all_day_date_range();
        let weekday_selection: BTreeSet<WeekdayCursor> = event
            .recurrence
            .first()
            .filter(|rule| rule.frequency == RecurrenceFrequency::Weekly)
            .map(|rule| {
                rule.days_of_week
                    .iter()
                    .filter_map(|day| WeekdayCursor::from_code(day))
                    .collect()
            })
            .unwrap_or_default();
        let weekday_cursor = weekday_selection
            .iter()
            .next()
            .copied()
            .unwrap_or(WeekdayCursor::Monday);
        Self {
            editor_mode: EditorMode::Edit {
                event_id: event.id.clone(),
            },
            id: event.provider_id.clone(),
            occurrence_id: Some(event.id.clone()),
            occurrence_start: Some(event.start),
            occurrence_calendar_id: Some(event.calendar_id.clone()),
            title: event.title.clone(),
            calendar_index: calendars
                .iter()
                .position(|c| c.id == event.calendar_id)
                .unwrap_or(0),
            start: trusted_all_day
                .map(|(start, _)| start.to_string())
                .unwrap_or_else(|| {
                    event
                        .start
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                }),
            end: trusted_all_day
                .and_then(|(_, end)| end.checked_sub_signed(Duration::days(1)))
                .map(|end| end.to_string())
                .unwrap_or_else(|| {
                    event
                        .end
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                }),
            all_day: event.all_day,
            location: event.location.clone(),
            notes: event.notes.clone(),
            url: event.url.clone(),
            alarm_state: AlarmEditorState::from_event_alarms(
                &event.alarms,
                event.all_day,
                event.time_zone_provenance,
            ),
            recurrence: RecurrenceEditorState::from_rules(&event.recurrence),
            weekday_cursor,
            weekday_selection,
            time_zone: event.time_zone.clone(),
            time_zone_provenance: event.time_zone_provenance,
            availability: event.availability,
            span: EventSpan::ThisEvent,
            recurrence_scope: None,
            selected: 0,
            all_day_editor: trusted_all_day.map(|(original_start, original_end_exclusive)| {
                AllDayEditorState {
                    original_start,
                    original_end_exclusive,
                }
            }),
            create_all_day_timed_backup: None,
        }
    }

    pub fn duplicate_from(event: &Event, calendars: &[CalendarInfo]) -> Self {
        let mut form = Self::from_event(event, calendars);
        form.editor_mode = EditorMode::Duplicate {
            source_id: event.id.clone(),
        };
        form.id = None;
        form.occurrence_id = None;
        form.occurrence_start = None;
        form.occurrence_calendar_id = None;
        form.recurrence = RecurrenceEditorState::none();
        form.weekday_selection.clear();
        form.span = EventSpan::ThisEvent;
        form
    }

    pub fn field(&self) -> FormField {
        self.visible_fields()
            .get(self.selected)
            .copied()
            .unwrap_or(FormField::Title)
    }
    pub fn visible_fields(&self) -> Vec<FormField> {
        let mut fields = FormField::ALL.to_vec();
        let is_structured = matches!(self.recurrence, RecurrenceEditorState::Structured(_));
        if !is_structured {
            fields.retain(|field| *field != FormField::RecurrenceInterval);
        }
        let weekly = matches!(&self.recurrence, RecurrenceEditorState::Structured(data) if data.rule.frequency == RecurrenceFrequency::Weekly);
        if !weekly {
            fields.retain(|field| *field != FormField::Weekdays);
        }
        match &self.recurrence {
            RecurrenceEditorState::Structured(data) => match data.end_mode {
                RecurrenceEndMode::Never | RecurrenceEndMode::InvalidExisting => {
                    fields.retain(|field| {
                        !matches!(
                            field,
                            FormField::RecurrenceEndDate | FormField::RecurrenceOccurrences
                        )
                    });
                }
                RecurrenceEndMode::OnDate => {
                    fields.retain(|field| *field != FormField::RecurrenceOccurrences);
                }
                RecurrenceEndMode::AfterOccurrences => {
                    fields.retain(|field| *field != FormField::RecurrenceEndDate);
                }
            },
            RecurrenceEditorState::None | RecurrenceEditorState::Unsupported { .. } => {
                fields.retain(|field| {
                    !matches!(
                        field,
                        FormField::RecurrenceEnds
                            | FormField::RecurrenceEndDate
                            | FormField::RecurrenceOccurrences
                    )
                });
            }
        }
        fields
    }
    fn normalize_cursor(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_fields().len().saturating_sub(1));
    }
    pub fn next_field(&mut self) {
        let len = self.visible_fields().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }
    pub fn previous_field(&mut self) {
        let len = self.visible_fields().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }
    pub fn is_toggle(&self) -> bool {
        matches!(
            self.field(),
            FormField::Calendar
                | FormField::AllDay
                | FormField::Availability
                | FormField::Recurrence
                | FormField::RecurrenceInterval
                | FormField::RecurrenceEnds
        )
    }

    pub fn value(&self, calendars: &[CalendarInfo]) -> String {
        match self.field() {
            FormField::Title => self.title.clone(),
            FormField::Calendar => calendars
                .get(self.calendar_index)
                .map(|c| c.title.clone())
                .unwrap_or_default(),
            FormField::Start => self.start.clone(),
            FormField::End => self.end.clone(),
            FormField::AllDay => if self.all_day { "yes" } else { "no" }.into(),
            FormField::Location => self.location.clone(),
            FormField::Notes => self.notes.clone(),
            FormField::Url => self.url.clone(),
            FormField::Alarms => self.alarm_state.display_in_timezone(&self.time_zone),
            FormField::Recurrence => self.recurrence.picker_label().into(),
            FormField::RecurrenceInterval => match &self.recurrence {
                RecurrenceEditorState::Structured(data) => data.rule.interval.to_string(),
                RecurrenceEditorState::None => "—".into(),
                RecurrenceEditorState::Unsupported { .. } => "protected".into(),
            },
            FormField::Weekdays => self.weekday_display(),
            FormField::RecurrenceEnds => match &self.recurrence {
                RecurrenceEditorState::Structured(data) => match data.end_mode {
                    RecurrenceEndMode::Never => "Never",
                    RecurrenceEndMode::OnDate => "On date",
                    RecurrenceEndMode::AfterOccurrences => "After occurrences",
                    RecurrenceEndMode::InvalidExisting => "Existing condition is invalid",
                }
                .into(),
                RecurrenceEditorState::None | RecurrenceEditorState::Unsupported { .. } => {
                    String::new()
                }
            },
            FormField::RecurrenceEndDate => match &self.recurrence {
                RecurrenceEditorState::Structured(data) => data.end_date_buffer.clone(),
                RecurrenceEditorState::None | RecurrenceEditorState::Unsupported { .. } => {
                    String::new()
                }
            },
            FormField::RecurrenceOccurrences => match &self.recurrence {
                RecurrenceEditorState::Structured(data) => data.occurrence_count_buffer.clone(),
                RecurrenceEditorState::None | RecurrenceEditorState::Unsupported { .. } => {
                    String::new()
                }
            },
            FormField::TimeZone => self.time_zone.clone(),
            FormField::Availability => self.availability.to_string(),
            FormField::Scope => match self.span {
                EventSpan::ThisEvent => "this event",
                EventSpan::FutureEvents => "this and future events",
            }
            .into(),
        }
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.field() {
            FormField::Title => Some(&mut self.title),
            FormField::Start => Some(&mut self.start),
            FormField::End => Some(&mut self.end),
            FormField::Location => Some(&mut self.location),
            FormField::Notes => Some(&mut self.notes),
            FormField::Url => Some(&mut self.url),
            FormField::TimeZone => Some(&mut self.time_zone),
            _ => None,
        }
    }
    pub fn push(&mut self, character: char) -> bool {
        if self.field() == FormField::Alarms {
            return false;
        }
        if self.field() == FormField::RecurrenceEndDate {
            let event_start = parse_local(&self.start)
                .map(|date| date.with_timezone(&Local).date_naive())
                .unwrap_or_else(|_| Local::now().date_naive());
            if let RecurrenceEditorState::Structured(data) = &mut self.recurrence
                && data.end_mode == RecurrenceEndMode::OnDate
            {
                data.end_date_buffer.push(character);
                data.refresh_end_date_validation(event_start);
            }
            return true;
        }
        if self.field() == FormField::RecurrenceOccurrences {
            if let RecurrenceEditorState::Structured(data) = &mut self.recurrence
                && data.end_mode == RecurrenceEndMode::AfterOccurrences
            {
                data.occurrence_count_buffer.push(character);
                data.refresh_occurrence_count_validation();
            }
            return true;
        }
        if let Some(text) = self.text_mut() {
            text.push(character);
            return true;
        }
        false
    }
    pub fn backspace(&mut self) -> bool {
        if self.field() == FormField::Alarms {
            return false;
        }
        if self.field() == FormField::RecurrenceEndDate {
            let event_start = parse_local(&self.start)
                .map(|date| date.with_timezone(&Local).date_naive())
                .unwrap_or_else(|_| Local::now().date_naive());
            if let RecurrenceEditorState::Structured(data) = &mut self.recurrence
                && data.end_mode == RecurrenceEndMode::OnDate
            {
                data.end_date_buffer.pop();
                data.refresh_end_date_validation(event_start);
            }
            return true;
        }
        if self.field() == FormField::RecurrenceOccurrences {
            if let RecurrenceEditorState::Structured(data) = &mut self.recurrence
                && data.end_mode == RecurrenceEndMode::AfterOccurrences
            {
                data.occurrence_count_buffer.pop();
                data.refresh_occurrence_count_validation();
            }
            return true;
        }
        if let Some(text) = self.text_mut() {
            text.pop();
            return true;
        }
        false
    }

    pub fn adjust(&mut self, direction: i32, calendars: &[CalendarInfo]) {
        match self.field() {
            FormField::Calendar if !calendars.is_empty() => {
                for _ in 0..calendars.len() {
                    self.calendar_index = if direction < 0 {
                        (self.calendar_index + calendars.len() - 1) % calendars.len()
                    } else {
                        (self.calendar_index + 1) % calendars.len()
                    };
                    if calendars[self.calendar_index].is_writable {
                        break;
                    }
                }
            }
            FormField::AllDay => {
                self.all_day = !self.all_day;
                if matches!(self.editor_mode, EditorMode::Create) {
                    if self.all_day {
                        let backup = (self.start.clone(), self.end.clone());
                        if let (Some(start), Some(end)) = (
                            form_calendar_date(&self.start),
                            form_calendar_date(&self.end),
                        ) {
                            self.start = start.to_string();
                            self.end = end.to_string();
                            self.create_all_day_timed_backup = Some(backup);
                        }
                    } else if let Some((start, end)) = self.create_all_day_timed_backup.take() {
                        self.start = start;
                        self.end = end;
                    }
                }
                if self.all_day {
                    self.alarm_state = if self.id.is_some() {
                        AlarmEditorState::ProtectedExisting
                    } else {
                        AlarmEditorState::from_existing(&[])
                    };
                }
            }
            FormField::Availability => {
                self.availability = match (self.availability, direction < 0) {
                    (Availability::Busy, true) => Availability::Unavailable,
                    (Availability::Busy, false) => Availability::Free,
                    (Availability::Free, true) => Availability::Busy,
                    (Availability::Free, false) => Availability::Tentative,
                    (Availability::Tentative, true) => Availability::Free,
                    (Availability::Tentative, false) => Availability::Unavailable,
                    (Availability::Unavailable, true) => Availability::Tentative,
                    _ => Availability::Busy,
                }
            }
            // Recurrence scope is selected in the dedicated modal before an
            // edit/delete mutation. It intentionally is not editable here.
            FormField::Scope => {}
            FormField::Recurrence => {
                let date = NaiveDateTime::parse_from_str(&self.start, "%Y-%m-%d %H:%M")
                    .map(|value| value.date())
                    .unwrap_or_else(|_| Local::now().date_naive());
                self.recurrence.cycle_mode(direction, date);
                if let RecurrenceEditorState::Structured(data) = &self.recurrence {
                    if data.rule.frequency == RecurrenceFrequency::Weekly {
                        self.weekday_selection = data
                            .rule
                            .days_of_week
                            .iter()
                            .filter_map(|day| WeekdayCursor::from_code(day))
                            .collect();
                        self.weekday_cursor = self
                            .weekday_selection
                            .iter()
                            .next()
                            .copied()
                            .unwrap_or_else(|| {
                                WeekdayCursor::from_code(weekday_code(date.weekday())).unwrap()
                            });
                    } else {
                        self.weekday_selection.clear();
                    }
                }
                self.normalize_cursor();
            }
            FormField::RecurrenceInterval => {
                if let RecurrenceEditorState::Structured(data) = &mut self.recurrence {
                    data.rule.interval = if direction < 0 {
                        data.rule.interval.saturating_sub(1).max(1)
                    } else {
                        data.rule.interval.saturating_add(1)
                    };
                }
            }
            FormField::RecurrenceEnds => {
                if let RecurrenceEditorState::Structured(data) = &mut self.recurrence {
                    data.cycle_end_mode(direction);
                    self.normalize_cursor();
                }
            }
            FormField::Weekdays => self.weekday_cursor = self.weekday_cursor.move_by(direction),
            _ => {}
        }
    }

    pub fn to_draft(
        &self,
        calendars: &[CalendarInfo],
    ) -> Result<(EventDraft, EventSpan, AlarmMutation), String> {
        if matches!(self.editor_mode, EditorMode::Edit { .. }) && self.id.is_none() {
            return Err("Event provider identity is unavailable; refresh before editing".into());
        }
        if self.title.trim().is_empty() {
            return Err("Title is required".into());
        }
        let calendar = calendars
            .get(self.calendar_index)
            .ok_or("Select a calendar")?;
        if !calendar.is_writable {
            return Err("Selected calendar is read-only".into());
        }
        let (time, recurrence_start) =
            if matches!(self.editor_mode, EditorMode::Duplicate { .. }) && self.all_day {
                let state = self
                    .all_day_editor
                    .as_ref()
                    .ok_or("Cannot safely duplicate legacy all-day event until refreshed")?;
                (
                    EventTimeInput::all_day(state.original_start, state.original_end_exclusive)?,
                    state.original_start,
                )
            } else if matches!(self.editor_mode, EditorMode::Create) && self.all_day {
                let start_date = NaiveDate::parse_from_str(self.start.trim(), "%Y-%m-%d")
                    .map_err(|_| "All-day start date must use YYYY-MM-DD")?;
                let inclusive_end = NaiveDate::parse_from_str(self.end.trim(), "%Y-%m-%d")
                    .map_err(|_| "All-day end date must use YYYY-MM-DD")?;
                if inclusive_end < start_date {
                    return Err("All-day end date must not be before start date".into());
                }
                let end_date_exclusive = inclusive_end
                    .checked_add_signed(Duration::days(1))
                    .ok_or("All-day end date is out of range")?;
                (
                    EventTimeInput::all_day(start_date, end_date_exclusive)?,
                    start_date,
                )
            } else {
                let (start, end) = if let Some(state) = &self.all_day_editor {
                    (
                        state.original_start.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                        state
                            .original_end_exclusive
                            .and_hms_opt(0, 0, 0)
                            .unwrap()
                            .and_utc(),
                    )
                } else {
                    (parse_local(&self.start)?, parse_local(&self.end)?)
                };
                if end <= start {
                    return Err("End must be after start".into());
                }
                (
                    if self.all_day {
                        EventTimeInput::legacy_all_day_unknown(start, end)?
                    } else {
                        EventTimeInput::timed(start, end)?
                    },
                    start.with_timezone(&Local).date_naive(),
                )
            };
        let url = self.url.trim();
        if !url.is_empty() && !url.contains("://") {
            return Err("URL must include a scheme, such as https://".into());
        }
        let updating = self.id.is_some();
        let (alarms, alarm_mutation) = self.alarm_state.draft_alarms_and_mutation(updating)?;
        Ok((
            EventDraft {
                id: self.id.clone(),
                occurrence_id: self.occurrence_id.clone(),
                occurrence_start: self.occurrence_start,
                occurrence_calendar_id: self.occurrence_calendar_id.clone(),
                calendar_id: calendar.id.clone(),
                title: self.title.trim().into(),
                time,
                location: self.location.trim().into(),
                notes: self.notes.trim().into(),
                url: url.into(),
                time_zone: self.time_zone.trim().into(),
                availability: self.availability,
                // Public EventKit exposes attendees as read-only. Preserve them
                // on the native object and never offer unsupported editing here.
                attendees: vec![],
                alarms,
                recurrence: self.rules_for_save(recurrence_start)?,
            },
            self.span,
            alarm_mutation,
        ))
    }

    fn time_mutation(&self) -> Result<EventTimeMutation, String> {
        if let Some(state) = &self.all_day_editor {
            let start = NaiveDate::parse_from_str(self.start.trim(), "%Y-%m-%d")
                .map_err(|_| "All-day start date must use YYYY-MM-DD")?;
            let inclusive_end = NaiveDate::parse_from_str(self.end.trim(), "%Y-%m-%d")
                .map_err(|_| "All-day end date must use YYYY-MM-DD")?;
            if inclusive_end < start {
                return Err("All-day end date must not be before start date".into());
            }
            let end_exclusive = inclusive_end
                .checked_add_signed(Duration::days(1))
                .ok_or("All-day end date is out of range")?;
            return Ok(
                if (start, end_exclusive) == (state.original_start, state.original_end_exclusive) {
                    EventTimeMutation::Preserve
                } else {
                    EventTimeMutation::ReplaceAllDay {
                        start_date: start,
                        end_date_exclusive: end_exclusive,
                    }
                },
            );
        }
        Ok(if self.id.is_some() && self.all_day {
            EventTimeMutation::Preserve
        } else {
            EventTimeMutation::ReplaceLegacy
        })
    }
}

impl EventForm {
    fn weekday_display(&self) -> String {
        WeekdayCursor::ALL
            .iter()
            .map(|day| {
                let selected = self.weekday_selection.contains(day);
                let cursor = *day == self.weekday_cursor;
                format!(
                    "{}{}{}{}",
                    if cursor { ">" } else { " " },
                    if selected { "[" } else { "" },
                    match day {
                        WeekdayCursor::Monday => "Mon",
                        WeekdayCursor::Tuesday => "Tue",
                        WeekdayCursor::Wednesday => "Wed",
                        WeekdayCursor::Thursday => "Thu",
                        WeekdayCursor::Friday => "Fri",
                        WeekdayCursor::Saturday => "Sat",
                        WeekdayCursor::Sunday => "Sun",
                    },
                    if selected { "]" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn toggle_weekday(&mut self) {
        if !self.weekday_selection.insert(self.weekday_cursor) {
            self.weekday_selection.remove(&self.weekday_cursor);
        }
    }
    fn rules_for_save(&self, event_start: NaiveDate) -> Result<Vec<RecurrenceRule>, String> {
        let mut state = self.recurrence.clone();
        if let RecurrenceEditorState::Structured(data) = &mut state
            && data.rule.frequency == RecurrenceFrequency::Weekly
        {
            data.rule.days_of_week = self
                .weekday_selection
                .iter()
                .map(|day| day.code().into())
                .collect();
        }
        state
            .to_rules(event_start)
            .map_err(|error| error.to_string())
    }
}

fn parse_local(text: &str) -> Result<DateTime<Utc>, String> {
    let naive = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M")
        .map_err(|_| "Date/time must use YYYY-MM-DD HH:MM".to_string())?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|d| d.with_timezone(&Utc))
        .ok_or_else(|| "Date/time is ambiguous or invalid in the local time zone".into())
}

/// Extracts the calendar date shown by either the existing timed editor buffer
/// or the all-day create buffer. This is UI parsing only; it never creates an
/// instant for an all-day event.
fn form_calendar_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M")
                .ok()
                .map(|date_time| date_time.date())
        })
}

pub fn parse_alarms(text: &str) -> Result<Vec<Alarm>, String> {
    if text.trim().is_empty() || text.trim().eq_ignore_ascii_case("none") {
        return Ok(vec![]);
    }
    text.split(',')
        .map(|item| {
            let item = item.trim();
            if let Some(date) = item.strip_prefix('@') {
                let absolute = DateTime::parse_from_rfc3339(date)
                    .map_err(|_| format!("Invalid absolute alert: {item}"))?;
                return Ok(Alarm {
                    relative_seconds: None,
                    absolute_date: Some(absolute.with_timezone(&Utc)),
                    is_editable: true,
                });
            }
            let (number, unit) = item.split_at(item.len().saturating_sub(1));
            let amount: i64 = number
                .parse()
                .map_err(|_| format!("Invalid alert: {item}"))?;
            if amount < 0 {
                return Err(format!("Alert must be at or before the event: {item}"));
            }
            let multiplier = match unit.to_ascii_lowercase().as_str() {
                "m" => 60,
                "h" => 3600,
                "d" => 86400,
                _ => return Err(format!("Alert unit must be m, h, or d: {item}")),
            };
            let seconds = amount
                .checked_mul(multiplier)
                .ok_or_else(|| format!("Alert is too large: {item}"))?;
            Ok(Alarm {
                relative_seconds: Some(-seconds),
                absolute_date: None,
                is_editable: true,
            })
        })
        .collect()
}

pub fn parse_recurrence(text: &str) -> Result<Vec<RecurrenceRule>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("none") {
        return Ok(vec![]);
    }
    let parts = text.split(':').collect::<Vec<_>>();
    let frequency = match parts[0].to_ascii_lowercase().as_str() {
        "daily" => RecurrenceFrequency::Daily,
        "weekly" => RecurrenceFrequency::Weekly,
        "monthly" => RecurrenceFrequency::Monthly,
        "yearly" => RecurrenceFrequency::Yearly,
        _ => return Err("Repeat must be none, daily, weekly, monthly, or yearly".into()),
    };
    let interval = parts
        .get(1)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>())
        .transpose()
        .map_err(|_| "Repeat interval must be a number".to_string())?
        .unwrap_or(1);
    if interval == 0 {
        return Err("Repeat interval must be at least 1".into());
    }
    let days_of_week = parts
        .get(2)
        .map(|days| {
            days.split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_uppercase())
                .collect()
        })
        .unwrap_or_default();
    Ok(vec![RecurrenceRule {
        frequency,
        interval,
        days_of_week,
        occurrence_count: None,
        end_date: None,
    }])
}

pub fn humanize_recurrence(rules: &[RecurrenceRule]) -> String {
    let Some(rule) = rules.first() else {
        return "Does not repeat".into();
    };
    let interval = rule.interval.max(1);
    let every = |unit: &str| {
        if interval == 1 {
            format!("Repeats every {unit}")
        } else {
            format!("Repeats every {interval} {unit}s")
        }
    };
    match rule.frequency {
        RecurrenceFrequency::Daily => every("day"),
        RecurrenceFrequency::Weekly if !rule.days_of_week.is_empty() => {
            let names = rule
                .days_of_week
                .iter()
                .map(|day| match day.as_str() {
                    "MO" => "Monday",
                    "TU" => "Tuesday",
                    "WE" => "Wednesday",
                    "TH" => "Thursday",
                    "FR" => "Friday",
                    "SA" => "Saturday",
                    "SU" => "Sunday",
                    other => other,
                })
                .collect::<Vec<_>>()
                .join(", ");
            if interval == 1 {
                format!("Repeats every week on {names}")
            } else {
                format!("Repeats every {interval} weeks on {names}")
            }
        }
        RecurrenceFrequency::Weekly => every("week"),
        RecurrenceFrequency::Monthly => every("month"),
        RecurrenceFrequency::Yearly => every("year"),
    }
}

#[allow(dead_code)]
fn format_recurrence(rules: &[RecurrenceRule]) -> String {
    let Some(rule) = rules.first() else {
        return "none".into();
    };
    let frequency = match rule.frequency {
        RecurrenceFrequency::Daily => "daily",
        RecurrenceFrequency::Weekly => "weekly",
        RecurrenceFrequency::Monthly => "monthly",
        RecurrenceFrequency::Yearly => "yearly",
    };
    if rule.days_of_week.is_empty() {
        format!("{frequency}:{}", rule.interval)
    } else {
        format!(
            "{frequency}:{}:{}",
            rule.interval,
            rule.days_of_week.join(",")
        )
    }
}

pub fn spawn_worker(
    backend: Arc<dyn CalendarBackend>,
    cache: Cache,
    refresh_seconds: u64,
    cache_past_days: u32,
    cache_future_days: u32,
) -> (
    mpsc::UnboundedSender<WorkerCommand>,
    mpsc::UnboundedReceiver<WorkerUpdate>,
) {
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut changes = backend.subscribe_changes();
        let mut backend_states = backend.subscribe_backend_state();
        let refresh_period = std::time::Duration::from_secs(refresh_seconds.max(5));
        // The initial synchronize below already refreshes authoritatively.
        // Schedule the periodic tick after one full period so it cannot race a
        // change-notification regression test or duplicate startup I/O.
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + refresh_period, refresh_period);
        let mut pending_change_refresh = None::<tokio::time::Instant>;
        let _ = synchronize(
            &backend,
            &cache,
            &updates_tx,
            cache_past_days,
            cache_future_days,
        )
        .await;
        loop {
            tokio::select! {
                Some(command) = commands_rx.recv() => {
                    match command {
                        WorkerCommand::Refresh => { let _ = synchronize(&backend, &cache, &updates_tx, cache_past_days, cache_future_days).await; }
                        WorkerCommand::EnsureRange(request) => {
                            load_visible_range(&backend, &cache, &updates_tx, request, false).await;
                        }
                        WorkerCommand::RefreshRange(request) => {
                            load_visible_range(&backend, &cache, &updates_tx, request, true).await;
                        }
                        WorkerCommand::SetCalendarEnabled(id, enabled) => {
                            match cache.set_calendar_enabled(&id, enabled).and_then(|_| cache.load_snapshot()) {
                                Ok(snapshot) => { let _ = updates_tx.send(WorkerUpdate::Snapshot(snapshot)); }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::Error(error.to_string())); }
                            }
                        }
                        WorkerCommand::CreateCalendar(request) => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSaving);
                            match backend.create_calendar(request).await {
                                Ok(calendar) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarCreateSucceeded(calendar));
                                    refresh_calendar_metadata(&backend, &cache, &updates_tx).await;
                                }
                                Err(error) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarCreateFailed(calendar_error_from_backend(error)));
                                }
                            }
                        }
                        WorkerCommand::RenameCalendar(request) => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSaving);
                            match backend.rename_calendar(request).await {
                                Ok(calendar) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarRenameSucceeded(calendar));
                                    refresh_calendar_metadata(&backend, &cache, &updates_tx).await;
                                }
                                Err(error) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarRenameFailed(calendar_error_from_backend(error)));
                                }
                            }
                        }
                        WorkerCommand::SetCalendarColor(request) => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSaving);
                            match backend.set_calendar_color(request).await {
                                Ok(calendar) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarColorSucceeded(calendar));
                                    refresh_calendar_metadata(&backend, &cache, &updates_tx).await;
                                }
                                Err(error) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarColorFailed(calendar_error_from_backend(error)));
                                }
                            }
                        }
                        WorkerCommand::DeleteCalendar(request) => {
                            match backend.delete_calendar(request).await {
                                Ok(response) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarDeleteSucceeded(response));
                                    refresh_calendar_metadata(&backend, &cache, &updates_tx).await;
                                }
                                Err(error) => {
                                    let _ = updates_tx.send(WorkerUpdate::CalendarDeleteFailed(calendar_error_from_backend(error)));
                                }
                            }
                        }
                        WorkerCommand::Create(event) => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSaving);
                            match backend.create_event(event).await {
                                Ok(created) => {
                                    let effect = MutationEffect::Created { event_id: created.id.clone(), interval: (created.start, created.end) };
                                    let _ = updates_tx.send(WorkerUpdate::MutationSucceeded(effect.clone()));
                                    refresh_mutation_effect(&backend, &cache, &updates_tx, &effect).await;
                                }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::MutationFailed(format!("Unable to save event: {error}"))); }
                            }
                        }
                        WorkerCommand::CreateWithSession { session, event } => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSavingFor(session));
                            match backend.create_event(event).await {
                                Ok(created) => {
                                    let effect = MutationEffect::Created { event_id: created.id.clone(), interval: (created.start, created.end) };
                                    let _ = updates_tx.send(WorkerUpdate::MutationSucceededFor(session, effect.clone()));
                                    refresh_mutation_effect(&backend, &cache, &updates_tx, &effect).await;
                                }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::MutationFailedFor(session, format!("Unable to save event: {error}"))); }
                            }
                        }
                        WorkerCommand::Update(event, span, recurrence_scope, alarm_mutation, time_mutation) => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSaving);
                            let before = event.occurrence_id.as_deref().and_then(|id| cache.load_snapshot().ok().and_then(|snapshot| snapshot.events.into_iter().find(|item| item.id == id))).map(|item| (item.start, item.end));
                            match backend.update_event(event, span, alarm_mutation, time_mutation).await {
                                Ok(updated) => {
                                    let effect = MutationEffect::Updated {
                                        event_id: updated.id.clone(),
                                        before_interval: before.unwrap_or((updated.start, updated.end)),
                                        after_interval: (updated.start, updated.end),
                                        recurrence_scope,
                                    };
                                    let _ = updates_tx.send(WorkerUpdate::MutationSucceeded(effect.clone()));
                                    refresh_mutation_effect(&backend, &cache, &updates_tx, &effect).await;
                                }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::MutationFailed(format!("Unable to save event: {error}"))); }
                            }
                        }
                        WorkerCommand::UpdateWithSession { session, event, span, recurrence_scope, alarms, time_mutation } => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSavingFor(session));
                            let before = event.occurrence_id.as_deref().and_then(|id| cache.load_snapshot().ok().and_then(|snapshot| snapshot.events.into_iter().find(|item| item.id == id))).map(|item| (item.start, item.end));
                            match backend.update_event(event, span, alarms, time_mutation).await {
                                Ok(updated) => {
                                    let effect = MutationEffect::Updated {
                                        event_id: updated.id.clone(),
                                        before_interval: before.unwrap_or((updated.start, updated.end)),
                                        after_interval: (updated.start, updated.end),
                                        recurrence_scope,
                                    };
                                    let _ = updates_tx.send(WorkerUpdate::MutationSucceededFor(session, effect.clone()));
                                    refresh_mutation_effect(&backend, &cache, &updates_tx, &effect).await;
                                }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::MutationFailedFor(session, format!("Unable to save event: {error}"))); }
                            }
                        }
                        WorkerCommand::Delete(id, span, recurrence_scope) => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSaving);
                            let selected = cache.load_snapshot().ok().and_then(|snapshot| snapshot.events.into_iter().find(|item| item.id == id));
                            let before = selected.as_ref().map(|item| (item.start, item.end));
                            let target = selected.and_then(|event| event.provider_id.map(|provider_id| crate::model::EventMutationTarget { provider_id, calendar_id: event.calendar_id, occurrence_start: event.start }));
                            let result = match target {
                                Some(target) => backend.delete_event(target, span).await,
                                None => Err(BackendError::Invalid("event provider identity is unavailable; refresh before deleting".into())),
                            };
                            match result {
                                Ok(_) => {
                                    if let Some(interval) = before {
                                        let effect = MutationEffect::Deleted { event_id: id, interval, recurrence_scope };
                                        let _ = updates_tx.send(WorkerUpdate::MutationSucceeded(effect.clone()));
                                        refresh_mutation_effect(&backend, &cache, &updates_tx, &effect).await;
                                    } else {
                                        let _ = updates_tx.send(WorkerUpdate::MutationSucceeded(MutationEffect::Deleted {
                                            event_id: id,
                                            interval: (Utc::now(), Utc::now() + Duration::days(1)),
                                            recurrence_scope,
                                        }));
                                    }
                                }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::MutationFailed(format!("Unable to delete event: {error}"))); }
                            }
                        }
                        WorkerCommand::DeleteWithSession { session, event_id: id, span, recurrence_scope } => {
                            let _ = updates_tx.send(WorkerUpdate::MutationSavingFor(session));
                            let selected = cache.load_snapshot().ok().and_then(|snapshot| snapshot.events.into_iter().find(|item| item.id == id));
                            let before = selected.as_ref().map(|item| (item.start, item.end));
                            let target = selected.and_then(|event| event.provider_id.map(|provider_id| crate::model::EventMutationTarget { provider_id, calendar_id: event.calendar_id, occurrence_start: event.start }));
                            let result = match target {
                                Some(target) => backend.delete_event(target, span).await,
                                None => Err(BackendError::Invalid("event provider identity is unavailable; refresh before deleting".into())),
                            };
                            match result {
                                Ok(_) => {
                                    if let Some(interval) = before {
                                        let effect = MutationEffect::Deleted { event_id: id, interval, recurrence_scope };
                                        let _ = updates_tx.send(WorkerUpdate::MutationSucceededFor(session, effect.clone()));
                                        refresh_mutation_effect(&backend, &cache, &updates_tx, &effect).await;
                                    } else {
                                        let _ = updates_tx.send(WorkerUpdate::MutationSucceededFor(session, MutationEffect::Deleted {
                                            event_id: id,
                                            interval: (Utc::now(), Utc::now() + Duration::days(1)),
                                            recurrence_scope,
                                        }));
                                    }
                                }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::MutationFailedFor(session, format!("Unable to delete event: {error}"))); }
                            }
                        }
                        WorkerCommand::OpenUrl(url) => {
                            match tokio::process::Command::new("open").arg(&url).status().await {
                                Ok(status) if status.success() => { let _ = updates_tx.send(WorkerUpdate::Status("Opened event link".into())); }
                                Ok(status) => { let _ = updates_tx.send(WorkerUpdate::Error(format!("macOS open exited with {status}"))); }
                                Err(error) => { let _ = updates_tx.send(WorkerUpdate::Error(format!("Could not open link: {error}"))); }
                            }
                        }
                    }
                }
                _ = changes.recv() => {
                    // EventKit exposes only a store-wide notification, without a
                    // changed event or calendar identity. A refresh must therefore
                    // override existing fetched coverage; ordinary EnsureRange
                    // would otherwise consider a stale complete range loaded.
                    cache_lifecycle_log(
                        "eventkit_change received; changed_identity=unavailable refresh=scheduled delay=750ms coverage=overridden",
                    );
                    // EventKit often emits a burst for one logical edit. Coalesce it
                    // without delaying input or an explicit refresh command.
                    pending_change_refresh = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(750));
                }
                Ok(state) = backend_states.recv() => {
                    let _ = updates_tx.send(WorkerUpdate::BackendState(state));
                }
                _ = async {
                    match pending_change_refresh {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    pending_change_refresh = None;
                    cache_lifecycle_log("eventkit_change refresh=started scope=cache-window");
                    let _ = synchronize(&backend, &cache, &updates_tx, cache_past_days, cache_future_days).await;
                }
                _ = interval.tick() => { let _ = synchronize(&backend, &cache, &updates_tx, cache_past_days, cache_future_days).await; }
                else => break,
            }
        }
    });
    (commands_tx, updates_rx)
}

fn cache_lifecycle_log(message: &str) {
    if std::env::var_os("TUI_CALENDAR_DEBUG").is_some() {
        eprintln!("tui-calendar cache {message}");
    }
}

fn calendar_error_from_backend(error: BackendError) -> CalendarError {
    match error {
        BackendError::Calendar(error) => error,
        BackendError::PermissionDenied => CalendarError::PermissionDenied,
        BackendError::Unsupported(_) => CalendarError::Unsupported,
        _ => CalendarError::Internal,
    }
}

async fn refresh_calendar_metadata(
    backend: &Arc<dyn CalendarBackend>,
    cache: &Cache,
    updates: &mpsc::UnboundedSender<WorkerUpdate>,
) {
    match backend.calendars().await {
        Ok(calendars) => match cache
            .save_calendars(&calendars)
            .and_then(|_| cache.load_snapshot())
        {
            Ok(snapshot) => {
                let _ = updates.send(WorkerUpdate::Snapshot(snapshot));
            }
            Err(error) => {
                let _ = updates.send(WorkerUpdate::Error(format!(
                    "Calendar created, but local metadata refresh failed: {error}"
                )));
            }
        },
        Err(error) => {
            let _ = updates.send(WorkerUpdate::Error(format!(
                "Calendar created, but metadata refresh failed: {error}"
            )));
        }
    }
}

async fn refresh_mutation_effect(
    backend: &Arc<dyn CalendarBackend>,
    cache: &Cache,
    updates: &mpsc::UnboundedSender<WorkerUpdate>,
    effect: &MutationEffect,
) {
    const MARGIN_HOURS: i64 = 24;
    let (intervals, future_start) = match effect {
        MutationEffect::Created { interval, .. } => (vec![*interval], None),
        MutationEffect::Deleted {
            interval,
            recurrence_scope,
            ..
        } => (
            vec![*interval],
            (*recurrence_scope == Some(RecurrenceMutationScope::FutureEvents))
                .then_some(interval.0),
        ),
        MutationEffect::Updated {
            before_interval,
            after_interval,
            recurrence_scope,
            ..
        } => (
            vec![*before_interval, *after_interval],
            (*recurrence_scope == Some(RecurrenceMutationScope::FutureEvents))
                .then_some(before_interval.0),
        ),
    };
    // Ordinary and ThisEvent changes retain their narrow refreshes. When a
    // FutureEvents suffix is already covered, replay precisely those compact
    // segments instead: extending a safety margin into their gaps would turn
    // previously unloaded time into fetched coverage.
    let (refreshes, refreshes_are_covered) = if let Some(start) = future_start {
        match cache.fetched_ranges_from(start) {
            Ok(covered) if !covered.is_empty() => (
                covered
                    .into_iter()
                    .map(|range| (range.start, range.end))
                    .collect(),
                true,
            ),
            Ok(_) => (intervals, false),
            Err(error) => {
                let _ = updates.send(WorkerUpdate::Error(format!(
                    "Event saved, but reading loaded future coverage failed: {error}"
                )));
                return;
            }
        }
    } else {
        (intervals, false)
    };
    let loader = RangeLoader::new(backend.clone(), cache.clone());
    for (start, end) in refreshes {
        let request = RangeRequest {
            id: 0,
            start: if refreshes_are_covered {
                start
            } else {
                start - Duration::hours(MARGIN_HOURS)
            },
            end: if refreshes_are_covered {
                end
            } else {
                end + Duration::hours(MARGIN_HOURS)
            },
            all_day_range: None,
            reason: RangeReason::EventKitChange,
            priority: RangePriority::Critical,
        };
        match loader.refresh_range(request).await {
            Ok(snapshot) => {
                let _ = updates.send(WorkerUpdate::Snapshot(snapshot));
            }
            Err(error) => {
                let _ = updates.send(WorkerUpdate::Error(format!(
                    "Event saved, but refreshing its range failed: {error}"
                )));
            }
        }
    }
}

async fn load_visible_range(
    backend: &Arc<dyn CalendarBackend>,
    cache: &Cache,
    updates: &mpsc::UnboundedSender<WorkerUpdate>,
    request: RangeRequest,
    force_refresh: bool,
) {
    let _ = updates.send(WorkerUpdate::RangeStarted(request.id));
    let loader = RangeLoader::new(backend.clone(), cache.clone());
    let result = if force_refresh {
        loader.refresh_range(request).await
    } else {
        loader.ensure_range(request).await
    };
    match result {
        Ok(snapshot) => {
            let _ = updates.send(WorkerUpdate::Snapshot(snapshot));
            let _ = updates.send(WorkerUpdate::RangeLoaded(request.id));
        }
        Err(error) => {
            let _ = updates.send(WorkerUpdate::RangeFailed(
                request.id,
                format!("Could not load requested range: {error}"),
            ));
        }
    }
}

async fn synchronize(
    backend: &Arc<dyn CalendarBackend>,
    cache: &Cache,
    updates: &mpsc::UnboundedSender<WorkerUpdate>,
    cache_past_days: u32,
    cache_future_days: u32,
) -> Result<(), ()> {
    let _ = updates.send(WorkerUpdate::Syncing(true));
    let result = async {
        let mut authorization = backend.authorization_status().await.map_err(|e| e.to_string())?;
        if authorization == AuthorizationStatus::NotDetermined {
            let _ = updates.send(WorkerUpdate::Status("Terminal Calendar requires access to macOS calendars.".into()));
            authorization = backend.request_access().await.map_err(|e| e.to_string())?;
        }
        cache.save_authorization(authorization).map_err(|e| e.to_string())?;
        if authorization != AuthorizationStatus::FullAccess {
            return Err(match authorization {
                AuthorizationStatus::Denied => "Calendar access denied. Enable it in System Settings → Privacy & Security → Calendars.".into(),
                AuthorizationStatus::Restricted => "Calendar access is restricted by macOS policy.".into(),
                AuthorizationStatus::WriteOnly => "Full Calendar access is required to browse events.".into(),
                _ => "Calendar access is not available.".into(),
            });
        }
        if let Ok(capabilities) = backend.calendar_capabilities().await {
            let _ = updates.send(WorkerUpdate::CalendarCapabilities(capabilities));
        }
        if let Ok(sources) = backend.calendar_sources().await {
            let _ = updates.send(WorkerUpdate::CalendarSources(sources));
        }
        let now = Utc::now();
        let start = now - Duration::days(i64::from(cache_past_days));
        let end = now + Duration::days(i64::from(cache_future_days));
        cache_lifecycle_log(&format!(
            "refresh=authoritative range=[{}, {}) fetched_ranges=overridden",
            start.to_rfc3339(),
            end.to_rfc3339()
        ));
        let loader = RangeLoader::new(backend.clone(), cache.clone());
        let snapshot = loader
            .refresh_range(RangeRequest {
                id: 0,
                start,
                end,
                all_day_range: None,
                reason: RangeReason::BackgroundRefresh,
                priority: RangePriority::Background,
            })
            .await?;
        let _ = updates.send(WorkerUpdate::Snapshot(snapshot));
        cache_lifecycle_log("refresh=completed snapshot=updated");
        Ok::<(), String>(())
    }.await;
    let succeeded = result.is_ok();
    if let Err(error) = result {
        let _ = updates.send(WorkerUpdate::Error(error));
    }
    let _ = updates.send(WorkerUpdate::Syncing(false));
    if succeeded { Ok(()) } else { Err(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app_with_mock_events() -> (App, crate::backend::MockBackend) {
        let backend = crate::backend::MockBackend::seeded();
        let snapshot = Snapshot {
            calendars: backend.calendars().await.unwrap(),
            events: backend
                .events(
                    Utc::now() - Duration::days(7),
                    Utc::now() + Duration::days(7),
                    &[],
                )
                .await
                .unwrap(),
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        };
        (App::new(Config::default(), snapshot), backend)
    }

    fn timeline_event(
        template: &Event,
        day: NaiveDate,
        start: u16,
        end: u16,
        title: &str,
    ) -> Event {
        let mut event = template.clone();
        event.id = format!("viewport-{title}-{start}");
        event.title = title.into();
        event.all_day = false;
        event.all_day_start_date = None;
        event.all_day_end_date_exclusive = None;
        event.start = local_midnight(day) + Duration::minutes(i64::from(start));
        event.end = local_midnight(day) + Duration::minutes(i64::from(end));
        event
    }

    #[tokio::test]
    async fn smart_timeline_focus_ignores_long_background_events() {
        let (mut app, _) = app_with_mock_events().await;
        let day = NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        let template = app.snapshot.events[0].clone();
        app.snapshot.events = vec![
            timeline_event(&template, day, 0, 24 * 60, "background"),
            timeline_event(&template, day, 8 * 60, 17 * 60, "workday"),
            timeline_event(&template, day, 9 * 60, 9 * 60 + 30, "QNAP"),
            timeline_event(&template, day, 9 * 60 + 45, 10 * 60, "Stand-up"),
            timeline_event(&template, day, 12 * 60 + 30, 13 * 60 + 30, "Music"),
            timeline_event(&template, day, 15 * 60 + 45, 16 * 60, "Review"),
        ];
        app.active_date = day;
        app.view = View::Day;
        let now = Local
            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .single()
            .unwrap();

        let start = app.smart_timeline_start_minute_at(16, now);
        assert!((7 * 60..=9 * 60).contains(&start), "start={start}");
    }

    #[tokio::test]
    async fn smart_timeline_focus_keeps_legitimate_early_appointments() {
        let (mut app, _) = app_with_mock_events().await;
        let day = NaiveDate::from_ymd_opt(2026, 8, 28).unwrap();
        let template = app.snapshot.events[0].clone();
        app.snapshot.events = vec![
            timeline_event(&template, day, 30, 60, "Early one"),
            timeline_event(&template, day, 90, 120, "Early two"),
            timeline_event(&template, day, 180, 210, "Early three"),
        ];
        app.active_date = day;
        app.view = View::Day;
        let now = Local
            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .single()
            .unwrap();
        assert!(app.smart_timeline_start_minute_at(8, now) <= 30);
    }

    #[tokio::test]
    async fn smart_timeline_today_can_bias_toward_an_imminent_appointment() {
        let (mut app, _) = app_with_mock_events().await;
        let day = NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        let template = app.snapshot.events[0].clone();
        app.snapshot.events = vec![timeline_event(
            &template,
            day,
            12 * 60 + 30,
            13 * 60,
            "Next",
        )];
        app.active_date = day;
        app.view = View::Day;
        let now = Local
            .with_ymd_and_hms(2026, 8, 27, 12, 0, 0)
            .single()
            .unwrap();
        let start = app.smart_timeline_start_minute_at(8, now);
        assert!((10 * 60..=12 * 60).contains(&start), "start={start}");
    }

    #[tokio::test]
    async fn manual_timeline_scroll_survives_refresh_but_date_navigation_reenables_auto() {
        let (mut app, _) = app_with_mock_events().await;
        app.view = View::Day;
        app.refresh_auto_timeline_viewport(12);
        app.scroll_timeline(8 * 60);
        let manual = app.timeline_start_minute;
        assert_eq!(app.timeline_viewport_owner, TimelineViewportOwner::Manual);
        app.apply_update(WorkerUpdate::Snapshot(app.snapshot.clone()));
        app.refresh_auto_timeline_viewport(12);
        assert_eq!(app.timeline_start_minute, manual);

        app.navigate_date(1);
        assert_eq!(app.timeline_viewport_owner, TimelineViewportOwner::Auto);
    }

    #[tokio::test]
    async fn week_uses_the_same_smart_timeline_focus_policy() {
        let (mut app, _) = app_with_mock_events().await;
        let day = NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();
        let template = app.snapshot.events[0].clone();
        app.snapshot.events = vec![
            timeline_event(&template, day + Duration::days(2), 0, 24 * 60, "background"),
            timeline_event(
                &template,
                day + Duration::days(2),
                9 * 60,
                9 * 60 + 30,
                "Planning",
            ),
            timeline_event(
                &template,
                day + Duration::days(4),
                15 * 60,
                16 * 60,
                "Review",
            ),
        ];
        app.active_date = day;
        app.view = View::Week;
        let now = Local
            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .single()
            .unwrap();
        assert_ne!(app.smart_timeline_start_minute_at(12, now), 0);
    }

    #[tokio::test]
    async fn eventkit_change_refreshes_a_complete_but_stale_cache_range() {
        let backend = Arc::new(crate::backend::MockBackend::seeded());
        let template = backend
            .events(
                Utc::now() - Duration::days(1),
                Utc::now() + Duration::days(1),
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
                event.id = format!("change-refresh-{index}");
                event.start = Utc::now() + Duration::minutes(10 + i64::from(index) * 30);
                event.end = event.start + Duration::minutes(20);
                event
            })
            .collect::<Vec<_>>();
        backend.set_events_for_test(provider_events[..4].to_vec());
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::open(directory.path().join("cache.db")).unwrap();
        let (commands, mut updates) = spawn_worker(backend.clone(), cache.clone(), 3600, 1, 1);

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(WorkerUpdate::Snapshot(snapshot)) = updates.recv().await
                    && snapshot.events.len() == 4
                {
                    break;
                }
            }
        })
        .await
        .expect("initial stale snapshot must load");
        assert!(cache.stats().unwrap().fetched_range_count > 0);

        backend.set_events_for_test(provider_events);
        backend.notify_change_for_test();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(WorkerUpdate::Snapshot(snapshot)) = updates.recv().await
                    && snapshot.events.len() == 8
                {
                    break;
                }
            }
        })
        .await
        .expect("store change must authoritatively refresh stale coverage");

        assert_eq!(cache.load_snapshot().unwrap().events.len(), 8);
        drop(commands);
    }

    fn pointer(
        x: u16,
        y: u16,
        action: PointerAction,
        button: Option<crate::input::PointerButton>,
    ) -> PointerEvent {
        PointerEvent {
            position: Some(crate::input::PointerPosition { x, y }),
            button,
            action,
        }
    }

    fn timed_drag_geometry(event_id: &str) -> CalendarHitGeometry {
        CalendarHitGeometry::Day(crate::hit_test::TimelineHitGeometry {
            day_columns: vec![crate::hit_test::CalendarDayColumn {
                date: Local::now().date_naive(),
                rect: crate::hit_test::ScreenRect::new(10, 10, 20, 10),
            }],
            all_day_area: None,
            timed_area: crate::hit_test::ScreenRect::new(10, 10, 20, 10),
            viewport: crate::layout::TimelineViewport {
                start_minute: 8 * 60,
                minutes_per_row: 60,
                rows: 10,
            },
            event_regions: vec![crate::hit_test::CalendarEventRegion {
                event_id: event_id.into(),
                rect: crate::hit_test::ScreenRect::new(10, 10, 20, 1),
            }],
        })
    }

    fn recurring_occurrences(source: &Event) -> Vec<Event> {
        (1..=5)
            .map(|index| {
                let mut event = source.clone();
                event.id = format!("A{index}");
                event.start = source.start + Duration::days(i64::from(index - 1));
                event.end = event.start + Duration::minutes(30);
                event
            })
            .collect()
    }

    #[tokio::test]
    async fn future_events_refresh_reconciles_only_loaded_suffix_segments() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut source = backend
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
        let start = source.start;
        let start_date = start.date_naive();
        source.all_day = true;
        source.all_day_start_date = Some(start_date);
        source.all_day_end_date_exclusive = Some(start_date + Duration::days(1));
        let occurrence = |id: &str, day: i64| {
            let mut event = source.clone();
            event.id = id.into();
            event.start = start + Duration::days(day);
            event.end = event.start + Duration::days(1);
            event.all_day_start_date = Some(start_date + Duration::days(day));
            event.all_day_end_date_exclusive = Some(start_date + Duration::days(day + 1));
            event
        };
        let original = [0, 1, 2, 3, 5]
            .into_iter()
            .enumerate()
            .map(|(index, day)| occurrence(&format!("A{}", index + 1), day))
            .collect::<Vec<_>>();
        let authoritative = vec![
            original[0].clone(),
            original[1].clone(),
            occurrence("B3", 2),
            occurrence("B4", 3),
            occurrence("B5", 5),
        ];
        backend.set_events_for_test(authoritative);

        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("cache.db")).unwrap();
        cache.save_calendars(&calendars).unwrap();
        // Two loaded segments with an intentional one-day hole. The second
        // segment proves FutureEvents refreshes coverage beyond the selected
        // occurrence without loading the gap.
        cache
            .replace_events(start, start + Duration::days(4), &original[..4])
            .unwrap();
        cache
            .replace_events(
                start + Duration::days(5),
                start + Duration::days(6),
                &original[4..],
            )
            .unwrap();
        let (updates, mut receiver) = mpsc::unbounded_channel();
        let effect = MutationEffect::Updated {
            event_id: "B3".into(),
            before_interval: (original[2].start, original[2].end),
            after_interval: (
                start + Duration::days(2),
                start + Duration::days(2) + Duration::minutes(30),
            ),
            recurrence_scope: Some(RecurrenceMutationScope::FutureEvents),
        };
        let backend: Arc<dyn CalendarBackend> = Arc::new(backend);
        refresh_mutation_effect(&backend, &cache, &updates, &effect).await;
        while receiver.try_recv().is_ok() {}

        let ids = cache
            .load_snapshot()
            .unwrap()
            .events
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["A1", "A2", "B3", "B4", "B5"]);
        assert!(
            cache
                .load_snapshot()
                .unwrap()
                .events
                .iter()
                .all(|event| event.all_day_date_range().is_some()),
            "split-series reconciliation must retain trusted all-day identity"
        );
        assert!(
            !cache
                .range_is_fetched(start + Duration::days(4), start + Duration::days(5))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn mutation_response_identity_is_preferred_after_a_split_snapshot() {
        let (mut app, _) = app_with_mock_events().await;
        let occurrences = recurring_occurrences(&app.snapshot.events[0]);
        app.snapshot.events = occurrences.clone();
        app.view = View::Agenda;
        app.selected_event = 2;
        let mut replacement = occurrences[2].clone();
        replacement.id = "B3".into();
        let mut refreshed = vec![occurrences[0].clone(), occurrences[1].clone(), replacement];
        for (index, occurrence) in occurrences.iter().enumerate().skip(3) {
            let mut event = occurrence.clone();
            event.id = format!("B{}", index + 1);
            refreshed.push(event);
        }

        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: "B3".into(),
            before_interval: (occurrences[2].start, occurrences[2].end),
            after_interval: (occurrences[2].start, occurrences[2].end),
            recurrence_scope: Some(RecurrenceMutationScope::FutureEvents),
        }));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: app.snapshot.calendars.clone(),
            events: refreshed,
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.selected_event_ref().unwrap().id, "B3");
    }

    #[tokio::test]
    async fn removed_recurring_selection_clears_focus_and_closes_details() {
        let (mut app, _) = app_with_mock_events().await;
        let occurrences = recurring_occurrences(&app.snapshot.events[0]);
        app.snapshot.events = occurrences.clone();
        app.view = View::Agenda;
        app.selected_event = 2;
        app.mode = Mode::Details;
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: app.snapshot.calendars.clone(),
            events: occurrences[..2].to_vec(),
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.selected_event_ref().is_none());
    }

    #[tokio::test]
    async fn empty_snapshot_clears_a_deleted_recurring_selection_safely() {
        let (mut app, _) = app_with_mock_events().await;
        app.snapshot.events = recurring_occurrences(&app.snapshot.events[0]);
        app.view = View::Agenda;
        app.selected_event = 2;
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: app.snapshot.calendars.clone(),
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert!(app.selected_event_ref().is_none());
        assert_eq!(app.selected_event, usize::MAX);
    }

    fn key(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn command_palette_filters_actions_and_disables_event_mutations_without_selection() {
        let (mut app, _) = app_with_mock_events().await;
        app.snapshot.events.clear();
        app.palette_query = "edit".into();

        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, PaletteCommand::EditEvent);
        assert!(!entries[0].enabled);
        assert_eq!(entries[0].unavailable_reason(), "Select an event first");

        app.mode = Mode::Palette;
        app.handle_palette(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Palette);
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Select an event first")
        );
    }

    #[tokio::test]
    async fn command_palette_reuses_existing_event_and_recurring_flows() {
        let (mut app, _) = app_with_mock_events().await;

        app.palette_query = "create".into();
        assert!(
            app.palette_entries()
                .iter()
                .any(|entry| entry.command == PaletteCommand::NewEvent && entry.enabled)
        );
        app.execute_palette(PaletteCommand::NewEvent);
        assert_eq!(app.mode, Mode::Form);

        app.mode = Mode::Normal;
        app.selected_event = 0;
        app.execute_palette(PaletteCommand::EditEvent);
        assert_eq!(app.mode, Mode::RecurringEditScope);

        app.mode = Mode::Normal;
        app.execute_palette(PaletteCommand::DeleteEvent);
        assert_eq!(app.mode, Mode::RecurringDeleteScope);

        app.mode = Mode::Normal;
        app.selected_event = 1;
        app.execute_palette(PaletteCommand::DuplicateEvent);
        assert_eq!(app.mode, Mode::Form);
    }

    #[tokio::test]
    async fn keyboard_palette_and_details_dispatch_the_same_event_action() {
        let (mut keyboard, _) = app_with_mock_events().await;
        keyboard.selected_event = 0;
        keyboard.handle_key(key('e'));
        assert_eq!(keyboard.mode, Mode::RecurringEditScope);

        let (mut palette, _) = app_with_mock_events().await;
        palette.selected_event = 0;
        palette.handle_key(key(':'));
        palette.execute_palette(PaletteCommand::EditEvent);
        assert_eq!(palette.mode, Mode::RecurringEditScope);

        let (mut details, _) = app_with_mock_events().await;
        details.selected_event = 0;
        details.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        details.handle_key(key('e'));
        assert_eq!(details.mode, Mode::RecurringEditScope);

        assert_eq!(
            keyboard.pending_recurring_mutation,
            palette.pending_recurring_mutation
        );
        assert_eq!(
            keyboard.pending_recurring_mutation,
            details.pending_recurring_mutation
        );
    }

    #[tokio::test]
    async fn keyboard_palette_and_details_dispatch_the_same_delete_workflow() {
        // Use a non-recurring event so every source reaches the shared delete
        // confirmation rather than stopping at a recurrence-scope chooser.
        let (mut keyboard, _) = app_with_mock_events().await;
        keyboard.selected_event = 1;
        let event_id = keyboard.selected_event_ref().unwrap().id.clone();
        keyboard.handle_key(key('d'));

        let (mut palette, _) = app_with_mock_events().await;
        palette.selected_event = 1;
        palette.handle_key(key(':'));
        palette.execute_palette(PaletteCommand::DeleteEvent);

        let (mut details, _) = app_with_mock_events().await;
        details.selected_event = 1;
        details.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        details.handle_key(key('d'));

        for app in [&keyboard, &palette, &details] {
            assert_eq!(app.mode, Mode::Delete);
            assert_eq!(
                app.pending_delete_event_id.as_deref(),
                Some(event_id.as_str())
            );
            assert_eq!(app.delete_span, EventSpan::ThisEvent);
            assert_eq!(app.delete_recurrence_scope, None);
        }
    }

    #[tokio::test]
    async fn action_dispatcher_rejects_a_stale_event_identity() {
        let (mut app, _) = app_with_mock_events().await;
        app.execute_action(UserAction::DeleteEvent("removed-event".into()));

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Event no longer exists")
        );

        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        app.snapshot
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == event.calendar_id)
            .unwrap()
            .is_writable = false;
        assert!(!app.start_drag_session(
            event.id.clone(),
            CalendarHitTarget::ExistingEvent { event_id: event.id },
        ));
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("This calendar is read-only")
        );
    }

    #[tokio::test]
    async fn undo_history_is_committed_only_after_provider_confirmation() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        app.selected_event = app
            .visible_events()
            .iter()
            .position(|candidate| candidate.id == event.id)
            .unwrap();
        let command = app
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .expect("a movable event must produce an update");
        app.note_dispatched_mutation(&command);
        assert!(
            app.undo_stack.is_empty(),
            "unconfirmed work is not undoable"
        );

        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        assert_eq!(app.undo_stack.len(), 1);
        assert!(app.redo_stack.is_empty());
        app.apply_update(WorkerUpdate::Snapshot(app.snapshot.clone()));
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some(event.id.as_str())
        );
        assert_eq!(
            app.undo_stack.len(),
            1,
            "authoritative refresh retains confirmed history"
        );

        let Some(WorkerCommand::Update(draft, span, scope, alarms, mutation)) =
            app.execute_action(UserAction::Undo)
        else {
            panic!("confirmed move must be reversible");
        };
        assert_eq!(draft.time.as_timed_range(), Some((event.start, event.end)));
        assert_eq!(span, EventSpan::ThisEvent);
        assert_eq!(scope, None);
        assert_eq!(alarms, AlarmMutation::Replace(event.alarms.clone()));
        assert_eq!(mutation, EventTimeMutation::ReplaceLegacy);

        app.note_dispatched_mutation(&WorkerCommand::Update(draft, span, scope, alarms, mutation));
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            after_interval: (event.start, event.end),
            recurrence_scope: None,
        }));
        assert!(app.undo_stack.is_empty());
        assert_eq!(app.redo_stack.len(), 1);

        let Some(WorkerCommand::Update(draft, _, _, _, _)) = app.execute_action(UserAction::Redo)
        else {
            panic!("confirmed undo must be redoable");
        };
        assert_eq!(
            draft.time.as_timed_range(),
            Some((
                event.start + Duration::hours(1),
                event.end + Duration::hours(1)
            ))
        );
        app.note_dispatched_mutation(&WorkerCommand::Update(
            draft,
            EventSpan::ThisEvent,
            None,
            AlarmMutation::Replace(event.alarms.clone()),
            EventTimeMutation::ReplaceLegacy,
        ));
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id,
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        assert_eq!(app.undo_stack.len(), 1);
        assert!(app.redo_stack.is_empty());
    }

    #[tokio::test]
    async fn undo_history_preserves_all_day_dates_and_protected_alarms() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter_mut()
            .find(|event| event.id == "mock-review")
            .unwrap();
        event.alarms = vec![Alarm {
            relative_seconds: None,
            absolute_date: Some(event.start),
            is_editable: false,
        }];
        let event = event.clone();
        let command = app
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .unwrap();
        app.note_dispatched_mutation(&command);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        let Some(WorkerCommand::Update(_, _, _, AlarmMutation::Preserve, _)) =
            app.execute_action(UserAction::Undo)
        else {
            panic!("undo must preserve protected alarms rather than rewrite them");
        };

        let (mut app, _) = app_with_mock_events().await;
        let mut all_day = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let all_day_start = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        all_day.id = "undo-trusted-all-day".into();
        all_day.calendar_id = "personal".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(all_day_start);
        all_day.all_day_end_date_exclusive = Some(all_day_start + Duration::days(2));
        app.snapshot.events.push(all_day.clone());
        let delete = WorkerCommand::Delete(all_day.id.clone(), EventSpan::ThisEvent, None);
        app.note_dispatched_mutation(&delete);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Deleted {
            event_id: all_day.id.clone(),
            interval: (all_day.start, all_day.end),
            recurrence_scope: None,
        }));
        let Some(WorkerCommand::Create(draft)) = app.execute_action(UserAction::Undo) else {
            panic!("a confirmed non-recurring deletion must be restorable");
        };
        assert_eq!(
            draft.time.as_all_day_range(),
            all_day.all_day_date_range(),
            "undo must restore floating all-day dates, never compatibility instants"
        );

        let (mut app, _) = app_with_mock_events().await;
        let mut all_day = app.snapshot.events[0].clone();
        let start_date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        all_day.id = "undo-all-day-move".into();
        all_day.calendar_id = "personal".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(start_date);
        all_day.all_day_end_date_exclusive = Some(start_date + Duration::days(2));
        all_day.has_recurrence = false;
        all_day.recurrence.clear();
        app.snapshot.events = vec![all_day.clone()];
        let moved = App::reversible_event_draft(&all_day, Some(all_day.id.clone()), false)
            .unwrap()
            .draft;
        let moved_start = start_date + Duration::days(3);
        let moved_end = moved_start + Duration::days(2);
        let move_command = WorkerCommand::Update(
            moved.clone(),
            EventSpan::ThisEvent,
            None,
            AlarmMutation::Preserve,
            EventTimeMutation::ReplaceAllDay {
                start_date: moved_start,
                end_date_exclusive: moved_end,
            },
        );
        app.note_dispatched_mutation(&move_command);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: all_day.id.clone(),
            before_interval: (all_day.start, all_day.end),
            after_interval: (all_day.start, all_day.end),
            recurrence_scope: None,
        }));
        assert_eq!(app.undo_stack.len(), 1);
        let Some(WorkerCommand::Update(
            undo_draft,
            _,
            _,
            _,
            EventTimeMutation::ReplaceAllDay {
                start_date: undo_start,
                end_date_exclusive: undo_end,
            },
        )) = app.execute_action(UserAction::Undo)
        else {
            panic!("trusted all-day update must be reversible");
        };
        assert!(matches!(
            undo_draft.time,
            EventTimeInput::LegacyAllDayUnknown { .. }
        ));
        assert_eq!(
            (undo_start, undo_end),
            (start_date, start_date + Duration::days(2))
        );
    }

    #[tokio::test]
    async fn failed_and_recurring_mutations_do_not_create_unsafe_history() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let draft = App::reversible_event_draft(&event, None, true)
            .unwrap()
            .draft;
        let create = WorkerCommand::Create(draft);
        app.note_dispatched_mutation(&create);
        app.apply_update(WorkerUpdate::MutationFailed(
            "provider rejected create".into(),
        ));
        assert!(app.undo_stack.is_empty());
        assert!(app.redo_stack.is_empty());

        app.note_dispatched_mutation(&create);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Created {
            event_id: "provider-created".into(),
            interval: (event.start, event.end),
        }));
        let mut snapshot = app.snapshot.clone();
        let mut created = event.clone();
        created.id = "provider-created".into();
        created.has_recurrence = false;
        created.recurrence.clear();
        snapshot.events.push(created);
        app.apply_update(WorkerUpdate::Snapshot(snapshot));
        assert_eq!(app.undo_stack.len(), 1);
        // The provider-created identity is recorded only after success, so an
        // undo deletes the exact event instead of guessing from its title.
        let Some(WorkerCommand::Delete(id, EventSpan::ThisEvent, None)) =
            app.execute_action(UserAction::Undo)
        else {
            panic!("a confirmed non-recurring create must be reversible");
        };
        assert_eq!(id, "provider-created");
        app.note_dispatched_mutation(&WorkerCommand::Delete(
            id.clone(),
            EventSpan::ThisEvent,
            None,
        ));
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Deleted {
            event_id: id,
            interval: (event.start, event.end),
            recurrence_scope: None,
        }));
        assert!(matches!(
            app.execute_action(UserAction::Redo),
            Some(WorkerCommand::Create(_))
        ));

        let (mut app, _) = app_with_mock_events().await;
        let recurring = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-standup")
            .unwrap()
            .clone();
        let update = WorkerCommand::Update(
            App::reversible_event_draft(&recurring, Some(recurring.id.clone()), false)
                .unwrap()
                .draft,
            EventSpan::FutureEvents,
            Some(RecurrenceMutationScope::FutureEvents),
            AlarmMutation::Preserve,
            EventTimeMutation::ReplaceLegacy,
        );
        app.note_dispatched_mutation(&update);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: recurring.id.clone(),
            before_interval: (recurring.start, recurring.end),
            after_interval: (recurring.start, recurring.end),
            recurrence_scope: Some(RecurrenceMutationScope::FutureEvents),
        }));
        assert!(
            app.undo_stack.is_empty(),
            "series/split scope is intentionally not reversible yet"
        );
    }

    #[tokio::test]
    async fn undo_history_is_bounded() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let draft = App::reversible_event_draft(&event, None, true)
            .unwrap()
            .draft;
        for index in 0..=UNDO_HISTORY_LIMIT {
            let command = WorkerCommand::Create(draft.clone());
            app.note_dispatched_mutation(&command);
            app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Created {
                event_id: format!("history-{index}"),
                interval: (event.start, event.end),
            }));
        }
        assert_eq!(app.undo_stack.len(), UNDO_HISTORY_LIMIT);
        assert!(matches!(
            app.undo_stack.first(),
            Some(UndoRecord::Created {
                created_event_id,
                ..
            }) if created_event_id == "history-1"
        ));
    }

    #[tokio::test]
    async fn pending_history_does_not_duplicate_and_new_successful_work_clears_redo() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let first = app
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .unwrap();
        app.note_dispatched_mutation(&first);
        app.note_dispatched_mutation(&first);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        assert_eq!(app.undo_stack.len(), 1, "one request creates one record");

        let undo = app.execute_action(UserAction::Undo).unwrap();
        app.note_dispatched_mutation(&undo);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            after_interval: (event.start, event.end),
            recurrence_scope: None,
        }));
        assert_eq!(app.redo_stack.len(), 1);

        let redo = app.execute_action(UserAction::Redo).unwrap();
        app.note_dispatched_mutation(&redo);
        app.apply_update(WorkerUpdate::MutationFailed(
            "provider rejected redo".into(),
        ));
        assert!(app.undo_stack.is_empty());
        assert_eq!(app.redo_stack.len(), 1, "failed redo must remain retryable");

        let second = app
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(2),
                    end: event.end + Duration::hours(2),
                },
            })
            .unwrap();
        app.note_dispatched_mutation(&second);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id,
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(2),
                event.end + Duration::hours(2),
            ),
            recurrence_scope: None,
        }));
        assert_eq!(app.undo_stack.len(), 1);
        assert!(app.redo_stack.is_empty());
    }

    #[tokio::test]
    async fn stale_or_read_only_history_is_retained_without_dispatching_an_inverse() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let command = app
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .unwrap();
        app.note_dispatched_mutation(&command);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));

        app.snapshot
            .events
            .retain(|candidate| candidate.id != event.id);
        assert!(app.execute_action(UserAction::Undo).is_none());
        assert_eq!(app.undo_stack.len(), 1, "stale history must not be popped");
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Provider state changed; event no longer exists")
        );

        app.snapshot.events.push(event.clone());
        app.snapshot
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == event.calendar_id)
            .unwrap()
            .is_writable = false;
        assert!(app.execute_action(UserAction::Undo).is_none());
        assert_eq!(
            app.undo_stack.len(),
            1,
            "permission failures retain history"
        );
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("This calendar is read-only")
        );
    }

    #[tokio::test]
    async fn undo_shortcut_and_palette_dispatch_the_existing_history_actions() {
        let (mut keyboard, _) = app_with_mock_events().await;
        let event = keyboard
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let command = keyboard
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .unwrap();
        keyboard.note_dispatched_mutation(&command);
        keyboard.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        let undo = keyboard.handle_key(key('u'));
        assert!(matches!(
            undo.as_ref(),
            Some(WorkerCommand::Update(_, EventSpan::ThisEvent, None, _, _))
        ));
        keyboard.note_dispatched_mutation(undo.as_ref().unwrap());
        keyboard.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id.clone(),
            before_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            after_interval: (event.start, event.end),
            recurrence_scope: None,
        }));
        assert!(matches!(
            keyboard.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(WorkerCommand::Update(_, EventSpan::ThisEvent, None, _, _))
        ));

        let (mut palette, _) = app_with_mock_events().await;
        let command = palette
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .unwrap();
        palette.note_dispatched_mutation(&command);
        palette.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id,
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        let undo_entry = palette
            .palette_entries()
            .into_iter()
            .find(|entry| entry.command == PaletteCommand::Undo)
            .unwrap();
        assert!(undo_entry.enabled);
        assert!(matches!(
            palette.execute_palette(PaletteCommand::Undo),
            Some(WorkerCommand::Update(_, EventSpan::ThisEvent, None, _, _))
        ));
    }

    #[tokio::test]
    async fn undo_controls_explain_unavailable_and_recurring_history_states() {
        let (mut app, _) = app_with_mock_events().await;
        let undo_entry = app
            .palette_entries()
            .into_iter()
            .find(|entry| entry.command == PaletteCommand::Undo)
            .unwrap();
        let redo_entry = app
            .palette_entries()
            .into_iter()
            .find(|entry| entry.command == PaletteCommand::Redo)
            .unwrap();
        assert!(!undo_entry.enabled);
        assert_eq!(undo_entry.unavailable_reason(), "Nothing to undo");
        assert!(!redo_entry.enabled);
        assert_eq!(redo_entry.unavailable_reason(), "Nothing to redo");
        assert!(app.handle_key(key('u')).is_none());
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Nothing to undo")
        );
        app.mode = Mode::Palette;
        app.palette_query = "undo".into();
        app.handle_palette(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Nothing to undo")
        );

        let recurring = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-standup")
            .unwrap()
            .clone();
        let update = WorkerCommand::Update(
            App::reversible_event_draft(&recurring, Some(recurring.id.clone()), false)
                .unwrap()
                .draft,
            EventSpan::FutureEvents,
            Some(RecurrenceMutationScope::FutureEvents),
            AlarmMutation::Preserve,
            EventTimeMutation::ReplaceLegacy,
        );
        app.note_dispatched_mutation(&update);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: recurring.id,
            before_interval: (recurring.start, recurring.end),
            after_interval: (recurring.start, recurring.end),
            recurrence_scope: Some(RecurrenceMutationScope::FutureEvents),
        }));
        let undo_entry = app
            .palette_entries()
            .into_iter()
            .find(|entry| entry.command == PaletteCommand::Undo)
            .unwrap();
        assert!(!undo_entry.enabled, "recurring changes must stay excluded");
        assert_eq!(undo_entry.unavailable_reason(), "Nothing to undo");
    }

    #[tokio::test]
    async fn undo_restores_the_exact_editable_alarm_collection() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter_mut()
            .find(|event| event.id == "mock-review")
            .unwrap();
        event.alarms = vec![Alarm {
            relative_seconds: Some(-15 * 60),
            absolute_date: None,
            is_editable: true,
        }];
        let event = event.clone();
        let command = app
            .execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target: EventMoveTarget::Timed {
                    start: event.start + Duration::hours(1),
                    end: event.end + Duration::hours(1),
                },
            })
            .unwrap();
        app.note_dispatched_mutation(&command);
        app.apply_update(WorkerUpdate::MutationSucceeded(MutationEffect::Updated {
            event_id: event.id,
            before_interval: (event.start, event.end),
            after_interval: (
                event.start + Duration::hours(1),
                event.end + Duration::hours(1),
            ),
            recurrence_scope: None,
        }));
        let Some(WorkerCommand::Update(_, _, _, AlarmMutation::Replace(alarms), _)) =
            app.execute_action(UserAction::Undo)
        else {
            panic!("editable alarm state must be restored exactly");
        };
        assert_eq!(alarms, event.alarms);
    }

    #[tokio::test]
    async fn command_palette_respects_read_only_calendar_permissions() {
        let (mut app, _) = app_with_mock_events().await;
        app.snapshot
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == "work")
            .unwrap()
            .is_writable = false;
        app.selected_event = 0;

        for command in [
            PaletteCommand::EditEvent,
            PaletteCommand::DuplicateEvent,
            PaletteCommand::DeleteEvent,
        ] {
            let entry = app
                .palette_entries()
                .into_iter()
                .find(|entry| entry.command == command)
                .unwrap();
            assert!(!entry.enabled);
            assert_eq!(entry.unavailable_reason(), "This calendar is read-only");
        }
    }

    #[tokio::test]
    async fn modal_stack_returns_palette_after_cancelling_date_jump() {
        let (mut app, _) = app_with_mock_events().await;

        app.handle_key(key(':'));
        assert_eq!(app.mode, Mode::Palette);
        assert_eq!(app.modal_stack.len(), 1);
        app.execute_palette(PaletteCommand::GoToDate);
        assert_eq!(app.mode, Mode::DateJump);
        assert_eq!(app.modal_stack.len(), 2);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Palette);
        assert_eq!(app.modal_stack.len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.modal_stack.is_empty());
    }

    #[tokio::test]
    async fn details_actions_return_to_details_without_losing_context() {
        let (mut app, _) = app_with_mock_events().await;
        app.selected_event = 1;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);
        app.detail_scroll = 4;
        let selected_id = app.selected_event_ref().unwrap().id.clone();

        app.handle_key(key('e'));
        assert_eq!(app.mode, Mode::Form);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);
        assert_eq!(app.detail_scroll, 4);
        assert_eq!(app.selected_event_ref().unwrap().id, selected_id);

        app.handle_key(key('d'));
        assert_eq!(app.mode, Mode::Delete);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);
        assert_eq!(app.selected_event_ref().unwrap().id, selected_id);
    }

    #[tokio::test]
    async fn recurring_scope_and_dirty_discard_confirm_restore_their_callers() {
        let (mut app, _) = app_with_mock_events().await;
        app.selected_event = 0;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_key(key('e'));
        assert_eq!(app.mode, Mode::RecurringEditScope);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);

        app.leave_modal();
        app.selected_event = 1;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_key(key('e'));
        app.handle_key(key('x'));
        assert!(app.form_dirty);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::DiscardConfirm);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Form);
        assert!(app.form.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::DiscardConfirm);
        app.handle_key(key('n'));
        assert_eq!(app.mode, Mode::Details);
        assert!(app.form.is_none());
    }

    fn alarm_key(state: &mut AlarmEditorState, key: KeyEvent) -> Result<bool, String> {
        state
            .handle_key(key, "UTC")
            .expect("alarm key should be handled")
    }

    #[test]
    fn parses_relative_and_absolute_alarms() {
        let alarms = parse_alarms("15m, 1d, @2026-09-01T12:00:00Z").unwrap();
        assert_eq!(alarms[0].relative_seconds, Some(-900));
        assert_eq!(alarms[1].relative_seconds, Some(-86400));
        assert!(alarms[2].absolute_date.is_some());
    }

    #[test]
    fn alarm_editor_preserves_unchanged_basic_alarms_and_protects_lossy_forms() {
        let basic = Alarm {
            relative_seconds: Some(-900),
            absolute_date: None,
            is_editable: true,
        };
        let state = AlarmEditorState::from_existing(std::slice::from_ref(&basic));
        assert_eq!(
            state.draft_alarms_and_mutation(true).unwrap().1,
            AlarmMutation::Preserve
        );

        let non_minute = Alarm {
            relative_seconds: Some(-630),
            absolute_date: None,
            is_editable: true,
        };
        let protected = AlarmEditorState::from_existing(&[basic, non_minute]);
        assert!(protected.is_protected());
        assert_eq!(
            protected.draft_alarms_and_mutation(true).unwrap().1,
            AlarmMutation::Preserve
        );

        let absolute = Alarm {
            relative_seconds: None,
            absolute_date: Some(Utc::now()),
            is_editable: true,
        };
        assert!(!AlarmEditorState::from_existing(&[absolute]).is_protected());
    }

    #[test]
    fn all_day_relative_alarms_are_protected_while_explicit_absolute_is_allowed() {
        for seconds in [0, -900, -86_400] {
            assert!(
                AlarmEditorState::from_event_alarms(
                    &[Alarm {
                        relative_seconds: Some(seconds),
                        absolute_date: None,
                        is_editable: true
                    }],
                    true,
                    TimeZoneProvenance::ExplicitEvent,
                )
                .is_protected()
            );
        }
        let absolute = Alarm {
            relative_seconds: None,
            absolute_date: Some(Utc::now()),
            is_editable: true,
        };
        assert!(
            !AlarmEditorState::from_event_alarms(
                std::slice::from_ref(&absolute),
                true,
                TimeZoneProvenance::ExplicitEvent
            )
            .is_protected()
        );
        assert!(
            AlarmEditorState::from_event_alarms(
                &[absolute],
                true,
                TimeZoneProvenance::HelperFallback
            )
            .is_protected()
        );
    }

    #[test]
    fn editable_alarm_round_trips_relative_absolute_and_safe_mixed_collections() {
        let instant = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let relative = Alarm {
            relative_seconds: Some(-900),
            absolute_date: None,
            is_editable: true,
        };
        let absolute = Alarm {
            relative_seconds: None,
            absolute_date: Some(instant),
            is_editable: true,
        };
        assert_eq!(
            EditableAlarm::from_alarm(&relative).unwrap(),
            EditableAlarm::Relative {
                relative_seconds: -900
            }
        );
        assert_eq!(
            EditableAlarm::from_alarm(&relative)
                .unwrap()
                .to_alarm()
                .unwrap(),
            relative
        );
        assert_eq!(
            EditableAlarm::from_alarm(&absolute).unwrap(),
            EditableAlarm::Absolute { date_time: instant }
        );
        assert_eq!(
            EditableAlarm::from_alarm(&absolute)
                .unwrap()
                .to_alarm()
                .unwrap(),
            absolute
        );

        let state = AlarmEditorState::from_existing(&[relative.clone(), absolute.clone()]);
        let AlarmEditorState::EditableBasic { data } = &state else {
            panic!("lossless mixed collection must be structurally represented");
        };
        assert_eq!(data.alarms.len(), 2);
        assert_eq!(data.to_alarms().unwrap(), vec![relative, absolute]);
        assert_eq!(
            state.draft_alarms_and_mutation(true).unwrap().1,
            AlarmMutation::Preserve
        );
        assert!(
            state
                .display_in_timezone("UTC")
                .contains("1 Sep 2026, 12:00")
        );
    }

    #[test]
    fn invalid_or_provider_marked_absolute_alarms_remain_protected() {
        let instant = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        for alarm in [
            Alarm {
                relative_seconds: Some(-900),
                absolute_date: Some(instant),
                is_editable: true,
            },
            Alarm {
                relative_seconds: None,
                absolute_date: None,
                is_editable: true,
            },
            Alarm {
                relative_seconds: None,
                absolute_date: Some(instant),
                is_editable: false,
            },
        ] {
            assert!(AlarmEditorState::from_existing(&[alarm]).is_protected());
        }
    }

    #[test]
    fn changed_absolute_alarm_materializes_replace_without_losing_the_instant() {
        let first = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let second = Utc.with_ymd_and_hms(2026, 9, 2, 12, 0, 0).unwrap();
        let original = Alarm {
            relative_seconds: None,
            absolute_date: Some(first),
            is_editable: true,
        };
        let mut state = AlarmEditorState::from_existing(&[original]);
        let AlarmEditorState::EditableBasic { data } = &mut state else {
            panic!("basic absolute alarm should be structurally represented");
        };
        data.alarms[0] = EditableAlarm::Absolute { date_time: second };
        assert_eq!(
            state.draft_alarms_and_mutation(true).unwrap().1,
            AlarmMutation::Replace(vec![Alarm {
                relative_seconds: None,
                absolute_date: Some(second),
                is_editable: true,
            }])
        );
    }

    #[test]
    fn absolute_structural_state_does_not_capture_form_navigation_or_escape() {
        let state = AlarmEditorState::from_existing(&[Alarm {
            relative_seconds: None,
            absolute_date: Some(Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap()),
            is_editable: true,
        }]);
        let mut state = state;
        assert!(
            state
                .handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), "UTC")
                .is_none()
        );
        assert!(
            state
                .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), "UTC")
                .is_none()
        );
    }

    #[test]
    fn absolute_alarm_resolution_is_timezone_and_dst_safe() {
        let AbsoluteResolution::Single(instant) =
            resolve_absolute_alarm("2026-09-15", "12:30", "Europe/Berlin").unwrap()
        else {
            panic!("ordinary time must resolve once")
        };
        assert_eq!(
            absolute_edit_buffers(instant, "Europe/Berlin").unwrap(),
            ("2026-09-15".into(), "12:30".into())
        );
        assert_eq!(
            resolve_absolute_alarm("2026-03-29", "02:30", "Europe/Berlin"),
            Err(AlarmValidationError::NonexistentLocalTime)
        );
        let AbsoluteResolution::Ambiguous(first, second) =
            resolve_absolute_alarm("2026-10-25", "02:30", "Europe/Berlin").unwrap()
        else {
            panic!("fall-back time must require a choice")
        };
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn duplicate_copies_lossless_relative_and_absolute_alarms_into_create_payload() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut event = backend
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
        let instant = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        event.alarms.push(Alarm {
            relative_seconds: None,
            absolute_date: Some(instant),
            is_editable: true,
        });
        let form = EventForm::duplicate_from(&event, &calendars);
        let (draft, _, mutation) = form.to_draft(&calendars).unwrap();
        assert!(draft.id.is_none());
        assert_eq!(draft.alarms, event.alarms);
        assert_eq!(mutation, AlarmMutation::Replace(event.alarms));
    }

    #[tokio::test]
    async fn trusted_all_day_duplicate_uses_the_original_date_range() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let event = backend
            .events(
                "2026-09-01T00:00:00Z".parse().unwrap(),
                "2026-10-01T00:00:00Z".parse().unwrap(),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.id == "mock-all-day-multi")
            .unwrap();
        let form = EventForm::duplicate_from(&event, &calendars);
        let (draft, _, _) = form.to_draft(&calendars).unwrap();
        assert_eq!(
            draft.time,
            EventTimeInput::AllDay {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            }
        );
        let _created = backend.create_event(draft).await.unwrap();
        assert_eq!(
            backend.last_created_time(),
            Some(EventTimeInput::AllDay {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            })
        );
    }

    #[tokio::test]
    async fn legacy_all_day_duplicate_is_rejected_without_a_create_request() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let event = backend
            .events(
                "2026-09-01T00:00:00Z".parse().unwrap(),
                "2026-10-01T00:00:00Z".parse().unwrap(),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.id == "mock-all-day-legacy")
            .unwrap();
        let form = EventForm::duplicate_from(&event, &calendars);
        assert!(
            form.to_draft(&calendars)
                .unwrap_err()
                .contains("Cannot safely duplicate legacy all-day")
        );
        assert_eq!(backend.last_created_time(), None);
    }

    #[tokio::test]
    async fn all_day_duplicate_preserves_dates_across_dst_without_elapsed_time_math() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut event = backend
            .events(
                "2026-09-01T00:00:00Z".parse().unwrap(),
                "2026-10-01T00:00:00Z".parse().unwrap(),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.id == "mock-all-day-multi")
            .unwrap();
        event.all_day_start_date = Some(NaiveDate::from_ymd_opt(2026, 3, 28).unwrap());
        event.all_day_end_date_exclusive = Some(NaiveDate::from_ymd_opt(2026, 3, 31).unwrap());
        let form = EventForm::duplicate_from(&event, &calendars);
        let (draft, _, _) = form.to_draft(&calendars).unwrap();
        assert_eq!(
            draft.time.as_all_day_range(),
            Some((
                NaiveDate::from_ymd_opt(2026, 3, 28).unwrap(),
                NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
            ))
        );
    }

    #[tokio::test]
    async fn event_form_create_all_day_uses_floating_dates_and_preserves_fields() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut form = EventForm::new(
            0,
            &calendars,
            NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            9 * 60,
            60,
        );
        form.title = "Conference".into();
        form.location = "Berlin".into();
        form.notes = "Bring badge".into();
        form.all_day = true;
        form.start = "2026-09-10".into();
        form.end = "2026-09-12".into();
        let (draft, _, _) = form.to_draft(&calendars).unwrap();
        assert_eq!(
            draft.time,
            EventTimeInput::AllDay {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            }
        );
        assert_eq!(draft.location, "Berlin");
        assert_eq!(draft.notes, "Bring badge");
        let created = backend.create_event(draft).await.unwrap();
        assert_eq!(
            created.all_day_date_range(),
            Some((
                NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            ))
        );
    }

    #[tokio::test]
    async fn event_form_create_all_day_keeps_single_day_and_dst_crossing_dates() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        for (start, end, exclusive) in [
            ("2026-09-10", "2026-09-10", "2026-09-11"),
            ("2026-03-28", "2026-03-30", "2026-03-31"),
        ] {
            let mut form = EventForm::new(
                0,
                &calendars,
                NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                9 * 60,
                60,
            );
            form.title = "All-day".into();
            form.all_day = true;
            form.start = start.into();
            form.end = end.into();
            let (draft, _, _) = form.to_draft(&calendars).unwrap();
            assert_eq!(
                draft.time.as_all_day_range(),
                Some((
                    NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap(),
                    NaiveDate::parse_from_str(exclusive, "%Y-%m-%d").unwrap(),
                ))
            );
        }
    }

    #[tokio::test]
    async fn event_form_create_all_day_rejects_reversed_dates_without_backend_mutation() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut form = EventForm::new(
            0,
            &calendars,
            NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            9 * 60,
            60,
        );
        form.title = "Invalid all-day".into();
        form.all_day = true;
        form.start = "2026-09-12".into();
        form.end = "2026-09-10".into();
        assert!(form.to_draft(&calendars).is_err());
        assert_eq!(backend.last_created_time(), None);
    }

    #[tokio::test]
    async fn event_form_timed_create_retains_instant_input_behavior() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut form = EventForm::new(
            0,
            &calendars,
            NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            9 * 60,
            60,
        );
        form.title = "Timed".into();
        form.start = "2026-09-10 09:00".into();
        form.end = "2026-09-10 10:00".into();
        let (draft, _, _) = form.to_draft(&calendars).unwrap();
        assert_eq!(
            draft.time.as_timed_range(),
            Some((
                parse_local("2026-09-10 09:00").unwrap(),
                parse_local("2026-09-10 10:00").unwrap(),
            ))
        );
    }

    #[tokio::test]
    async fn quick_add_all_day_create_uses_the_typed_date_range() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                ..Snapshot::default()
            },
        );
        app.quick_add_input = "Holiday 2026-09-10 all-day".into();
        let Some(WorkerCommand::Create(draft)) =
            app.handle_quick_add(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!("Quick Add must dispatch a create request");
        };
        assert_eq!(
            draft.time,
            EventTimeInput::AllDay {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            }
        );
        let created = backend.create_event(draft).await.unwrap();
        assert_eq!(
            created.all_day_date_range(),
            Some((
                NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            ))
        );
    }

    #[tokio::test]
    async fn visible_day_uses_trusted_all_day_dates_when_compatibility_instants_miss() {
        let backend = crate::backend::MockBackend::seeded();
        let snapshot = Snapshot {
            calendars: backend.calendars().await.unwrap(),
            events: backend
                .events(
                    "2026-09-10T00:00:00Z".parse().unwrap(),
                    "2026-09-11T00:00:00Z".parse().unwrap(),
                    &[],
                )
                .await
                .unwrap(),
            authorization: AuthorizationStatus::FullAccess,
            updated_at: None,
        };
        let mut app = App::new(Config::default(), snapshot);
        app.view = View::Day;
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert!(
            app.visible_events()
                .iter()
                .any(|event| event.id == "mock-all-day-shifted")
        );
    }

    #[tokio::test]
    async fn trusted_all_day_search_and_defaults_never_use_compatibility_start() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let event: Event = serde_json::from_value(serde_json::json!({
            "id": "trusted-search-event",
            "calendarId": "personal",
            "title": "Timezone-safe holiday",
            "start": "2026-09-09T00:00:00Z",
            "end": "2026-09-10T00:00:00Z",
            "allDay": true,
            "allDayStartDate": "2026-09-10",
            "allDayEndDateExclusive": "2026-09-11",
        }))
        .unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![event],
                ..Snapshot::default()
            },
        );
        app.search_query = "holiday".into();
        app.handle_search(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.active_date,
            NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            "search navigation must use the trusted floating start date"
        );
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some("trusted-search-event")
        );
        let _ = app.execute_action(UserAction::ChangeView(View::Month));
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some("trusted-search-event"),
            "search activation must retain the same occurrence across a view switch"
        );

        app.view = View::Day;
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert_eq!(app.suggested_start_minute(), 9 * 60);
    }

    #[tokio::test]
    async fn trusted_all_day_move_uses_floating_date_identity() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let event: Event = serde_json::from_value(serde_json::json!({
            "id": "trusted-reschedule-event",
            "calendarId": "personal",
            "title": "Holiday",
            "start": "2026-09-09T00:00:00Z",
            "end": "2026-09-10T00:00:00Z",
            "allDay": true,
            "allDayStartDate": "2026-09-10",
            "allDayEndDateExclusive": "2026-09-11",
        }))
        .unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![event],
                ..Snapshot::default()
            },
        );
        app.view = View::Day;
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let Some(WorkerCommand::Update(
            _,
            _,
            _,
            _,
            EventTimeMutation::ReplaceAllDay {
                start_date,
                end_date_exclusive,
            },
        )) = app.move_selected_event(1, 0)
        else {
            panic!("trusted all-day movement must generate an update request");
        };
        assert_eq!(start_date, NaiveDate::from_ymd_opt(2026, 9, 11).unwrap());
        assert_eq!(
            end_date_exclusive,
            NaiveDate::from_ymd_opt(2026, 9, 12).unwrap()
        );
    }

    #[tokio::test]
    async fn drag_session_previews_and_returns_a_timed_move_action_without_execution() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let origin = CalendarHitTarget::ExistingEvent {
            event_id: event.id.clone(),
        };
        assert!(app.start_drag_session(event.id.clone(), origin));
        assert_eq!(app.drag_session.state, DragState::Pressed);

        let target = CalendarHitTarget::TimedSlot {
            date: event.start.with_timezone(&Local).date_naive(),
            minute: 15 * 60,
        };
        assert!(app.update_drag_preview(target));
        assert_eq!(app.drag_session.state, DragState::Preview);
        assert!(matches!(
            app.drag_preview(),
            Some(DragPreview::Timed {
                proposed_start,
                proposed_end,
                ..
            }) if proposed_start.with_timezone(&Local).hour() == 15
                && proposed_end - proposed_start == event.end - event.start
        ));

        let Some(UserAction::MoveEvent { event_id, target }) = app.drop_drag_session() else {
            panic!("a valid preview must produce, but not execute, a move action");
        };
        assert_eq!(event_id, event.id);
        let EventMoveTarget::Timed { start, end } = target else {
            panic!("timed event must keep a timed move target");
        };
        assert_eq!(start.with_timezone(&Local).hour(), 15);
        assert_eq!(end - start, event.end - event.start);
        assert_eq!(app.drag_session.state, DragState::Dropped);
    }

    #[tokio::test]
    async fn drag_session_can_cancel_and_rejects_invalid_or_stale_drops() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        assert!(app.start_drag_session(
            event.id.clone(),
            CalendarHitTarget::ExistingEvent {
                event_id: event.id.clone(),
            },
        ));
        assert!(!app.update_drag_preview(CalendarHitTarget::OutsideCalendar));
        assert_eq!(app.drag_session.state, DragState::Dragging);
        assert!(app.drop_drag_session().is_none());

        app.cancel_drag_session();
        assert_eq!(app.drag_session, DragSession::default());

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
        app.snapshot.events.clear();
        assert!(app.drop_drag_session().is_none());
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Event no longer exists")
        );
    }

    #[tokio::test]
    async fn drag_session_all_day_preview_uses_calendar_dates_without_instants() {
        let (mut app, _) = app_with_mock_events().await;
        let start = Local::now().date_naive();
        let mut event = app.snapshot.events[0].clone();
        event.id = "drag-all-day".into();
        event.calendar_id = "personal".into();
        event.all_day = true;
        event.all_day_start_date = Some(start);
        event.all_day_end_date_exclusive = Some(start + Duration::days(3));
        app.snapshot.events = vec![event];
        app.view = View::Day;
        app.active_date = start;
        let event = app.snapshot.events[0].clone();
        assert!(app.start_drag_session(
            event.id.clone(),
            CalendarHitTarget::ExistingEvent {
                event_id: event.id.clone(),
            },
        ));
        assert!(app.update_drag_preview(CalendarHitTarget::AllDayRow {
            date: start + Duration::days(5),
        }));
        assert!(matches!(
            app.drag_preview(),
            Some(DragPreview::AllDay {
                proposed_start_date,
                proposed_end_date_exclusive,
                ..
            }) if proposed_start_date == start + Duration::days(5)
                && proposed_end_date_exclusive == start + Duration::days(8)
        ));

        let Some(UserAction::MoveEvent {
            target:
                EventMoveTarget::AllDay {
                    start_date,
                    end_date_exclusive,
                },
            ..
        }) = app.drop_drag_session()
        else {
            panic!("trusted all-day preview must produce a calendar-date intent");
        };
        assert_eq!(start_date, start + Duration::days(5));
        assert_eq!(end_date_exclusive, start + Duration::days(8));
    }

    #[tokio::test]
    async fn pointer_press_move_release_only_produces_a_move_intent() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let snapshot = app.snapshot.events.clone();
        let geometry = timed_drag_geometry(&event.id);

        assert!(
            app.handle_pointer_with_hit_test(
                pointer(
                    12,
                    10,
                    PointerAction::Press,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .is_none()
        );
        assert_eq!(app.drag_session.state, DragState::Pressed);
        assert!(
            app.handle_pointer_with_hit_test(
                pointer(12, 17, PointerAction::Move, None),
                Some(&geometry),
            )
            .is_none()
        );
        assert_eq!(app.drag_session.state, DragState::Preview);

        let Some(UserAction::MoveEvent { event_id, target }) = app.handle_pointer_with_hit_test(
            pointer(
                12,
                17,
                PointerAction::Release,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        ) else {
            panic!("pointer release must create, but not dispatch, a move intent");
        };
        assert_eq!(event_id, event.id);
        let EventMoveTarget::Timed { start, end } = target else {
            panic!("timed pointer target must preserve a timed move");
        };
        assert_eq!(start.with_timezone(&Local).hour(), 15);
        assert_eq!(end - start, event.end - event.start);
        assert_eq!(app.snapshot.events, snapshot);
        assert_eq!(app.drag_session.state, DragState::Dropped);
    }

    #[tokio::test]
    async fn pointer_drag_ignores_empty_presses_and_cancels_stale_or_all_day_sessions() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let geometry = timed_drag_geometry(&event.id);
        assert!(
            app.handle_pointer_with_hit_test(
                pointer(
                    1,
                    1,
                    PointerAction::Press,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .is_none()
        );
        assert_eq!(app.drag_session, DragSession::default());

        let _ = app.handle_pointer_with_hit_test(
            pointer(
                12,
                10,
                PointerAction::Press,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        );
        app.snapshot.events.clear();
        let _ = app.handle_pointer_with_hit_test(
            pointer(12, 17, PointerAction::Move, None),
            Some(&geometry),
        );
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Event no longer exists")
        );
        let _ = app.handle_pointer_with_hit_test(PointerEvent::cancel(), Some(&geometry));
        assert_eq!(app.drag_session, DragSession::default());

        let (mut app, _) = app_with_mock_events().await;
        let start = Local::now().date_naive();
        let mut all_day = app.snapshot.events[0].clone();
        all_day.id = "pointer-all-day".into();
        all_day.calendar_id = "personal".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(start);
        all_day.all_day_end_date_exclusive = Some(start + Duration::days(2));
        all_day.recurrence.clear();
        all_day.has_recurrence = false;
        app.snapshot.events = vec![all_day.clone()];
        app.view = View::Day;
        app.active_date = start;
        let geometry = CalendarHitGeometry::Day(crate::hit_test::TimelineHitGeometry {
            day_columns: vec![crate::hit_test::CalendarDayColumn {
                date: start,
                rect: crate::hit_test::ScreenRect::new(10, 2, 20, 10),
            }],
            all_day_area: Some(crate::hit_test::ScreenRect::new(10, 4, 20, 1)),
            timed_area: crate::hit_test::ScreenRect::new(10, 5, 20, 7),
            viewport: crate::layout::TimelineViewport {
                start_minute: 8 * 60,
                minutes_per_row: 60,
                rows: 7,
            },
            event_regions: vec![crate::hit_test::CalendarEventRegion {
                event_id: all_day.id.clone(),
                rect: crate::hit_test::ScreenRect::new(10, 4, 5, 1),
            }],
        });
        let _ = app.handle_pointer_with_hit_test(
            pointer(
                12,
                4,
                PointerAction::Press,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        );
        let Some(UserAction::MoveEvent {
            target:
                EventMoveTarget::AllDay {
                    start_date,
                    end_date_exclusive,
                },
            ..
        }) = app.handle_pointer_with_hit_test(
            pointer(
                20,
                4,
                PointerAction::Release,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        )
        else {
            panic!("all-day pointer release must preserve a calendar-date move intent");
        };
        assert_eq!(start_date, start);
        assert_eq!(end_date_exclusive, start + Duration::days(2));
    }

    #[tokio::test]
    async fn dispatched_pointer_timed_move_reuses_the_existing_update_workflow() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let snapshot = app.snapshot.events.clone();
        let geometry = timed_drag_geometry(&event.id);
        let _ = app.handle_pointer_with_hit_test(
            pointer(
                12,
                10,
                PointerAction::Press,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        );
        let action = app
            .handle_pointer_with_hit_test(
                pointer(
                    12,
                    17,
                    PointerAction::Release,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .expect("valid pointer release must produce a move action");
        app.cancel_drag_session();
        let Some(WorkerCommand::Update(draft, span, scope, alarms, mutation)) =
            app.execute_action(action)
        else {
            panic!("pointer move must enter the existing update workflow");
        };
        assert_eq!(span, EventSpan::ThisEvent);
        assert_eq!(scope, None);
        assert_eq!(alarms, AlarmMutation::Preserve);
        assert_eq!(mutation, EventTimeMutation::ReplaceLegacy);
        assert_eq!(
            draft
                .time
                .as_timed_range()
                .map(|(start, _)| start.with_timezone(&Local).hour()),
            Some(15)
        );
        assert_eq!(app.drag_session, DragSession::default());
        assert_eq!(app.snapshot.events, snapshot, "no optimistic pointer move");

        app.apply_update(WorkerUpdate::Error("simulated move failure".into()));
        assert_eq!(app.drag_session, DragSession::default());
        assert_eq!(app.snapshot.events, snapshot);
    }

    #[tokio::test]
    async fn keyboard_and_pointer_moves_dispatch_the_same_typed_update() {
        let (mut keyboard, _) = app_with_mock_events().await;
        keyboard.config.event.move_step_minutes = 60;
        keyboard.selected_event = 1;
        let Some(WorkerCommand::Update(
            keyboard_draft,
            keyboard_span,
            keyboard_scope,
            keyboard_alarms,
            keyboard_time_mutation,
        )) = keyboard.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT))
        else {
            panic!("keyboard move must enter the shared update workflow");
        };

        let (mut pointer_app, _) = app_with_mock_events().await;
        let event = pointer_app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let geometry = timed_drag_geometry(&event.id);
        let _ = pointer_app.handle_pointer_with_hit_test(
            pointer(
                12,
                10,
                PointerAction::Press,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        );
        let action = pointer_app
            .handle_pointer_with_hit_test(
                pointer(
                    12,
                    16,
                    PointerAction::Release,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .expect("pointer release must produce the same 60-minute move intent");
        pointer_app.cancel_drag_session();
        let Some(WorkerCommand::Update(
            pointer_draft,
            pointer_span,
            pointer_scope,
            pointer_alarms,
            pointer_time_mutation,
        )) = pointer_app.execute_action(action)
        else {
            panic!("pointer move must enter the shared update workflow");
        };

        assert_eq!(pointer_draft, keyboard_draft);
        assert_eq!(pointer_span, keyboard_span);
        assert_eq!(pointer_scope, keyboard_scope);
        assert_eq!(pointer_alarms, keyboard_alarms);
        assert_eq!(pointer_time_mutation, keyboard_time_mutation);
    }

    #[tokio::test]
    async fn dispatched_pointer_all_day_and_recurring_moves_preserve_existing_safety() {
        let (mut app, _) = app_with_mock_events().await;
        let start = Local::now().date_naive();
        let mut all_day = app.snapshot.events[0].clone();
        all_day.id = "dispatch-all-day".into();
        all_day.calendar_id = "personal".into();
        all_day.all_day = true;
        all_day.all_day_start_date = Some(start);
        all_day.all_day_end_date_exclusive = Some(start + Duration::days(2));
        all_day.recurrence.clear();
        all_day.has_recurrence = false;
        app.snapshot.events = vec![all_day.clone()];
        app.view = View::Day;
        app.active_date = start;
        let geometry = CalendarHitGeometry::Day(crate::hit_test::TimelineHitGeometry {
            day_columns: vec![crate::hit_test::CalendarDayColumn {
                date: start,
                rect: crate::hit_test::ScreenRect::new(10, 2, 20, 10),
            }],
            all_day_area: Some(crate::hit_test::ScreenRect::new(10, 4, 20, 1)),
            timed_area: crate::hit_test::ScreenRect::new(10, 5, 20, 7),
            viewport: crate::layout::TimelineViewport {
                start_minute: 8 * 60,
                minutes_per_row: 60,
                rows: 7,
            },
            event_regions: vec![crate::hit_test::CalendarEventRegion {
                event_id: all_day.id.clone(),
                rect: crate::hit_test::ScreenRect::new(10, 4, 5, 1),
            }],
        });
        let _ = app.handle_pointer_with_hit_test(
            pointer(
                12,
                4,
                PointerAction::Press,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        );
        let action = app
            .handle_pointer_with_hit_test(
                pointer(
                    20,
                    4,
                    PointerAction::Release,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .unwrap();
        app.cancel_drag_session();
        let Some(WorkerCommand::Update(
            _,
            _,
            _,
            _,
            EventTimeMutation::ReplaceAllDay {
                start_date,
                end_date_exclusive,
            },
        )) = app.execute_action(action)
        else {
            panic!("all-day pointer move must use the existing typed update");
        };
        assert_eq!(start_date, start);
        assert_eq!(end_date_exclusive, start + Duration::days(2));

        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-standup")
            .unwrap()
            .clone();
        let geometry = timed_drag_geometry(&event.id);
        let _ = app.handle_pointer_with_hit_test(
            pointer(
                12,
                10,
                PointerAction::Press,
                Some(crate::input::PointerButton::Primary),
            ),
            Some(&geometry),
        );
        let action = app
            .handle_pointer_with_hit_test(
                pointer(
                    12,
                    17,
                    PointerAction::Release,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .unwrap();
        app.cancel_drag_session();
        assert!(app.execute_action(action).is_none());
        assert_eq!(app.mode, Mode::RecurringEditScope);
    }

    #[tokio::test]
    async fn pointer_press_rejects_a_read_only_calendar_without_a_drag_session() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        app.snapshot
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == event.calendar_id)
            .unwrap()
            .is_writable = false;
        let geometry = timed_drag_geometry(&event.id);
        assert!(
            app.handle_pointer_with_hit_test(
                pointer(
                    12,
                    10,
                    PointerAction::Press,
                    Some(crate::input::PointerButton::Primary),
                ),
                Some(&geometry),
            )
            .is_none()
        );
        assert_eq!(app.drag_session, DragSession::default());
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("This calendar is read-only")
        );
    }

    #[tokio::test]
    async fn move_action_preserves_timed_duration_and_records_the_typed_update() {
        let (mut app, backend) = app_with_mock_events().await;
        let event = app
            .snapshot
            .events
            .iter()
            .find(|event| event.id == "mock-review")
            .unwrap()
            .clone();
        let target = EventMoveTarget::Timed {
            start: event.start + Duration::hours(3),
            end: event.end + Duration::hours(3),
        };
        let Some(WorkerCommand::Update(draft, span, scope, alarms, mutation)) =
            app.execute_action(UserAction::MoveEvent {
                event_id: event.id.clone(),
                target,
            })
        else {
            panic!("timed movement must produce an update request");
        };
        assert_eq!(
            draft.time.as_timed_range(),
            Some((
                event.start + Duration::hours(3),
                event.end + Duration::hours(3)
            ))
        );
        assert_eq!(span, EventSpan::ThisEvent);
        assert_eq!(scope, None);
        assert_eq!(alarms, AlarmMutation::Preserve);
        assert_eq!(mutation, EventTimeMutation::ReplaceLegacy);

        backend
            .update_event(draft, span, alarms, mutation)
            .await
            .unwrap();
        assert_eq!(
            backend
                .last_updated_time()
                .and_then(|time| time.as_timed_range()),
            Some((
                event.start + Duration::hours(3),
                event.end + Duration::hours(3)
            ))
        );
    }

    #[tokio::test]
    async fn move_action_rejects_legacy_read_only_and_stale_events() {
        let (mut app, _) = app_with_mock_events().await;
        let legacy = Event {
            all_day: true,
            all_day_start_date: None,
            all_day_end_date_exclusive: None,
            ..app.snapshot.events[1].clone()
        };
        app.snapshot.events = vec![legacy.clone()];
        app.execute_action(UserAction::MoveEvent {
            event_id: legacy.id.clone(),
            target: EventMoveTarget::AllDay {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 12).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            },
        });
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Legacy all-day events must be refreshed before moving")
        );

        let (mut app, _) = app_with_mock_events().await;
        app.snapshot
            .calendars
            .iter_mut()
            .find(|calendar| calendar.id == "work")
            .unwrap()
            .is_writable = false;
        let event = app.snapshot.events[1].clone();
        app.execute_action(UserAction::MoveEvent {
            event_id: event.id,
            target: EventMoveTarget::Timed {
                start: event.start + Duration::hours(1),
                end: event.end + Duration::hours(1),
            },
        });
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("This calendar is read-only")
        );

        app.execute_action(UserAction::MoveEvent {
            event_id: "removed-event".into(),
            target: EventMoveTarget::Timed {
                start: Utc::now(),
                end: Utc::now() + Duration::hours(1),
            },
        });
        assert_eq!(
            app.status.as_ref().map(|(message, _, _)| message.as_str()),
            Some("Event no longer exists")
        );
    }

    #[tokio::test]
    async fn recurring_move_requires_and_uses_an_explicit_scope() {
        let (mut app, _) = app_with_mock_events().await;
        let event = app.snapshot.events[0].clone();
        app.execute_action(UserAction::MoveEvent {
            event_id: event.id.clone(),
            target: EventMoveTarget::Timed {
                start: event.start + Duration::hours(2),
                end: event.end + Duration::hours(2),
            },
        });
        assert_eq!(app.mode, Mode::RecurringEditScope);
        let Some(WorkerCommand::Update(_, span, scope, _, _)) = app.handle_key(key('2')) else {
            panic!("future recurring move must produce an update request");
        };
        assert_eq!(span, EventSpan::FutureEvents);
        assert_eq!(scope, Some(RecurrenceMutationScope::FutureEvents));
    }

    #[test]
    fn daily_navigation_updates_the_week_window_and_snapshot_refresh_keeps_the_date() {
        let mut app = App::new(Config::default(), Snapshot::default());
        let start = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        app.view = View::Week;
        app.active_date = start;

        let Some(WorkerCommand::EnsureRange(request)) = app.handle_key(key('l')) else {
            panic!("daily navigation must request the newly visible range");
        };
        assert_eq!(app.active_date, start + Duration::days(1));
        assert_eq!(
            request.all_day_range,
            Some(CalendarDateRange {
                start_date: start + Duration::days(1),
                end_date_exclusive: start + Duration::days(8),
            })
        );

        // Range loading is authoritative for cached data only. It must not
        // restore a date from the pre-navigation snapshot.
        app.apply_update(WorkerUpdate::RangeStarted(request.id));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot::default()));
        app.apply_update(WorkerUpdate::RangeLoaded(request.id));
        assert_eq!(app.active_date, start + Duration::days(1));
        assert_eq!(app.visible_range_state, VisibleRangeState::Ready);

        let Some(WorkerCommand::EnsureRange(previous)) = app.handle_key(key('h')) else {
            panic!("reverse daily navigation must request the newly visible range");
        };
        assert_eq!(app.active_date, start);
        assert_eq!(
            previous.all_day_range,
            Some(CalendarDateRange {
                start_date: start,
                end_date_exclusive: start + Duration::days(7),
            })
        );

        let Some(WorkerCommand::EnsureRange(next)) =
            app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        else {
            panic!("right arrow must use daily navigation");
        };
        assert_eq!(app.active_date, start + Duration::days(1));
        assert_eq!(next.all_day_range, request.all_day_range);

        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start);

        app.view = View::Month;
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        let september = app.visible_range_request().all_day_range;
        let Some(WorkerCommand::EnsureRange(october)) = app.handle_key(key('l')) else {
            panic!("month-boundary navigation must request the new month window");
        };
        assert_eq!(
            app.active_date,
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
        );
        assert_ne!(october.all_day_range, september);
    }

    #[test]
    fn month_navigation_uses_days_horizontally_and_weeks_vertically() {
        let mut app = App::new(Config::default(), Snapshot::default());
        let start = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        app.view = View::Month;
        app.active_date = start;
        app.selected_event = 3;

        assert!(matches!(
            app.handle_key(key('h')),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start - Duration::days(1));
        assert!(app.selected_event_ref().is_none());
        assert!(matches!(
            app.handle_key(key('l')),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start);

        assert!(matches!(
            app.handle_key(key('j')),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start + Duration::days(7));
        assert!(matches!(
            app.handle_key(key('k')),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start);

        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start + Duration::days(7));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(WorkerCommand::EnsureRange(_))
        ));
        assert_eq!(app.active_date, start);
    }

    #[tokio::test]
    async fn day_navigation_keeps_j_and_k_as_event_selection() {
        let (mut app, _) = app_with_mock_events().await;
        let mut second = app.snapshot.events[0].clone();
        second.id = "day-selection-second".into();
        second.start += Duration::minutes(1);
        second.end += Duration::minutes(1);
        app.active_date = second.start.with_timezone(&Local).date_naive();
        app.view = View::Day;
        app.snapshot.events.push(second);
        app.selected_event = 0;
        let date = app.active_date;

        assert!(app.visible_events().len() >= 2);
        assert!(app.handle_key(key('j')).is_none());
        assert_eq!(app.selected_event, 1);
        assert_eq!(app.active_date, date);
        assert!(app.handle_key(key('k')).is_none());
        assert_eq!(app.selected_event, 0);
        assert_eq!(app.active_date, date);
    }

    #[tokio::test]
    async fn month_tab_cycles_events_on_the_active_date_for_details_and_actions() {
        let (mut app, _) = app_with_mock_events().await;
        let mut first = app.snapshot.events[0].clone();
        first.id = "month-event-first".into();
        first.has_recurrence = false;
        first.recurrence.clear();
        let mut second = first.clone();
        second.id = "month-event-second".into();
        second.start += Duration::hours(1);
        second.end += Duration::hours(1);
        app.snapshot.events = vec![first.clone(), second.clone()];
        app.view = View::Month;
        app.active_date = first.start.with_timezone(&Local).date_naive();
        app.selected_event = usize::MAX;

        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some(first.id.as_str())
        );
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some(second.id.as_str())
        );
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
                .is_none()
        );
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some(first.id.as_str())
        );

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);
    }

    #[test]
    fn visible_fetch_requests_preserve_calendar_date_intent_for_each_view() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();

        app.view = View::Day;
        assert_eq!(
            app.visible_range_request().all_day_range,
            Some(CalendarDateRange {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            })
        );

        app.view = View::Week;
        assert_eq!(
            app.visible_range_request().all_day_range,
            Some(CalendarDateRange {
                start_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 9, 17).unwrap(),
            })
        );

        app.view = View::Month;
        assert_eq!(
            app.visible_range_request().all_day_range,
            Some(CalendarDateRange {
                start_date: NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
                end_date_exclusive: NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
            })
        );
    }

    #[test]
    fn alarm_editor_add_edit_delete_and_custom_values_are_structured() {
        let basic = Alarm {
            relative_seconds: Some(-900),
            absolute_date: None,
            is_editable: true,
        };
        let mut state = AlarmEditorState::from_existing(&[basic]);

        // Edit 15m to 30m, then add a custom 45m alarm through the same
        // canonical parser Quick Add already uses.
        assert!(
            !alarm_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .unwrap()
        );
        for character in "\x7f\x7f\x7f30m".chars() {
            let code = if character == '\x7f' {
                KeyCode::Backspace
            } else {
                KeyCode::Char(character)
            };
            alarm_key(&mut state, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
        }
        assert!(
            alarm_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .unwrap()
        );
        assert!(!alarm_key(&mut state, key('a')).unwrap());
        for _ in 0..ALARM_PRESETS.len() - 1 {
            alarm_key(&mut state, key('j')).unwrap();
        }
        assert!(
            !alarm_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .unwrap()
        );
        for character in "45m".chars() {
            alarm_key(&mut state, key(character)).unwrap();
        }
        assert!(
            alarm_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .unwrap()
        );
        let (alarms, mutation) = state.draft_alarms_and_mutation(true).unwrap();
        assert_eq!(
            alarms
                .iter()
                .map(|alarm| alarm.relative_seconds)
                .collect::<Vec<_>>(),
            vec![Some(-1800), Some(-2700)]
        );
        assert_eq!(mutation, AlarmMutation::Replace(alarms));

        // Delete both entries: this is an intentional clear, not Preserve.
        assert!(alarm_key(&mut state, key('d')).unwrap());
        assert!(alarm_key(&mut state, key('d')).unwrap());
        assert_eq!(
            state.draft_alarms_and_mutation(true).unwrap().1,
            AlarmMutation::Replace(vec![])
        );
    }

    #[test]
    fn alarm_editor_preserves_semantically_restored_sets_and_rejects_invalid_custom_input() {
        let original = Alarm {
            relative_seconds: Some(-900),
            absolute_date: None,
            is_editable: true,
        };
        let mut state = AlarmEditorState::from_existing(&[original]);
        alarm_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        for character in "\x7f\x7f\x7f30m".chars() {
            let code = if character == '\x7f' {
                KeyCode::Backspace
            } else {
                KeyCode::Char(character)
            };
            alarm_key(&mut state, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
        }
        alarm_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        alarm_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        for character in "\x7f\x7f\x7f15m".chars() {
            let code = if character == '\x7f' {
                KeyCode::Backspace
            } else {
                KeyCode::Char(character)
            };
            alarm_key(&mut state, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
        }
        alarm_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(
            state.draft_alarms_and_mutation(true).unwrap().1,
            AlarmMutation::Preserve
        );

        let mut empty = AlarmEditorState::from_existing(&[]);
        alarm_key(&mut empty, key('a')).unwrap();
        for _ in 0..ALARM_PRESETS.len() - 1 {
            alarm_key(&mut empty, key('j')).unwrap();
        }
        alarm_key(
            &mut empty,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        for character in "nonsense".chars() {
            alarm_key(&mut empty, key(character)).unwrap();
        }
        assert!(
            alarm_key(
                &mut empty,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .is_err()
        );
        assert!(
            empty
                .display_in_timezone("UTC")
                .contains("Custom: nonsense")
        );
    }

    #[test]
    fn parses_native_recurrence_description() {
        let rule = parse_recurrence("weekly:2:TU,TH").unwrap().remove(0);
        assert_eq!(rule.frequency, RecurrenceFrequency::Weekly);
        assert_eq!(rule.interval, 2);
        assert_eq!(rule.days_of_week, ["TU", "TH"]);
    }

    #[test]
    fn describes_weekly_recurrence_for_details() {
        let rule = parse_recurrence("weekly:2:TU,TH").unwrap();
        assert_eq!(
            humanize_recurrence(&rule),
            "Repeats every 2 weeks on Tuesday, Thursday"
        );
    }

    #[test]
    fn recurrence_end_fields_follow_structured_state_and_keep_cursor_valid() {
        let calendars = vec![CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: String::new(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        }];
        let mut form = EventForm::new(0, &calendars, Local::now().date_naive(), 9 * 60, 60);
        let rule = RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 1,
            days_of_week: vec!["MO".into()],
            occurrence_count: None,
            end_date: None,
        };
        form.recurrence = RecurrenceEditorState::Structured(RecurrenceEditorData::from_rule(rule));

        let fields = form.visible_fields();
        assert!(fields.contains(&FormField::Weekdays));
        assert!(fields.contains(&FormField::RecurrenceEnds));
        assert!(!fields.contains(&FormField::RecurrenceEndDate));
        assert!(!fields.contains(&FormField::RecurrenceOccurrences));

        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::OnDate;
        let fields = form.visible_fields();
        assert!(fields.contains(&FormField::Weekdays));
        assert!(fields.contains(&FormField::RecurrenceEndDate));
        assert!(!fields.contains(&FormField::RecurrenceOccurrences));
        form.selected = fields
            .iter()
            .position(|field| *field == FormField::RecurrenceEndDate)
            .unwrap();

        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::Never;
        form.normalize_cursor();
        let fields = form.visible_fields();
        assert!(form.selected < fields.len());
        assert_ne!(form.field(), FormField::RecurrenceEndDate);

        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::AfterOccurrences;
        let fields = form.visible_fields();
        assert!(fields.contains(&FormField::RecurrenceOccurrences));
        assert!(!fields.contains(&FormField::RecurrenceEndDate));

        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::InvalidExisting;
        let fields = form.visible_fields();
        assert!(fields.contains(&FormField::RecurrenceEnds));
        assert!(!fields.contains(&FormField::RecurrenceEndDate));
        assert!(!fields.contains(&FormField::RecurrenceOccurrences));

        form.recurrence = RecurrenceEditorState::Unsupported {
            original_rules: Vec::new(),
            summary: "Custom recurrence".into(),
        };
        let fields = form.visible_fields();
        assert!(!fields.contains(&FormField::RecurrenceEnds));
        assert!(!fields.contains(&FormField::RecurrenceEndDate));
        assert!(!fields.contains(&FormField::RecurrenceOccurrences));
    }

    #[test]
    fn recurrence_end_mode_cycles_without_mutating_the_rule_or_buffers() {
        let rule = RecurrenceRule {
            frequency: RecurrenceFrequency::Daily,
            interval: 1,
            days_of_week: vec![],
            occurrence_count: None,
            end_date: None,
        };
        let mut data = RecurrenceEditorData::from_rule(rule);
        data.end_date_buffer = "2030-01-31".into();
        data.occurrence_count_buffer = "12".into();

        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::OnDate);
        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::AfterOccurrences);
        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::Never);
        assert_eq!(data.end_date_buffer, "2030-01-31");
        assert_eq!(data.occurrence_count_buffer, "12");
        assert_eq!(data.rule.end_date, None);
        assert_eq!(data.rule.occurrence_count, None);

        data.end_mode = RecurrenceEndMode::InvalidExisting;
        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::Never);
        data.end_mode = RecurrenceEndMode::InvalidExisting;
        data.cycle_end_mode(-1);
        assert_eq!(data.end_mode, RecurrenceEndMode::AfterOccurrences);
    }

    #[test]
    fn changing_ends_marks_the_form_dirty_but_navigation_does_not() {
        let calendar = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: String::new(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars: vec![calendar],
                ..Snapshot::default()
            },
        );
        let mut form = EventForm::new(0, &app.snapshot.calendars, app.active_date, 9 * 60, 60);
        form.recurrence =
            RecurrenceEditorState::Structured(RecurrenceEditorData::from_rule(RecurrenceRule {
                frequency: RecurrenceFrequency::Daily,
                interval: 1,
                days_of_week: vec![],
                occurrence_count: None,
                end_date: None,
            }));
        form.selected = form
            .visible_fields()
            .iter()
            .position(|field| *field == FormField::RecurrenceEnds)
            .unwrap();
        app.form = Some(form);
        app.mode = Mode::Form;

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(app.form_dirty);
        assert!(
            app.form
                .as_ref()
                .unwrap()
                .visible_fields()
                .contains(&FormField::RecurrenceEndDate)
        );

        app.form_dirty = false;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!app.form_dirty);
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(app.form_dirty);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.form.as_ref().unwrap().field(),
            FormField::RecurrenceOccurrences
        );
        app.form_dirty = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(app.form_dirty);
    }

    #[test]
    fn end_date_buffer_validates_without_mutating_the_recurrence_rule() {
        let calendar = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: String::new(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        let mut form = EventForm::new(
            0,
            &[calendar],
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            9 * 60,
            60,
        );
        form.recurrence =
            RecurrenceEditorState::Structured(RecurrenceEditorData::from_rule(RecurrenceRule {
                frequency: RecurrenceFrequency::Daily,
                interval: 1,
                days_of_week: vec![],
                occurrence_count: None,
                end_date: None,
            }));
        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::OnDate;
        form.selected = form
            .visible_fields()
            .iter()
            .position(|field| *field == FormField::RecurrenceEndDate)
            .unwrap();

        for character in "2026-12-".chars() {
            form.push(character);
        }
        let RecurrenceEditorState::Structured(data) = &form.recurrence else {
            unreachable!();
        };
        assert_eq!(data.end_date_buffer, "2026-12-");
        assert_eq!(data.validated_end_date, None);
        assert_eq!(data.rule.end_date, None);

        for character in "31".chars() {
            form.push(character);
        }
        let RecurrenceEditorState::Structured(data) = &form.recurrence else {
            unreachable!();
        };
        assert_eq!(
            data.validated_end_date,
            Some(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap())
        );
        assert_eq!(data.rule.end_date, None);
        assert_eq!(form.value(&[]), "2026-12-31");
        assert!(form.recurrence.summary().contains("Until 31 Dec 2026"));
    }

    #[test]
    fn invalid_or_early_end_date_stays_buffered_and_is_rejected_on_save() {
        let calendar = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: String::new(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        let mut form = EventForm::new(
            0,
            std::slice::from_ref(&calendar),
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            9 * 60,
            60,
        );
        form.title = "Planning".into();
        form.recurrence =
            RecurrenceEditorState::Structured(RecurrenceEditorData::from_rule(RecurrenceRule {
                frequency: RecurrenceFrequency::Daily,
                interval: 1,
                days_of_week: vec![],
                occurrence_count: None,
                end_date: None,
            }));
        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::OnDate;
        form.selected = form
            .visible_fields()
            .iter()
            .position(|field| *field == FormField::RecurrenceEndDate)
            .unwrap();

        for character in "2026-02-30".chars() {
            form.push(character);
        }
        let RecurrenceEditorState::Structured(data) = &form.recurrence else {
            unreachable!();
        };
        assert_eq!(data.validated_end_date, None);
        assert_eq!(data.end_date_buffer, "2026-02-30");

        for _ in 0..10 {
            form.backspace();
        }
        for character in "2026-11-30".chars() {
            form.push(character);
        }
        let RecurrenceEditorState::Structured(data) = &form.recurrence else {
            unreachable!();
        };
        assert_eq!(data.validated_end_date, None);
        assert_eq!(data.end_date_buffer, "2026-11-30");
        assert!(form.to_draft(&[calendar]).is_err());
    }

    #[test]
    fn switching_end_modes_preserves_the_end_date_buffer() {
        let mut data = RecurrenceEditorData::from_rule(RecurrenceRule {
            frequency: RecurrenceFrequency::Daily,
            interval: 1,
            days_of_week: vec![],
            occurrence_count: None,
            end_date: None,
        });
        data.end_mode = RecurrenceEndMode::OnDate;
        data.end_date_buffer = "2026-12-31".into();
        data.refresh_end_date_validation(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap());

        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::AfterOccurrences);
        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::Never);
        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::OnDate);
        assert_eq!(data.end_date_buffer, "2026-12-31");
        assert_eq!(
            data.validated_end_date,
            Some(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap())
        );
    }

    #[test]
    fn occurrence_buffer_validates_without_mutating_the_recurrence_rule() {
        let calendar = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: String::new(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        let mut form = EventForm::new(0, &[calendar], Local::now().date_naive(), 9 * 60, 60);
        form.recurrence =
            RecurrenceEditorState::Structured(RecurrenceEditorData::from_rule(RecurrenceRule {
                frequency: RecurrenceFrequency::Daily,
                interval: 1,
                days_of_week: vec![],
                occurrence_count: None,
                end_date: None,
            }));
        let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
            unreachable!();
        };
        data.end_mode = RecurrenceEndMode::AfterOccurrences;
        form.selected = form
            .visible_fields()
            .iter()
            .position(|field| *field == FormField::RecurrenceOccurrences)
            .unwrap();

        form.push('1');
        let RecurrenceEditorState::Structured(data) = &form.recurrence else {
            unreachable!();
        };
        assert_eq!(data.validated_occurrence_count, Some(1));
        assert_eq!(data.rule.occurrence_count, None);

        form.push('0');
        let RecurrenceEditorState::Structured(data) = &form.recurrence else {
            unreachable!();
        };
        assert_eq!(data.occurrence_count_buffer, "10");
        assert_eq!(data.validated_occurrence_count, Some(10));
        assert_eq!(data.rule.occurrence_count, None);
        assert_eq!(form.value(&[]), "10");
        assert!(form.recurrence.summary().contains("For 10 occurrences"));
    }

    #[test]
    fn invalid_occurrence_counts_stay_buffered_and_are_rejected_on_save() {
        let calendar = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions::default(),
            title: "Work".into(),
            account: String::new(),
            provider: String::new(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        for value in ["", "0", "-1", "abc"] {
            let mut form = EventForm::new(
                0,
                std::slice::from_ref(&calendar),
                Local::now().date_naive(),
                9 * 60,
                60,
            );
            form.title = "Planning".into();
            form.recurrence = RecurrenceEditorState::Structured(RecurrenceEditorData::from_rule(
                RecurrenceRule {
                    frequency: RecurrenceFrequency::Daily,
                    interval: 1,
                    days_of_week: vec![],
                    occurrence_count: None,
                    end_date: None,
                },
            ));
            let RecurrenceEditorState::Structured(data) = &mut form.recurrence else {
                unreachable!();
            };
            data.end_mode = RecurrenceEndMode::AfterOccurrences;
            form.selected = form
                .visible_fields()
                .iter()
                .position(|field| *field == FormField::RecurrenceOccurrences)
                .unwrap();
            for character in value.chars() {
                form.push(character);
            }
            let RecurrenceEditorState::Structured(data) = &form.recurrence else {
                unreachable!();
            };
            assert_eq!(data.occurrence_count_buffer, value);
            assert_eq!(data.validated_occurrence_count, None);
            assert!(form.to_draft(std::slice::from_ref(&calendar)).is_err());
        }
    }

    #[test]
    fn switching_end_modes_preserves_the_occurrence_buffer() {
        let mut data = RecurrenceEditorData::from_rule(RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 2,
            days_of_week: vec!["MO".into()],
            occurrence_count: None,
            end_date: None,
        });
        data.end_mode = RecurrenceEndMode::AfterOccurrences;
        data.occurrence_count_buffer = "5".into();
        data.refresh_occurrence_count_validation();

        data.cycle_end_mode(-1);
        assert_eq!(data.end_mode, RecurrenceEndMode::OnDate);
        data.cycle_end_mode(1);
        assert_eq!(data.end_mode, RecurrenceEndMode::AfterOccurrences);
        assert_eq!(data.occurrence_count_buffer, "5");
        assert_eq!(data.validated_occurrence_count, Some(5));
    }

    #[test]
    fn recurrence_save_conversion_materializes_each_end_condition() {
        let start = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let base = RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 2,
            days_of_week: vec!["MO".into(), "WE".into()],
            occurrence_count: None,
            end_date: None,
        };

        let data = RecurrenceEditorData::from_rule(base.clone());
        assert_eq!(data.to_rule(start).unwrap(), base);

        let mut until = RecurrenceEditorData::from_rule(base.clone());
        until.end_mode = RecurrenceEndMode::OnDate;
        until.end_date_buffer = "2026-12-31".into();
        let rule = until.to_rule(start).unwrap();
        assert_eq!(rule.interval, 2);
        assert_eq!(rule.days_of_week, ["MO", "WE"]);
        assert_eq!(rule.occurrence_count, None);
        assert_eq!(
            rule.end_date.unwrap().date_naive(),
            NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()
        );

        let mut count = RecurrenceEditorData::from_rule(base);
        count.end_mode = RecurrenceEndMode::AfterOccurrences;
        count.occurrence_count_buffer = "10".into();
        let rule = count.to_rule(start).unwrap();
        assert_eq!(rule.occurrence_count, Some(10));
        assert_eq!(rule.end_date, None);
    }

    #[test]
    fn recurrence_save_conversion_rejects_invalid_buffers_and_existing_conflicts() {
        let start = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let base = RecurrenceRule {
            frequency: RecurrenceFrequency::Daily,
            interval: 1,
            days_of_week: vec![],
            occurrence_count: None,
            end_date: None,
        };
        let mut date = RecurrenceEditorData::from_rule(base.clone());
        date.end_mode = RecurrenceEndMode::OnDate;
        for buffer in ["", "2026-", "2026-02-30"] {
            date.end_date_buffer = buffer.into();
            assert_eq!(
                date.to_rule(start),
                Err(RecurrenceValidationError::InvalidEndDate)
            );
        }
        date.end_date_buffer = "2026-11-30".into();
        assert_eq!(
            date.to_rule(start),
            Err(RecurrenceValidationError::EndDateBeforeStart)
        );

        let mut count = RecurrenceEditorData::from_rule(base.clone());
        count.end_mode = RecurrenceEndMode::AfterOccurrences;
        for buffer in ["", "0", "abc"] {
            count.occurrence_count_buffer = buffer.into();
            assert_eq!(
                count.to_rule(start),
                Err(RecurrenceValidationError::InvalidOccurrenceCount)
            );
        }
        let mut invalid_existing = RecurrenceEditorData::from_rule(base);
        invalid_existing.end_mode = RecurrenceEndMode::InvalidExisting;
        assert_eq!(
            invalid_existing.to_rule(start),
            Err(RecurrenceValidationError::InvalidExistingEndCondition)
        );
    }

    #[test]
    fn recurrence_state_preserves_existing_and_unsupported_rules_on_save() {
        let start = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let existing = RecurrenceRule {
            frequency: RecurrenceFrequency::Weekly,
            interval: 2,
            days_of_week: vec!["MO".into(), "WE".into()],
            occurrence_count: Some(5),
            end_date: None,
        };
        let state = RecurrenceEditorState::from_rules(std::slice::from_ref(&existing));
        assert_eq!(state.to_rules(start).unwrap(), vec![existing.clone()]);

        let unsupported = vec![existing.clone(), existing];
        let state = RecurrenceEditorState::from_rules(&unsupported);
        assert_eq!(state.to_rules(start).unwrap(), unsupported);
    }

    #[test]
    fn parses_deterministic_search_filters_with_quotes() {
        let query = SearchQuery::parse(
            "/calendar:calendar-work /attendee:\"Ada Lovelace\" /recurring:true /all-day:false /from:2026-09-01 /to:2026-09-30 /location:Munich migration",
        );
        assert_eq!(query.calendar.as_deref(), Some("calendar-work"));
        assert_eq!(query.location.as_deref(), Some("munich"));
        assert_eq!(query.attendee.as_deref(), Some("ada lovelace"));
        assert_eq!(query.recurring, Some(true));
        assert_eq!(query.all_day, Some(false));
        assert_eq!(query.from.unwrap().to_string(), "2026-09-01");
        assert_eq!(query.to.unwrap().to_string(), "2026-09-30");
        assert_eq!(query.terms, ["migration"]);
    }

    #[tokio::test]
    async fn offline_search_filters_and_ranks_cached_events_deterministically() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let event = |id: &str,
                     calendar_id: &str,
                     title: &str,
                     start: &str,
                     notes: &str,
                     location: &str,
                     all_day: bool,
                     recurring: bool| {
            let mut value = serde_json::json!({
                "id": id, "calendarId": calendar_id, "title": title,
                "start": start, "end": "2026-09-10T11:00:00Z",
                "allDay": all_day, "notes": notes, "location": location,
                "hasRecurrence": recurring,
                "attendees": [{"name": "Ada Lovelace", "email": "ada@example.test"}],
            });
            if all_day {
                value["allDayStartDate"] = serde_json::json!("2026-09-10");
                value["allDayEndDateExclusive"] = serde_json::json!("2026-09-13");
            }
            serde_json::from_value::<Event>(value).unwrap()
        };
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![
                    event(
                        "contains",
                        "work",
                        "Planning notes",
                        "2026-09-10T12:00:00Z",
                        "Unicode CAFÉ",
                        "München",
                        false,
                        false,
                    ),
                    event(
                        "exact",
                        "work",
                        "Planning",
                        "2026-09-10T09:00:00Z",
                        "",
                        "Berlin",
                        false,
                        true,
                    ),
                    event(
                        "all-day",
                        "personal",
                        "Holiday",
                        "2026-09-09T00:00:00Z",
                        "",
                        "",
                        true,
                        true,
                    ),
                ],
                ..Snapshot::default()
            },
        );
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();

        app.search_query = "planning".into();
        assert_eq!(
            app.search_results()
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            ["exact", "contains"]
        );
        app.search_query = "café".into();
        assert_eq!(app.search_results()[0].id, "contains");
        app.search_query = "münchen".into();
        assert_eq!(app.search_results()[0].id, "contains");
        app.search_query = "/attendee:ada".into();
        assert_eq!(app.search_results().len(), 3);
        app.search_query = "/calendar:work /recurring:true".into();
        assert_eq!(app.search_results()[0].id, "exact");
        app.search_query = "/all-day:true /from:2026-09-12 /to:2026-09-12".into();
        assert_eq!(app.search_results()[0].id, "all-day");
        app.search_query = "/all-day:false /from:2026-09-10 /to:2026-09-10".into();
        assert_eq!(app.search_results().len(), 2);
        app.search_query.clear();
        assert!(app.search_results().is_empty());
        app.search_query = "missing".into();
        assert!(app.search_results().is_empty());
    }

    #[test]
    fn trusted_all_day_search_filters_use_date_coverage_not_legacy_start() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "id": "all-day-search",
            "calendarId": "personal",
            "title": "Trip",
            "start": "2026-09-09T22:00:00Z",
            "end": "2026-09-12T22:00:00Z",
            "allDay": true,
            "allDayStartDate": "2026-09-10",
            "allDayEndDateExclusive": "2026-09-13",
            "hasRecurrence": true,
            "recurrence": [{"frequency": "weekly", "interval": 1}],
        }))
        .unwrap();
        let date = |day| NaiveDate::from_ymd_opt(2026, 9, day).unwrap();
        assert!(event_matches_search_dates(
            &event,
            Some(date(10)),
            Some(date(12)),
            event.display_start_date(),
        ));
        assert!(event_matches_search_dates(
            &event,
            Some(date(12)),
            Some(date(12)),
            event.display_start_date(),
        ));
        assert!(!event_matches_search_dates(
            &event,
            Some(date(13)),
            Some(date(13)),
            event.display_start_date(),
        ));
        assert!(event.has_recurrence);
    }

    #[test]
    fn month_visible_range_covers_the_rendered_six_week_grid() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.view = View::Month;
        app.active_date = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let (start, end) = app.view_range();
        assert_eq!(
            start.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 8, 31).unwrap()
        );
        assert_eq!(
            end.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()
        );
    }

    #[test]
    fn ignores_stale_range_completion_state() {
        let mut app = App::new(Config::default(), Snapshot::default());
        let first = app.visible_range_request().id;
        let second = app.visible_range_request().id;
        app.apply_update(WorkerUpdate::RangeLoaded(first));
        assert_eq!(app.visible_range_state, VisibleRangeState::Loading);
        app.apply_update(WorkerUpdate::RangeFailed(second, "failed".into()));
        let VisibleRangeState::Failed(error) = &app.visible_range_state else {
            panic!("expected visible range failure");
        };
        assert_eq!(error.request_id, second);
        assert_eq!(error.error, "failed");
        assert_eq!(error.range.id, second);

        let retry = app.visible_range_request();
        assert!(retry.id > second);
        app.apply_update(WorkerUpdate::RangeLoaded(retry.id));
        assert_eq!(app.visible_range_state, VisibleRangeState::Ready);
    }

    #[test]
    fn backend_updates_do_not_reset_interaction_state() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.view = View::Month;
        app.active_date = NaiveDate::from_ymd_opt(2030, 10, 1).unwrap();
        app.selected_event = 3;
        app.timeline_start_minute = 600;
        app.sidebar_visible = true;
        app.search_query = "planning".into();

        app.apply_update(WorkerUpdate::BackendState(BackendState::Restarting));

        assert_eq!(app.backend_state, BackendState::Restarting);
        assert_eq!(app.view, View::Month);
        assert_eq!(
            app.active_date,
            NaiveDate::from_ymd_opt(2030, 10, 1).unwrap()
        );
        assert_eq!(app.selected_event, 3);
        assert_eq!(app.timeline_start_minute, 600);
        assert!(app.sidebar_visible);
        assert_eq!(app.search_query, "planning");
    }

    #[tokio::test]
    async fn snapshot_refresh_preserves_the_selected_event_id() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let events = backend
            .events(
                Utc::now() - Duration::days(7),
                Utc::now() + Duration::days(7),
                &[],
            )
            .await
            .unwrap();
        let snapshot = Snapshot {
            calendars,
            events: events.clone(),
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        };
        let mut app = App::new(Config::default(), snapshot);
        app.selected_event = 1;
        let selected_id = app.selected_event_ref().unwrap().id.clone();

        let mut refreshed = events;
        refreshed.reverse();
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: app.snapshot.calendars.clone(),
            events: refreshed,
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));

        assert_eq!(app.selected_event_ref().unwrap().id, selected_id);
    }

    #[tokio::test]
    async fn cross_view_switches_preserve_the_concrete_occurrence_and_date() {
        let (mut app, _) = app_with_mock_events().await;
        let day = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let template = app.snapshot.events[0].clone();
        let mut selected = timeline_event(&template, day, 9 * 60, 10 * 60, "selected");
        selected.id = "occurrence-selected".into();
        selected.provider_id = Some("shared-provider-shape".into());
        selected.series_id = Some("shared-series".into());
        let mut other =
            timeline_event(&template, day + Duration::days(2), 9 * 60, 10 * 60, "other");
        other.id = "occurrence-other".into();
        other.provider_id = Some("shared-provider-shape".into());
        other.series_id = Some("shared-series".into());
        app.snapshot.events = vec![selected.clone(), other];
        app.active_date = day;
        app.view = View::Day;
        app.selected_event = 0;

        for view in [View::Week, View::Day, View::Agenda, View::Month, View::Day] {
            assert!(matches!(
                app.execute_action(UserAction::ChangeView(view)),
                Some(WorkerCommand::EnsureRange(_))
            ));
            assert_eq!(
                app.active_date, day,
                "{view:?} must retain the occurrence date"
            );
            assert_eq!(
                app.selected_event_ref().map(|event| event.id.as_str()),
                Some("occurrence-selected"),
                "{view:?} must retain the concrete occurrence rather than its provider/series ID"
            );
        }

        app.timeline_start_minute = 11 * 60;
        app.timeline_viewport_owner = TimelineViewportOwner::Manual;
        let _ = app.execute_action(UserAction::ChangeView(View::Agenda));
        let _ = app.execute_action(UserAction::ChangeView(View::Week));
        assert_eq!(app.timeline_start_minute, 11 * 60);
        assert_eq!(app.timeline_viewport_owner, TimelineViewportOwner::Manual);
    }

    #[tokio::test]
    async fn month_hidden_selection_and_pointer_hit_restore_the_same_event_in_day() {
        let (mut app, _) = app_with_mock_events().await;
        let day = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let template = app.snapshot.events[0].clone();
        app.snapshot.events = (0..10)
            .map(|index| {
                let mut event = timeline_event(&template, day, 9 * 60, 10 * 60, "dense");
                event.id = format!("month-occurrence-{index}");
                event.title = format!("Dense {index}");
                event
            })
            .collect();
        app.active_date = day;
        app.view = View::Month;
        app.selected_event = 9;
        let hidden_id = app.selected_event_ref().unwrap().id.clone();
        let Some(CalendarHitGeometry::Month(month_geometry)) =
            crate::ui::calendar_hit_geometry(&app, ratatui::layout::Rect::new(0, 0, 120, 40))
        else {
            panic!("month geometry must be available");
        };
        assert!(
            !month_geometry
                .event_regions
                .iter()
                .any(|region| region.event_id == hidden_id),
            "the selected occurrence is deliberately behind the overflow row"
        );
        let _ = app.execute_action(UserAction::ChangeView(View::Day));
        assert_eq!(app.active_date, day);
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some(hidden_id.as_str())
        );

        let Some(CalendarHitGeometry::Day(day_geometry)) =
            crate::ui::calendar_hit_geometry(&app, ratatui::layout::Rect::new(0, 0, 160, 60))
        else {
            panic!("day geometry must be available");
        };
        let hit = day_geometry
            .event_regions
            .iter()
            .find(|region| region.event_id == "month-occurrence-0")
            .unwrap();
        let _ = app.handle_pointer_with_hit_test(
            PointerEvent {
                position: Some(crate::input::PointerPosition {
                    x: hit.rect.x,
                    y: hit.rect.y,
                }),
                button: Some(crate::input::PointerButton::Primary),
                action: PointerAction::Press,
            },
            Some(&CalendarHitGeometry::Day(day_geometry)),
        );
        let _ = app.execute_action(UserAction::ChangeView(View::Week));
        assert_eq!(
            app.selected_event_ref().map(|event| event.id.as_str()),
            Some("month-occurrence-0")
        );
    }

    #[tokio::test]
    async fn refresh_removal_clears_selection_instead_of_reusing_its_index() {
        let (mut app, _) = app_with_mock_events().await;
        app.view = View::Day;
        let date = app.snapshot.events[0].display_start_date();
        app.active_date = date;
        let mut selected = app.snapshot.events[0].clone();
        selected.id = "selected-to-remove".into();
        let mut neighbour = selected.clone();
        neighbour.id = "unrelated-neighbour".into();
        neighbour.start += Duration::minutes(15);
        neighbour.end += Duration::minutes(15);
        app.snapshot.events = vec![selected, neighbour.clone()];
        app.selected_event = 0;
        app.apply_update(WorkerUpdate::BackendState(BackendState::Restarting));
        app.apply_update(WorkerUpdate::BackendState(BackendState::Connected));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: app.snapshot.calendars.clone(),
            events: vec![neighbour],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert!(app.selected_event_ref().is_none());
        assert_ne!(app.selected_event, 0);
    }

    #[tokio::test]
    async fn calendar_manager_preserves_calendar_selection_by_stable_id() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars: calendars.clone(),
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.selected_calendar = calendars
            .iter()
            .position(|calendar| calendar.id == "shared")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManager);

        let mut refreshed = calendars;
        refreshed.reverse();
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: refreshed,
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.snapshot.calendars[app.selected_calendar].id, "shared");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManagerDetails);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManager);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[tokio::test]
    async fn calendar_manager_clears_removed_calendar_selection_safely() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars: calendars.clone(),
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.selected_calendar = calendars
            .iter()
            .position(|calendar| calendar.id == "calendar-delete-test")
            .unwrap();
        app.mode = Mode::CalendarManager;
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: calendars
                .into_iter()
                .filter(|calendar| calendar.id != "calendar-delete-test")
                .collect(),
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert!(app.selected_calendar < app.snapshot.calendars.len());
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Selected calendar was removed")
        );
    }

    #[test]
    fn calendar_manager_receives_backend_capabilities_without_actions() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.apply_update(WorkerUpdate::CalendarCapabilities(CalendarCapabilities {
            can_list_sources: true,
            can_create: true,
            can_update: true,
            can_change_color: true,
            can_delete: true,
        }));
        assert!(app.calendar_capabilities.can_create);
        assert!(app.calendar_capabilities.can_update);
        assert!(app.calendar_capabilities.can_change_color);
        assert!(app.calendar_capabilities.can_delete);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
                .is_none()
        );
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.mode, Mode::CalendarManager);
    }

    #[test]
    fn calendar_create_form_is_capability_gated_and_preserves_failures() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.mode = Mode::CalendarManager;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.calendar_form.is_none());

        app.calendar_capabilities.can_create = true;
        app.calendar_sources = vec![CalendarSource {
            id: "source-test-1".into(),
            title: "Test Source".into(),
            source_type: "local".into(),
            is_writable: true,
        }];
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarCreate);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Calendar title is required")
        );

        let form = app.calendar_form.as_mut().unwrap();
        form.title = "New Calendar".into();
        form.source_index = Some(0);
        let command = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            command,
            Some(WorkerCommand::CreateCalendar(CreateCalendarRequest { source_id, .. }))
                if source_id == "source-test-1"
        ));
        app.apply_update(WorkerUpdate::CalendarCreateFailed(
            CalendarError::PermissionDenied,
        ));
        assert_eq!(app.mode, Mode::CalendarCreate);
        assert_eq!(app.calendar_form.as_ref().unwrap().title, "New Calendar");
        assert_eq!(app.mutation_state, MutationState::Failed);
    }

    #[test]
    fn calendar_create_selects_the_created_calendar_after_metadata_refresh() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.mode = Mode::CalendarCreate;
        app.calendar_form = Some(CalendarForm::new());
        let created = CalendarInfo {
            id: "calendar-created".into(),
            source_id: "source-test-1".into(),
            permissions: CalendarPermissions::default(),
            title: "Created".into(),
            account: "Test".into(),
            provider: "Local".into(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        app.apply_update(WorkerUpdate::CalendarCreateSucceeded(created.clone()));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: vec![created],
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(
            app.snapshot.calendars[app.selected_calendar].id,
            "calendar-created"
        );
    }

    #[tokio::test]
    async fn calendar_rename_form_is_capability_and_permission_gated() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.mode = Mode::CalendarManager;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.calendar_rename_form.is_none());

        app.calendar_capabilities.can_update = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarRename);
        assert_eq!(app.calendar_rename_form.as_ref().unwrap().title, "Work");
        app.calendar_rename_form.as_mut().unwrap().title = "   ".into();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Calendar title is required")
        );

        app.calendar_rename_form.as_mut().unwrap().title = "Work Renamed".into();
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(WorkerCommand::RenameCalendar(RenameCalendarRequest { calendar_id, title }))
                if calendar_id == "work" && title == "Work Renamed"
        ));
        app.apply_update(WorkerUpdate::CalendarRenameFailed(
            CalendarError::CannotModifyMetadata,
        ));
        assert_eq!(app.mode, Mode::CalendarRename);
        assert_eq!(
            app.calendar_rename_form.as_ref().unwrap().title,
            "Work Renamed"
        );

        app.mode = Mode::CalendarManager;
        app.calendar_rename_form = None;
        app.selected_calendar = app
            .snapshot
            .calendars
            .iter()
            .position(|calendar| calendar.id == "shared")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.calendar_rename_form.is_none());
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("This calendar's metadata cannot be modified")
        );
    }

    #[test]
    fn calendar_rename_selects_same_id_after_metadata_refresh() {
        let original = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions {
                can_modify_metadata: true,
                ..CalendarPermissions::default()
            },
            title: "Work".into(),
            account: "Mock".into(),
            provider: "Mock".into(),
            color: "#336699".into(),
            is_writable: true,
            enabled: true,
        };
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars: vec![original.clone()],
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.mode = Mode::CalendarRename;
        app.calendar_rename_form = Some(CalendarRenameForm::new(&original));
        let mut renamed = original;
        renamed.title = "Work Renamed".into();
        app.apply_update(WorkerUpdate::CalendarRenameSucceeded(renamed.clone()));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: vec![renamed],
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(app.snapshot.calendars[app.selected_calendar].id, "work");
        assert_eq!(
            app.snapshot.calendars[app.selected_calendar].title,
            "Work Renamed"
        );
    }

    #[tokio::test]
    async fn calendar_color_form_validates_and_preserves_typed_failures() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.mode = Mode::CalendarManager;
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE));
        assert!(app.calendar_color_form.is_none());

        app.calendar_capabilities.can_change_color = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarColor);
        assert_eq!(app.calendar_color_form.as_ref().unwrap().color, "#4C8DFF");
        app.calendar_color_form.as_mut().unwrap().color = "red".into();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Calendar color must use #RRGGBB")
        );

        app.calendar_color_form.as_mut().unwrap().color = "#336699".into();
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(WorkerCommand::SetCalendarColor(SetCalendarColorRequest { calendar_id, color }))
                if calendar_id == "work" && color == "#336699"
        ));
        app.apply_update(WorkerUpdate::CalendarColorFailed(
            CalendarError::PermissionDenied,
        ));
        assert_eq!(app.mode, Mode::CalendarColor);
        assert_eq!(app.calendar_color_form.as_ref().unwrap().color, "#336699");

        app.mode = Mode::CalendarManager;
        app.calendar_color_form = None;
        app.selected_calendar = app
            .snapshot
            .calendars
            .iter()
            .position(|calendar| calendar.id == "shared")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE));
        assert!(app.calendar_color_form.is_none());
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Calendar metadata is read-only")
        );
    }

    #[test]
    fn calendar_color_keeps_selection_after_metadata_refresh() {
        let calendar = CalendarInfo {
            id: "work".into(),
            source_id: "mock-work".into(),
            permissions: CalendarPermissions {
                can_modify_metadata: true,
                ..CalendarPermissions::default()
            },
            title: "Work".into(),
            account: "Mock".into(),
            provider: "Mock".into(),
            color: "#4C8DFF".into(),
            is_writable: true,
            enabled: true,
        };
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars: vec![calendar.clone()],
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        let mut updated = calendar;
        updated.color = "#FF5500".into();
        app.apply_update(WorkerUpdate::CalendarColorSucceeded(updated.clone()));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: vec![updated],
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(app.snapshot.calendars[app.selected_calendar].id, "work");
        assert_eq!(
            app.snapshot.calendars[app.selected_calendar].color,
            "#FF5500"
        );
    }

    #[tokio::test]
    async fn calendar_delete_is_capability_permission_gated_and_requires_y_confirmation() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.mode = Mode::CalendarManager;
        app.selected_calendar = app
            .snapshot
            .calendars
            .iter()
            .position(|calendar| calendar.id == "calendar-delete-test")
            .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Calendar deletion is not supported")
        );

        app.calendar_capabilities.can_delete = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarDeleteConfirm);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.mode, Mode::CalendarDeleteConfirm);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManager);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManager);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let command = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            command,
            Some(WorkerCommand::DeleteCalendar(DeleteCalendarRequest { calendar_id }))
                if calendar_id == "calendar-delete-test"
        ));
        assert_eq!(app.mutation_state, MutationState::Deleting);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .is_none()
        );
    }

    #[tokio::test]
    async fn calendar_delete_removes_metadata_and_uses_next_then_previous_selection() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let deleted_index = calendars
            .iter()
            .position(|calendar| calendar.id == "calendar-delete-test")
            .unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars: calendars.clone(),
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.mode = Mode::CalendarDeleteConfirm;
        app.selected_calendar = deleted_index;
        app.calendar_delete_confirmation = Some(CalendarDeleteConfirmation {
            calendar_id: "calendar-delete-test".into(),
            title: "Delete Me".into(),
        });
        app.apply_update(WorkerUpdate::CalendarDeleteSucceeded(
            DeleteCalendarResponse {
                calendar_id: "calendar-delete-test".into(),
            },
        ));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: calendars
                .into_iter()
                .filter(|calendar| calendar.id != "calendar-delete-test")
                .collect(),
            events: vec![],
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert!(app.calendar_delete_confirmation.is_none());
        assert!(
            app.snapshot
                .calendars
                .iter()
                .all(|calendar| calendar.id != "calendar-delete-test")
        );
        let expected_index = deleted_index.min(app.snapshot.calendars.len() - 1);
        assert_eq!(app.selected_calendar, expected_index);
    }

    #[tokio::test]
    async fn calendar_delete_prevents_read_only_calendars_and_keeps_manager_usable_after_failure() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let mut app = App::new(
            Config::default(),
            Snapshot {
                calendars,
                events: vec![],
                authorization: AuthorizationStatus::FullAccess,
                updated_at: Some(Utc::now()),
            },
        );
        app.mode = Mode::CalendarManager;
        app.calendar_capabilities.can_delete = true;
        app.selected_calendar = app
            .snapshot
            .calendars
            .iter()
            .position(|calendar| calendar.id == "shared")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("This calendar cannot be deleted")
        );

        app.calendar_delete_confirmation = Some(CalendarDeleteConfirmation {
            calendar_id: "missing-calendar".into(),
            title: "Missing".into(),
        });
        app.mode = Mode::CalendarDeleteConfirm;
        app.apply_update(WorkerUpdate::CalendarDeleteFailed(CalendarError::NotFound));
        assert_eq!(app.mode, Mode::CalendarManager);
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Calendar no longer exists")
        );
    }

    #[test]
    fn retry_shortcut_uses_a_new_request_and_clears_the_old_failure() {
        let mut app = App::new(Config::default(), Snapshot::default());
        let failed = app.visible_range_request();
        app.apply_update(WorkerUpdate::RangeFailed(failed.id, "offline".into()));

        let command = app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        let Some(WorkerCommand::RefreshRange(retry)) = command else {
            panic!("R should refresh the visible range");
        };
        assert!(retry.id > failed.id);
        assert_eq!(app.visible_range_state, VisibleRangeState::Loading);
        app.apply_update(WorkerUpdate::RangeFailed(failed.id, "stale".into()));
        assert_eq!(app.visible_range_state, VisibleRangeState::Loading);
        app.apply_update(WorkerUpdate::RangeLoaded(retry.id));
        assert_eq!(app.visible_range_state, VisibleRangeState::Ready);
    }

    #[tokio::test]
    async fn unified_editor_tracks_create_edit_and_duplicate_origins() {
        let backend = crate::backend::MockBackend::seeded();
        let calendars = backend.calendars().await.unwrap();
        let event = backend
            .events(
                Utc::now() - Duration::days(1),
                Utc::now() + Duration::days(1),
                &[],
            )
            .await
            .unwrap()
            .remove(0);

        let create = EventForm::new(0, &calendars, Local::now().date_naive(), 9 * 60, 60);
        assert_eq!(create.editor_mode, EditorMode::Create);
        let edit = EventForm::from_event(&event, &calendars);
        assert_eq!(
            edit.editor_mode,
            EditorMode::Edit {
                event_id: event.id.clone()
            }
        );
        let duplicate = EventForm::duplicate_from(&event, &calendars);
        assert_eq!(
            duplicate.editor_mode,
            EditorMode::Duplicate {
                source_id: event.id.clone()
            }
        );
        assert_eq!(duplicate.id, None);
        assert_eq!(duplicate.recurrence, RecurrenceEditorState::None);
    }

    #[test]
    fn dirty_editor_requires_an_explicit_discard_choice() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.form = Some(EventForm::new(
            0,
            &[CalendarInfo {
                id: "work".into(),
                source_id: "mock-work".into(),
                permissions: CalendarPermissions::default(),
                title: "Work".into(),
                account: String::new(),
                provider: String::new(),
                color: "#fff".into(),
                is_writable: true,
                enabled: true,
            }],
            Local::now().date_naive(),
            9 * 60,
            60,
        ));
        app.mode = Mode::Form;
        app.form_dirty = true;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::DiscardConfirm);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Form);
        assert!(app.form.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::DiscardConfirm);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.form.is_none());
    }

    #[test]
    fn mutation_failure_keeps_the_editor_and_its_contents() {
        let mut app = App::new(Config::default(), Snapshot::default());
        let mut form = EventForm::new(
            0,
            &[CalendarInfo {
                id: "work".into(),
                source_id: "mock-work".into(),
                permissions: CalendarPermissions::default(),
                title: "Work".into(),
                account: String::new(),
                provider: String::new(),
                color: "#fff".into(),
                is_writable: true,
                enabled: true,
            }],
            Local::now().date_naive(),
            9 * 60,
            60,
        );
        form.title = "Keep this title".into();
        form.all_day = true;
        form.start = "2026-09-10".into();
        form.end = "2026-09-12".into();
        app.form = Some(form);
        app.mode = Mode::Form;
        app.apply_update(WorkerUpdate::MutationFailed("Unable to save event".into()));
        assert_eq!(app.mutation_state, MutationState::Failed);
        assert_eq!(app.mode, Mode::Form);
        assert_eq!(app.form.as_ref().unwrap().title, "Keep this title");
        assert_eq!(app.form.as_ref().unwrap().start, "2026-09-10");
        assert_eq!(app.form.as_ref().unwrap().end, "2026-09-12");
    }

    #[tokio::test]
    async fn recurring_edit_scope_requires_a_choice_and_preserves_it_for_retry() {
        let (mut app, backend) = app_with_mock_events().await;
        let stable_id = "mock-standup";
        assert_eq!(app.selected_event_ref().unwrap().id, stable_id);

        app.handle_key(key('e'));
        assert_eq!(app.mode, Mode::RecurringEditScope);
        assert!(app.form.is_none());
        assert_eq!(
            app.pending_recurring_mutation,
            Some((stable_id.into(), RecurrenceMutationAction::Edit))
        );
        // Scope modals own their input: normal commands cannot leak through.
        app.handle_key(key('j'));
        app.handle_key(key('d'));
        assert_eq!(app.mode, Mode::RecurringEditScope);
        assert_eq!(backend.last_update_span(), None);

        app.handle_key(key('1'));
        assert_eq!(app.mode, Mode::Form);
        assert_eq!(app.form.as_ref().unwrap().span, EventSpan::ThisEvent);
        assert!(app.pending_recurring_mutation.is_none());
        let form = app.form.as_mut().unwrap();
        form.title.push_str(" changed");
        // The mock fixture's provider-facing weekday token is deliberately
        // opaque; this test exercises scope transport, not recurrence parsing.
        form.recurrence = RecurrenceEditorState::none();
        let command = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let Some(WorkerCommand::Update(
            draft,
            EventSpan::ThisEvent,
            Some(RecurrenceMutationScope::ThisEvent),
            AlarmMutation::Preserve,
            EventTimeMutation::ReplaceLegacy,
        )) = command
        else {
            panic!("recurring edit should retain the selected scope");
        };
        backend
            .update_event(
                draft.clone(),
                EventSpan::ThisEvent,
                AlarmMutation::Preserve,
                EventTimeMutation::ReplaceLegacy,
            )
            .await
            .unwrap();
        assert_eq!(backend.last_update_span(), Some(EventSpan::ThisEvent));

        app.apply_update(WorkerUpdate::MutationFailed("opaque failure".into()));
        assert_eq!(app.mode, Mode::Form);
        assert_eq!(app.form.as_ref().unwrap().span, EventSpan::ThisEvent);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(WorkerCommand::Update(
                _,
                EventSpan::ThisEvent,
                Some(RecurrenceMutationScope::ThisEvent),
                AlarmMutation::Preserve,
                _
            ))
        ));

        let (mut future_app, future_backend) = app_with_mock_events().await;
        future_app.handle_key(key('e'));
        future_app.handle_key(key('2'));
        future_app.form.as_mut().unwrap().recurrence = RecurrenceEditorState::none();
        let command =
            future_app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let Some(WorkerCommand::Update(
            draft,
            EventSpan::FutureEvents,
            Some(RecurrenceMutationScope::FutureEvents),
            AlarmMutation::Preserve,
            EventTimeMutation::ReplaceLegacy,
        )) = command
        else {
            panic!("future recurring edit should retain the selected scope");
        };
        future_backend
            .update_event(
                draft,
                EventSpan::FutureEvents,
                AlarmMutation::Preserve,
                EventTimeMutation::ReplaceLegacy,
            )
            .await
            .unwrap();
        assert_eq!(
            future_backend.last_update_span(),
            Some(EventSpan::FutureEvents)
        );
    }

    #[tokio::test]
    async fn recurring_scope_cancellation_and_delete_keep_a_stable_target() {
        let (mut app, backend) = app_with_mock_events().await;
        app.handle_key(key('e'));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.form.is_none());
        assert!(app.pending_recurring_mutation.is_none());

        app.handle_key(key('d'));
        assert_eq!(app.mode, Mode::RecurringDeleteScope);
        app.handle_key(key('2'));
        assert_eq!(app.mode, Mode::Delete);
        assert_eq!(app.delete_span, EventSpan::FutureEvents);
        assert_eq!(app.pending_delete_event_id.as_deref(), Some("mock-standup"));
        // Confirmation cancellation clears the stable target and scope.
        app.handle_key(key('n'));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_delete_event_id.is_none());
        assert_eq!(backend.last_delete_span(), None);

        app.handle_key(key('d'));
        app.handle_key(key('1'));
        // A selection change while a modal is open must not retarget deletion.
        app.selected_event = 1;
        let command = app.handle_key(key('y'));
        let Some(WorkerCommand::Delete(
            id,
            EventSpan::ThisEvent,
            Some(RecurrenceMutationScope::ThisEvent),
        )) = command
        else {
            panic!("confirmation should retain the recurring event ID and scope");
        };
        assert_eq!(id, "mock-standup");
        let event = backend
            .events(
                Utc::now() - Duration::days(7),
                Utc::now() + Duration::days(7),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.id == id)
            .unwrap();
        backend
            .delete_event(
                crate::model::EventMutationTarget {
                    provider_id: event.provider_id.unwrap(),
                    calendar_id: event.calendar_id,
                    occurrence_start: event.start,
                },
                EventSpan::ThisEvent,
            )
            .await
            .unwrap();
        assert_eq!(backend.last_delete_span(), Some(EventSpan::ThisEvent));
    }

    #[tokio::test]
    async fn recurring_scope_handles_disappearing_and_read_only_events_safely() {
        let (mut app, _backend) = app_with_mock_events().await;
        app.handle_key(key('e'));
        app.apply_update(WorkerUpdate::Snapshot(Snapshot {
            calendars: app.snapshot.calendars.clone(),
            events: app
                .snapshot
                .events
                .iter()
                .filter(|event| event.id != "mock-standup")
                .cloned()
                .collect(),
            authorization: AuthorizationStatus::FullAccess,
            updated_at: Some(Utc::now()),
        }));
        assert!(app.handle_key(key('1')).is_none());
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_recurring_mutation.is_none());
        assert_eq!(
            app.status.as_ref().map(|status| status.0.as_str()),
            Some("Event no longer exists")
        );

        let (mut app, backend) = app_with_mock_events().await;
        let mut readonly = app.snapshot.events[0].clone();
        readonly.calendar_id = "holidays".into();
        app.snapshot.events = vec![readonly];
        app.selected_event = 0;
        app.handle_key(key('e'));
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(key('d'));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_recurring_mutation.is_none());
        assert_eq!(backend.last_update_span(), None);
        assert_eq!(backend.last_delete_span(), None);
    }

    #[tokio::test]
    async fn non_recurring_edits_bypass_scope_and_recurring_future_scope_does_not_leak() {
        let (mut app, _) = app_with_mock_events().await;
        app.handle_key(key('e'));
        app.handle_key(key('2'));
        assert_eq!(app.form.as_ref().unwrap().span, EventSpan::FutureEvents);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);

        app.selected_event = app
            .visible_events()
            .iter()
            .position(|event| !event.has_recurrence)
            .unwrap();
        app.handle_key(key('e'));
        assert_eq!(app.mode, Mode::Form);
        assert_eq!(app.form.as_ref().unwrap().span, EventSpan::ThisEvent);
        assert!(app.pending_recurring_mutation.is_none());
    }

    #[tokio::test]
    async fn details_anchor_survives_row_changes_and_close_restores_the_occurrence() {
        let (mut app, _) = app_with_mock_events().await;
        let original_id = app.selected_event_ref().unwrap().id.clone();
        app.view = View::Day;
        app.timeline_start_minute = 11 * 60;
        app.timeline_viewport_owner = TimelineViewportOwner::Manual;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);

        // A refresh can reorder the stored selection index while Details is
        // open. Its action must still resolve the anchored occurrence.
        app.selected_event = 1;
        assert_eq!(app.details_event_ref().unwrap().id, original_id);
        app.handle_key(key('e'));
        assert_eq!(
            app.pending_recurring_mutation
                .as_ref()
                .map(|(id, _)| id.as_str()),
            Some(original_id.as_str())
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.selected_event_ref().unwrap().id, original_id);
        assert_eq!(app.timeline_start_minute, 11 * 60);
        assert_eq!(app.timeline_viewport_owner, TimelineViewportOwner::Manual);
    }

    #[tokio::test]
    async fn duplicate_cancel_returns_to_details_with_the_original_occurrence() {
        let (mut app, _) = app_with_mock_events().await;
        app.selected_event = app
            .visible_events()
            .iter()
            .position(|event| !event.has_recurrence)
            .unwrap();
        let original_id = app.selected_event_ref().unwrap().id.clone();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Form);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Details);
        assert_eq!(app.details_event_ref().unwrap().id, original_id);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.selected_event_ref().unwrap().id, original_id);
    }

    #[tokio::test]
    async fn stale_mutation_completion_cannot_close_a_newer_editor_session() {
        let (mut app, _) = app_with_mock_events().await;
        let first = app.snapshot.events[0].clone();
        let second = app.snapshot.events[1].clone();
        app.begin_edit_event(first);
        let first_command = app.begin_mutation_session(WorkerCommand::Create(EventDraft::new(
            "work".into(),
            app.active_date,
        )));
        let WorkerCommand::CreateWithSession {
            session: first_session,
            ..
        } = first_command
        else {
            panic!("event mutation must receive a session");
        };

        app.begin_edit_event(second.clone());
        let second_command = app.begin_mutation_session(WorkerCommand::Create(EventDraft::new(
            second.calendar_id.clone(),
            app.active_date,
        )));
        let WorkerCommand::CreateWithSession {
            session: second_session,
            ..
        } = second_command
        else {
            panic!("event mutation must receive a session");
        };
        assert_ne!(first_session, second_session);

        app.apply_update(WorkerUpdate::MutationSucceededFor(
            first_session,
            MutationEffect::Created {
                event_id: "stale-created".into(),
                interval: (second.start, second.end),
            },
        ));
        assert_eq!(app.mode, Mode::Form);
        assert_eq!(app.form.as_ref().unwrap().title, second.title);
        assert_eq!(app.active_mutation_session, Some(second_session));
    }

    #[test]
    fn help_is_modal_scrollable_and_does_not_leak_normal_mode_actions() {
        let mut app = App::new(Config::default(), Snapshot::default());
        app.handle_key(key('?'));
        assert_eq!(app.mode, Mode::Help);
        app.handle_key(key('j'));
        assert_eq!(app.help_scroll, 1);
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.help_scroll >= 9);
        // `n` is a normal-mode command, not a Help action.
        app.handle_key(key('n'));
        assert_eq!(app.mode, Mode::Help);
        assert!(app.form.is_none());
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.help_scroll, u16::MAX);
        app.handle_key(key('?'));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn palette_key_hints_match_effective_keyboard_actions() {
        assert_eq!(PaletteCommand::NewEvent.key_hint(), "n");
        assert_eq!(PaletteCommand::Search.key_hint(), "/");
        assert_eq!(PaletteCommand::Day.key_hint(), "gd");
        assert_eq!(PaletteCommand::RetryVisibleRange.key_hint(), "R");
    }
}
