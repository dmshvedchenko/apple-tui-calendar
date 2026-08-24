mod eventkit;
mod mock;

pub use eventkit::{EventKitBackend, IPC_PROTOCOL_VERSION, resolve_service};
pub use mock::MockBackend;

use crate::model::{
    AlarmMutation, AuthorizationStatus, CalendarCapabilities, CalendarError, CalendarInfo,
    CalendarSource, CreateCalendarRequest, CreateCalendarResponse, DeleteCalendarRequest,
    DeleteCalendarResponse, Event, EventDraft, EventSpan, EventTimeMutation, FetchRequest,
    InvitationResponse, RenameCalendarRequest, RenameCalendarResponse, SetCalendarColorRequest,
    SetCalendarColorResponse,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    Starting,
    Connected,
    Disconnected,
    Restarting,
    PermissionDenied,
    ProtocolMismatch,
    Failed,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("calendar operation rejected: {0:?}")]
    Calendar(CalendarError),
    #[error("calendar permission was denied")]
    PermissionDenied,
    #[error("calendar item was not found: {0}")]
    NotFound(String),
    #[error("invalid calendar data: {0}")]
    Invalid(String),
    #[error("operation is not supported by EventKit: {0}")]
    Unsupported(String),
    #[error("calendar service is unavailable: {0}")]
    Unavailable(String),
    #[error("calendar service helper exited")]
    HelperExited,
    #[error("calendar service timed out: {0}")]
    Timeout(String),
    #[error("calendar IPC protocol error: {0}")]
    Protocol(ProtocolError),
    #[error("calendar service error: {0}")]
    Service(String),
}

/// Typed transport faults. These are deliberately separate from calendar
/// domain failures and are safe to show without exposing helper output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProtocolError {
    #[error("malformed JSON response")]
    MalformedJson,
    #[error("unknown message schema")]
    UnknownMessageSchema,
    #[error("version mismatch (expected v{expected}, received {received:?})")]
    VersionMismatch {
        expected: u64,
        received: Option<u64>,
    },
}

#[async_trait]
pub trait CalendarBackend: Send + Sync {
    async fn authorization_status(&self) -> Result<AuthorizationStatus, BackendError>;
    async fn request_access(&self) -> Result<AuthorizationStatus, BackendError>;
    async fn calendars(&self) -> Result<Vec<CalendarInfo>, BackendError>;
    async fn calendar_capabilities(&self) -> Result<CalendarCapabilities, BackendError>;
    async fn calendar_sources(&self) -> Result<Vec<CalendarSource>, BackendError>;
    async fn create_calendar(
        &self,
        request: CreateCalendarRequest,
    ) -> Result<CreateCalendarResponse, BackendError>;
    async fn rename_calendar(
        &self,
        request: RenameCalendarRequest,
    ) -> Result<RenameCalendarResponse, BackendError>;
    async fn set_calendar_color(
        &self,
        request: SetCalendarColorRequest,
    ) -> Result<SetCalendarColorResponse, BackendError>;
    async fn delete_calendar(
        &self,
        request: DeleteCalendarRequest,
    ) -> Result<DeleteCalendarResponse, BackendError>;
    async fn events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        calendar_ids: &[String],
    ) -> Result<Vec<Event>, BackendError>;
    /// Additive date-aware fetch entry point. Backends that have not adopted
    /// calendar-date predicates retain their established instant behavior.
    async fn fetch_events(
        &self,
        request: FetchRequest,
        calendar_ids: &[String],
    ) -> Result<Vec<Event>, BackendError> {
        self.events(
            request.instant_range.start,
            request.instant_range.end,
            calendar_ids,
        )
        .await
    }
    async fn create_event(&self, event: EventDraft) -> Result<Event, BackendError>;
    async fn update_event(
        &self,
        event: EventDraft,
        span: EventSpan,
        alarms: AlarmMutation,
        time_mutation: EventTimeMutation,
    ) -> Result<Event, BackendError>;
    async fn delete_event(&self, id: &str, span: EventSpan) -> Result<(), BackendError>;
    async fn respond_to_invitation(
        &self,
        id: &str,
        response: InvitationResponse,
    ) -> Result<(), BackendError>;
    fn subscribe_changes(&self) -> broadcast::Receiver<()>;
    fn subscribe_backend_state(&self) -> broadcast::Receiver<BackendState>;
}

pub struct OfflineBackend {
    reason: String,
    changes: broadcast::Sender<()>,
    states: broadcast::Sender<BackendState>,
}

impl OfflineBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        let (changes, _) = broadcast::channel(1);
        let (states, _) = broadcast::channel(1);
        let _ = states.send(BackendState::Disconnected);
        Self {
            reason: reason.into(),
            changes,
            states,
        }
    }

    fn error(&self) -> BackendError {
        BackendError::Unavailable(self.reason.clone())
    }
}

#[async_trait]
impl CalendarBackend for OfflineBackend {
    async fn authorization_status(&self) -> Result<AuthorizationStatus, BackendError> {
        Err(self.error())
    }
    async fn request_access(&self) -> Result<AuthorizationStatus, BackendError> {
        Err(self.error())
    }
    async fn calendars(&self) -> Result<Vec<CalendarInfo>, BackendError> {
        Err(self.error())
    }
    async fn calendar_capabilities(&self) -> Result<CalendarCapabilities, BackendError> {
        Err(self.error())
    }
    async fn calendar_sources(&self) -> Result<Vec<CalendarSource>, BackendError> {
        Err(self.error())
    }
    async fn create_calendar(
        &self,
        _: CreateCalendarRequest,
    ) -> Result<CreateCalendarResponse, BackendError> {
        Err(self.error())
    }
    async fn rename_calendar(
        &self,
        _: RenameCalendarRequest,
    ) -> Result<RenameCalendarResponse, BackendError> {
        Err(self.error())
    }
    async fn set_calendar_color(
        &self,
        _: SetCalendarColorRequest,
    ) -> Result<SetCalendarColorResponse, BackendError> {
        Err(self.error())
    }
    async fn delete_calendar(
        &self,
        _: DeleteCalendarRequest,
    ) -> Result<DeleteCalendarResponse, BackendError> {
        Err(self.error())
    }
    async fn events(
        &self,
        _: DateTime<Utc>,
        _: DateTime<Utc>,
        _: &[String],
    ) -> Result<Vec<Event>, BackendError> {
        Err(self.error())
    }
    async fn create_event(&self, _: EventDraft) -> Result<Event, BackendError> {
        Err(self.error())
    }
    async fn update_event(
        &self,
        _: EventDraft,
        _: EventSpan,
        _: AlarmMutation,
        _: EventTimeMutation,
    ) -> Result<Event, BackendError> {
        Err(self.error())
    }
    async fn delete_event(&self, _: &str, _: EventSpan) -> Result<(), BackendError> {
        Err(self.error())
    }
    async fn respond_to_invitation(
        &self,
        _: &str,
        _: InvitationResponse,
    ) -> Result<(), BackendError> {
        Err(self.error())
    }
    fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }
    fn subscribe_backend_state(&self) -> broadcast::Receiver<BackendState> {
        self.states.subscribe()
    }
}
