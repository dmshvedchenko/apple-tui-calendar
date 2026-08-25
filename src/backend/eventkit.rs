use super::{BackendError, BackendState, CalendarBackend, ProtocolError};
use crate::model::{
    AlarmMutation, AuthorizationStatus, CalendarCapabilities, CalendarInfo, CalendarSource,
    CreateCalendarRequest, CreateCalendarResponse, DeleteCalendarRequest, DeleteCalendarResponse,
    Event, EventDraft, EventSpan, EventTimeMutation, InvitationResponse, RenameCalendarRequest,
    RenameCalendarResponse, SetCalendarColorRequest, SetCalendarColorResponse,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, broadcast, mpsc, oneshot},
};

type Pending = HashMap<u64, oneshot::Sender<Result<Value, BackendError>>>;
pub const IPC_PROTOCOL_VERSION: u64 = 2;

enum IncomingMessage {
    StoreChanged,
    Response {
        id: u64,
        result: Result<Value, BackendError>,
    },
}

fn decode_message(line: &str) -> Result<IncomingMessage, ProtocolError> {
    let message = serde_json::from_str::<Value>(line).map_err(|_| ProtocolError::MalformedJson)?;
    let object = message
        .as_object()
        .ok_or(ProtocolError::UnknownMessageSchema)?;
    let protocol = object
        .get("protocol")
        .and_then(Value::as_u64)
        .ok_or(ProtocolError::UnknownMessageSchema)?;
    if protocol != IPC_PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: IPC_PROTOCOL_VERSION,
            received: Some(protocol),
        });
    }
    if let Some(notification) = object.get("notification") {
        if notification.as_str() == Some("storeChanged")
            && !object.contains_key("id")
            && !object.contains_key("result")
            && !object.contains_key("error")
        {
            return Ok(IncomingMessage::StoreChanged);
        }
        return Err(ProtocolError::UnknownMessageSchema);
    }

    let id = object
        .get("id")
        .and_then(Value::as_u64)
        .ok_or(ProtocolError::UnknownMessageSchema)?;
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        return Err(ProtocolError::UnknownMessageSchema);
    }
    let result = if has_result {
        Ok(object.get("result").cloned().unwrap_or(Value::Null))
    } else {
        let error = object
            .get("error")
            .and_then(Value::as_object)
            .ok_or(ProtocolError::UnknownMessageSchema)?;
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::UnknownMessageSchema)?;
        let text = error
            .get("message")
            .map(|message| message.as_str().ok_or(ProtocolError::UnknownMessageSchema))
            .transpose()?
            .unwrap_or("unknown error")
            .to_string();
        let calendar_error =
            serde_json::from_value::<crate::model::CalendarError>(Value::String(code.to_owned()))
                .ok();
        Err(match calendar_error {
            Some(error) => BackendError::Calendar(error),
            None => match code {
                "permissionDenied" => BackendError::PermissionDenied,
                "notFound" => BackendError::NotFound(text),
                "unsupported" => BackendError::Unsupported(text),
                "invalid" | "invalidAlarm" => BackendError::Invalid(text),
                _ => BackendError::Service(text),
            },
        })
    };
    Ok(IncomingMessage::Response { id, result })
}

fn repeatable_connection_error(error: &BackendError) -> BackendError {
    match error {
        BackendError::HelperExited => BackendError::HelperExited,
        BackendError::Protocol(error) => BackendError::Protocol(*error),
        BackendError::Unavailable(error) => BackendError::Unavailable(error.clone()),
        _ => unreachable!("reader terminal errors are connection-level only"),
    }
}

struct ConnectionResources {
    service: PathBuf,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    pending: Arc<Mutex<Pending>>,
    changes: broadcast::Sender<()>,
    states: broadcast::Sender<BackendState>,
    reconnect: mpsc::Sender<()>,
    spawn_lock: Mutex<()>,
}

pub struct EventKitBackend {
    resources: Arc<ConnectionResources>,
    pending: Arc<Mutex<Pending>>,
    sequence: AtomicU64,
    consecutive_timeouts: AtomicU64,
}

impl EventKitBackend {
    pub async fn connect(configured_path: Option<&Path>) -> Result<Self, BackendError> {
        let service = resolve_service(configured_path).ok_or_else(|| {
            let searched = service_search_paths(configured_path)
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            BackendError::Unavailable(format!(
                "tui-calendar-service not found; searched {searched}; run `make swift-release` or set service_path in config.toml"
            ))
        })?;
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(HashMap::new()));
        let (changes, _) = broadcast::channel(32);
        let (states, _) = broadcast::channel(16);
        let (reconnect, reconnect_rx) = mpsc::channel(1);
        let resources = Arc::new(ConnectionResources {
            service,
            writer: Arc::new(Mutex::new(None)),
            child: Arc::new(Mutex::new(None)),
            pending: pending.clone(),
            changes,
            states,
            reconnect,
            spawn_lock: Mutex::new(()),
        });
        Self::start_connection(&resources).await?;
        Self::spawn_reconnect_supervisor(resources.clone(), reconnect_rx);
        Ok(Self {
            resources,
            pending,
            sequence: AtomicU64::new(1),
            consecutive_timeouts: AtomicU64::new(0),
        })
    }

    fn spawn_reconnect_supervisor(
        resources: Arc<ConnectionResources>,
        mut requests: mpsc::Receiver<()>,
    ) {
        tokio::spawn(async move {
            const BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
            while requests.recv().await.is_some() {
                let mut attempt = 0;
                loop {
                    let _ = resources.states.send(BackendState::Restarting);
                    tokio::time::sleep(std::time::Duration::from_secs(BACKOFF[attempt])).await;
                    match Self::start_connection(&resources).await {
                        Ok(()) => break,
                        Err(_) => {
                            let _ = resources.states.send(BackendState::Disconnected);
                            attempt = (attempt + 1).min(BACKOFF.len() - 1);
                        }
                    }
                }
                // Collapse any failures received while a single supervisor was
                // reconnecting; a successful connection resets the backoff.
                while requests.try_recv().is_ok() {}
            }
        });
    }

    async fn start_connection(resources: &Arc<ConnectionResources>) -> Result<(), BackendError> {
        let _guard = resources.spawn_lock.lock().await;
        if resources.writer.lock().await.is_some() {
            return Ok(());
        }
        let mut child = Command::new(&resources.service)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                BackendError::Unavailable(format!("starting {}: {e}", resources.service.display()))
            })?;
        if std::env::var_os("TUI_CALENDAR_DEBUG_PIPELINE").is_some() {
            let metadata = std::fs::metadata(&resources.service).ok();
            let modified = metadata
                .and_then(|value| value.modified().ok())
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_secs().to_string())
                .unwrap_or_else(|| "unavailable".into());
            eprintln!(
                "tui-calendar pipeline helper path={} mtime_unix={} parent_pid={} child_pid={} protocol=v{}",
                resources.service.display(),
                modified,
                std::process::id(),
                child.id().unwrap_or(0),
                IPC_PROTOCOL_VERSION
            );
        }
        let writer = child
            .stdin
            .take()
            .ok_or_else(|| BackendError::Unavailable("service stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::Unavailable("service stdout unavailable".into()))?;
        *resources.writer.lock().await = Some(writer);
        *resources.child.lock().await = Some(child);
        let reader_resources = resources.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let terminal_error = loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match decode_message(&line) {
                        Ok(IncomingMessage::StoreChanged) => {
                            let _ = reader_resources.changes.send(());
                        }
                        Ok(IncomingMessage::Response { id, result }) => {
                            if let Some(sender) = reader_resources.pending.lock().await.remove(&id)
                            {
                                let _ = sender.send(result);
                            }
                        }
                        Err(error) => break BackendError::Protocol(error),
                    },
                    Ok(None) => break BackendError::HelperExited,
                    Err(error) => break BackendError::Unavailable(error.to_string()),
                }
            };
            if matches!(
                &terminal_error,
                BackendError::Protocol(ProtocolError::VersionMismatch { .. })
            ) {
                let _ = reader_resources.states.send(BackendState::ProtocolMismatch);
            }
            let mut waiting = reader_resources.pending.lock().await;
            for (_, sender) in waiting.drain() {
                let _ = sender.send(Err(repeatable_connection_error(&terminal_error)));
            }
            drop(waiting);
            *reader_resources.writer.lock().await = None;
            *reader_resources.child.lock().await = None;
            let _ = reader_resources.states.send(BackendState::Disconnected);
            let _ = reader_resources.reconnect.try_send(());
        });
        let _ = resources.states.send(BackendState::Connected);
        Ok(())
    }

    async fn disconnect(&self) {
        *self.resources.writer.lock().await = None;
        *self.resources.child.lock().await = None;
        let _ = self.resources.states.send(BackendState::Disconnected);
        let _ = self.resources.reconnect.try_send(());
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, BackendError> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let request = serde_json::to_vec(&json!({ "protocol": IPC_PROTOCOL_VERSION, "id": id, "method": method, "params": params }))
            .map_err(|e| BackendError::Invalid(e.to_string()))?;
        let (send, receive) = oneshot::channel();
        self.pending.lock().await.insert(id, send);
        let write_result = {
            let mut writer = self.resources.writer.lock().await;
            if writer.is_none() {
                drop(writer);
                self.pending.lock().await.remove(&id);
                let _ = self.resources.reconnect.try_send(());
                return Err(BackendError::Unavailable(
                    "calendar service is reconnecting".into(),
                ));
            }
            let writer = writer.as_mut().expect("writer checked above");
            if let Err(error) = writer.write_all(&request).await {
                Err(error.to_string())
            } else if let Err(error) = writer.write_all(b"\n").await {
                Err(error.to_string())
            } else if let Err(error) = writer.flush().await {
                Err(error.to_string())
            } else {
                Ok(())
            }
        };
        if let Err(error) = write_result {
            self.pending.lock().await.remove(&id);
            self.disconnect().await;
            return Err(BackendError::Unavailable(error));
        }
        let value = match tokio::time::timeout(std::time::Duration::from_secs(30), receive).await {
            Ok(Ok(result)) => {
                self.consecutive_timeouts.store(0, Ordering::Relaxed);
                if matches!(&result, Err(BackendError::PermissionDenied)) {
                    let _ = self.resources.states.send(BackendState::PermissionDenied);
                }
                result?
            }
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err(BackendError::Unavailable(
                    "calendar service disconnected".into(),
                ));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                if self.consecutive_timeouts.fetch_add(1, Ordering::Relaxed) + 1 >= 3 {
                    self.disconnect().await;
                }
                return Err(BackendError::Timeout(method.into()));
            }
        };
        serde_json::from_value(value)
            .map_err(|e| BackendError::Invalid(format!("{method} response: {e}")))
    }

    pub async fn pending_request_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

/// Explicit create DTO for the v2 provider boundary. `AllDay` carries only
/// floating date identity; it is never projected to a Rust UTC midnight.
fn create_event_payload(event: &EventDraft) -> Result<Value, BackendError> {
    let time = match &event.time {
        crate::model::EventTimeInput::Timed { start, end } if end > start => {
            json!({ "kind": "timed", "start": start, "end": end })
        }
        crate::model::EventTimeInput::AllDay {
            start_date,
            end_date_exclusive,
        } if end_date_exclusive > start_date => {
            json!({ "kind": "allDay", "startDate": start_date, "endDateExclusive": end_date_exclusive })
        }
        crate::model::EventTimeInput::LegacyAllDayUnknown { .. } => {
            return Err(BackendError::Invalid(
                "legacy all-day drafts cannot be created without floating date identity".into(),
            ));
        }
        _ => return Err(BackendError::Invalid("invalid event time range".into())),
    };
    let mut payload =
        serde_json::to_value(event).map_err(|error| BackendError::Invalid(error.to_string()))?;
    payload
        .as_object_mut()
        .expect("EventDraft always serializes as an object")
        .insert("time".into(), time);
    Ok(payload)
}

/// Compatibility provider boundary for the already-established update
/// contract. Create operations must use `create_event_payload` above.
fn legacy_update_event_payload(event: &EventDraft) -> Result<Value, BackendError> {
    let (start, end, all_day) = match &event.time {
        crate::model::EventTimeInput::Timed { start, end } => (*start, *end, false),
        crate::model::EventTimeInput::LegacyAllDayUnknown { start, end } => (*start, *end, true),
        crate::model::EventTimeInput::AllDay { .. } => {
            return Err(BackendError::Invalid(
                "typed all-day update must use timeMutation".into(),
            ));
        }
    };
    let mut payload =
        serde_json::to_value(event).map_err(|error| BackendError::Invalid(error.to_string()))?;
    let object = payload
        .as_object_mut()
        .expect("EventDraft always serializes as an object");
    object.remove("time");
    object.insert("start".into(), json!(start));
    object.insert("end".into(), json!(end));
    object.insert("allDay".into(), json!(all_day));
    Ok(payload)
}

fn installed_service_path(executable: &Path) -> Option<PathBuf> {
    executable
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("libexec/tui-calendar/tui-calendar-service"))
}

fn sibling_development_service_path(executable: &Path) -> Option<PathBuf> {
    executable
        .parent()
        .map(|parent| parent.join("tui-calendar-service"))
}

/// Ordered helper locations used by both connection and diagnostics. Keeping
/// this list authoritative means a broken packaged install reports the exact
/// runtime-relative path it expected, rather than a generic "not found".
pub fn service_search_paths(configured: Option<&Path>) -> Vec<PathBuf> {
    [
        configured.map(Path::to_path_buf),
        std::env::var_os("TUI_CALENDAR_SERVICE").map(PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|executable| installed_service_path(&executable)),
        // `make build` stages the development helper beside the Rust release
        // binary. This keeps `target/release/tui-calendar` runnable from any
        // working directory without affecting the installed libexec layout.
        std::env::current_exe()
            .ok()
            .and_then(|executable| sibling_development_service_path(&executable)),
        // Development fallback only: installed builds must use libexec above.
        Some(PathBuf::from(
            "macos-calendar-service/.build/release/tui-calendar-service",
        )),
        Some(PathBuf::from(
            "macos-calendar-service/.build/debug/tui-calendar-service",
        )),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn resolve_service(configured: Option<&Path>) -> Option<PathBuf> {
    service_search_paths(configured)
        .into_iter()
        .find(|path| path.is_file())
}

#[async_trait]
impl CalendarBackend for EventKitBackend {
    async fn authorization_status(&self) -> Result<AuthorizationStatus, BackendError> {
        self.call("authorizationStatus", json!({})).await
    }

    async fn request_access(&self) -> Result<AuthorizationStatus, BackendError> {
        self.call("requestAccess", json!({})).await
    }

    async fn calendars(&self) -> Result<Vec<CalendarInfo>, BackendError> {
        self.call("listCalendars", json!({})).await
    }
    async fn calendar_capabilities(&self) -> Result<CalendarCapabilities, BackendError> {
        self.call("calendar.capabilities", json!({})).await
    }
    async fn calendar_sources(&self) -> Result<Vec<CalendarSource>, BackendError> {
        self.call("calendar.sources", json!({})).await
    }
    async fn create_calendar(
        &self,
        request: CreateCalendarRequest,
    ) -> Result<CreateCalendarResponse, BackendError> {
        self.call("calendar.create", json!({ "calendar": request }))
            .await
    }
    async fn rename_calendar(
        &self,
        request: RenameCalendarRequest,
    ) -> Result<RenameCalendarResponse, BackendError> {
        self.call("calendar.rename", json!({ "calendar": request }))
            .await
    }
    async fn set_calendar_color(
        &self,
        request: SetCalendarColorRequest,
    ) -> Result<SetCalendarColorResponse, BackendError> {
        self.call("calendar.setColor", json!({ "calendar": request }))
            .await
    }
    async fn delete_calendar(
        &self,
        request: DeleteCalendarRequest,
    ) -> Result<DeleteCalendarResponse, BackendError> {
        self.call("calendar.delete", json!({ "calendar": request }))
            .await
    }

    async fn events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        calendar_ids: &[String],
    ) -> Result<Vec<Event>, BackendError> {
        self.call(
            "fetchEvents",
            json!({ "start": start, "end": end, "calendarIds": calendar_ids }),
        )
        .await
    }

    async fn fetch_events(
        &self,
        request: crate::model::FetchRequest,
        calendar_ids: &[String],
    ) -> Result<Vec<Event>, BackendError> {
        // Keep the established fields until the helper adopts its additive
        // calendar-date predicate. `fetchRequest` makes the distinct intent
        // explicit without treating either range as a timezone conversion.
        self.call(
            "fetchEvents",
            json!({
                "start": request.instant_range.start,
                "end": request.instant_range.end,
                "calendarIds": calendar_ids,
                "fetchRequest": request,
            }),
        )
        .await
    }

    async fn create_event(&self, event: EventDraft) -> Result<Event, BackendError> {
        self.call(
            "createEvent",
            json!({ "event": create_event_payload(&event)? }),
        )
        .await
    }

    async fn update_event(
        &self,
        event: EventDraft,
        span: EventSpan,
        alarms: AlarmMutation,
        time_mutation: EventTimeMutation,
    ) -> Result<Event, BackendError> {
        self.call(
            "updateEvent",
            json!({ "event": legacy_update_event_payload(&event)?, "span": span, "alarmMutation": alarms, "timeMutation": time_mutation }),
        )
        .await
    }

    async fn delete_event(
        &self,
        target: crate::model::EventMutationTarget,
        span: EventSpan,
    ) -> Result<(), BackendError> {
        self.call("deleteEvent", json!({ "id": target.provider_id, "calendarId": target.calendar_id, "occurrenceStart": target.occurrence_start, "span": span }))
            .await
    }

    async fn respond_to_invitation(
        &self,
        id: &str,
        response: InvitationResponse,
    ) -> Result<(), BackendError> {
        self.call(
            "respondInvitation",
            json!({ "id": id, "response": response }),
        )
        .await
    }

    fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.resources.changes.subscribe()
    }

    fn subscribe_backend_state(&self) -> broadcast::Receiver<BackendState> {
        self.resources.states.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    #[cfg(unix)]
    fn helper_script(body: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(file.path()).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(file.path(), permissions).unwrap();
        file
    }

    #[cfg(unix)]
    fn helper_with_one_fault(
        directory: &tempfile::TempDir,
        faulty_response: &str,
    ) -> std::path::PathBuf {
        let helper = directory.path().join("helper");
        let state = directory.path().join("fault-sent");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nstate={state:?}\nIFS= read -r _line\nif [ -f \"$state\" ]; then\n  printf '%s\\n' '{{\"protocol\":2,\"id\":1,\"result\":[]}}'\nelse\n  : > \"$state\"\n  printf '%s\\n' '{faulty_response}'\nfi\n",
                state = state.display().to_string(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(unix)]
    async fn wait_for_reconnect(states: &mut broadcast::Receiver<BackendState>) {
        for _ in 0..4 {
            if tokio::time::timeout(std::time::Duration::from_secs(3), states.recv())
                .await
                .unwrap()
                .unwrap()
                == BackendState::Connected
            {
                return;
            }
        }
        panic!("helper did not reconnect");
    }

    #[test]
    fn explicit_helper_path_wins_when_present() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            resolve_service(Some(temp.path())),
            Some(temp.path().to_path_buf())
        );
    }

    #[test]
    fn homebrew_style_runtime_layout_resolves_to_the_sibling_libexec_helper() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("bin/tui-calendar");
        let helper = root
            .path()
            .join("libexec/tui-calendar/tui-calendar-service");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&executable, "binary").unwrap();
        std::fs::write(&helper, "helper").unwrap();

        assert_eq!(installed_service_path(&executable), Some(helper));
    }

    #[test]
    fn make_build_layout_resolves_to_the_release_binary_sibling_helper() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("target/release/tui-calendar");
        let helper = root.path().join("target/release/tui-calendar-service");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "binary").unwrap();
        std::fs::write(&helper, "helper").unwrap();

        assert_eq!(sibling_development_service_path(&executable), Some(helper));
    }

    #[test]
    fn helper_search_paths_preserve_an_explicit_missing_path_for_diagnostics() {
        let configured = PathBuf::from("/missing/tui-calendar-service");
        let paths = service_search_paths(Some(&configured));
        assert_eq!(paths.first(), Some(&configured));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn crashed_request_drains_pending_and_restarts_the_helper() {
        let helper = helper_script("IFS= read -r _line\nexit 0");
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let mut states = backend.subscribe_backend_state();

        let result = backend.calendars().await;
        assert!(matches!(result, Err(BackendError::HelperExited)));
        assert_eq!(backend.pending_request_count().await, 0);

        let mut observed = Vec::new();
        while observed.len() < 3 {
            observed.push(
                tokio::time::timeout(std::time::Duration::from_secs(3), states.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(observed.contains(&BackendState::Disconnected));
        assert!(observed.contains(&BackendState::Restarting));
        assert!(observed.contains(&BackendState::Connected));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_json_fails_immediately_and_the_helper_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let helper = helper_with_one_fault(&directory, "not json");
        let backend = EventKitBackend::connect(Some(&helper)).await.unwrap();
        let mut states = backend.subscribe_backend_state();

        assert!(matches!(
            backend.calendars().await,
            Err(BackendError::Protocol(ProtocolError::MalformedJson))
        ));
        assert_eq!(backend.pending_request_count().await, 0);
        wait_for_reconnect(&mut states).await;
        assert_eq!(backend.pending_request_count().await, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unknown_message_schema_fails_immediately_and_the_helper_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let helper = helper_with_one_fault(&directory, "{\"protocol\":2,\"id\":1}");
        let backend = EventKitBackend::connect(Some(&helper)).await.unwrap();
        let mut states = backend.subscribe_backend_state();

        assert!(matches!(
            backend.calendars().await,
            Err(BackendError::Protocol(ProtocolError::UnknownMessageSchema))
        ));
        assert_eq!(backend.pending_request_count().await, 0);
        wait_for_reconnect(&mut states).await;
        assert_eq!(backend.pending_request_count().await, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn version_mismatch_is_typed_and_the_helper_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let helper = helper_with_one_fault(&directory, "{\"protocol\":1,\"id\":1,\"result\":[]}");
        let backend = EventKitBackend::connect(Some(&helper)).await.unwrap();
        let mut states = backend.subscribe_backend_state();

        assert!(matches!(
            backend.calendars().await,
            Err(BackendError::Protocol(ProtocolError::VersionMismatch {
                expected: IPC_PROTOCOL_VERSION,
                received: Some(1),
            }))
        ));
        assert_eq!(backend.pending_request_count().await, 0);
        wait_for_reconnect(&mut states).await;
        assert_eq!(backend.pending_request_count().await, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn structured_calendar_error_decodes_by_code() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"error\":{\"code\":\"permission_denied\",\"message\":\"opaque\"}}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        assert!(matches!(
            backend.calendar_sources().await,
            Err(BackendError::Calendar(
                crate::model::CalendarError::PermissionDenied
            ))
        ));
    }

    #[test]
    fn typed_timed_create_payload_contains_only_instant_time_fields() {
        let mut draft = EventDraft::new(
            "work".into(),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        );
        draft.time = crate::model::EventTimeInput::timed(
            "2026-09-10T09:00:00Z".parse().unwrap(),
            "2026-09-10T10:00:00Z".parse().unwrap(),
        )
        .unwrap();
        let payload = create_event_payload(&draft).unwrap();
        assert_eq!(payload["time"]["kind"], "timed");
        assert!(payload["time"].get("start").is_some());
        assert!(payload["time"].get("end").is_some());
        assert!(payload.get("start").is_none());
        assert!(payload.get("allDay").is_none());
    }

    #[test]
    fn typed_all_day_create_payload_carries_only_floating_dates() {
        let mut draft = EventDraft::new(
            "work".into(),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        );
        draft.time = crate::model::EventTimeInput::all_day(
            chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
        )
        .unwrap();
        let payload = create_event_payload(&draft).unwrap();
        assert_eq!(
            payload["time"],
            json!({
                "kind": "allDay",
                "startDate": "2026-09-10",
                "endDateExclusive": "2026-09-13",
            })
        );
        assert!(!payload.to_string().contains("T00:00:00"));
        assert!(payload.get("start").is_none());
    }

    #[test]
    fn invalid_or_legacy_create_time_does_not_produce_an_ipc_payload() {
        let mut draft = EventDraft::new(
            "work".into(),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        );
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        draft.time = crate::model::EventTimeInput::AllDay {
            start_date: date,
            end_date_exclusive: date,
        };
        assert!(matches!(
            create_event_payload(&draft),
            Err(BackendError::Invalid(_))
        ));
        draft.time = crate::model::EventTimeInput::legacy_all_day_unknown(
            "2026-09-10T00:00:00Z".parse().unwrap(),
            "2026-09-11T00:00:00Z".parse().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            create_event_payload(&draft),
            Err(BackendError::Invalid(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_create_round_trips_calendar_metadata() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"result\":{\"id\":\"calendar-test-created-1\",\"sourceId\":\"source-test-1\",\"title\":\"Test Calendar\",\"color\":\"#336699\",\"isWritable\":true,\"permissions\":{\"canCreateEvents\":true,\"canModifyEvents\":true,\"canModifyMetadata\":false,\"canDelete\":false}}}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let calendar = backend
            .create_calendar(crate::model::CreateCalendarRequest {
                title: "Test Calendar".into(),
                color: "#336699".into(),
                source_id: "source-test-1".into(),
            })
            .await
            .unwrap();
        assert_eq!(calendar.id, "calendar-test-created-1");
        assert_eq!(calendar.source_id, "source-test-1");
        assert_eq!(calendar.color, "#336699");
        assert!(calendar.permissions.can_create_events);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_payload_round_trips_additive_all_day_date_metadata() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"result\":[{\"id\":\"weekly-occurrence\",\"calendarId\":\"calendar-1\",\"title\":\"Weekly all-day\",\"start\":\"2026-09-09T22:00:00Z\",\"end\":\"2026-09-10T22:00:00Z\",\"allDay\":true,\"allDayStartDate\":\"2026-09-10\",\"allDayEndDateExclusive\":\"2026-09-11\",\"hasRecurrence\":true},{\"id\":\"monthly-occurrence\",\"calendarId\":\"calendar-1\",\"title\":\"Monthly all-day\",\"start\":\"2026-09-11T22:00:00Z\",\"end\":\"2026-09-12T22:00:00Z\",\"allDay\":true,\"allDayStartDate\":\"2026-09-12\",\"allDayEndDateExclusive\":\"2026-09-13\",\"hasRecurrence\":true},{\"id\":\"dst-occurrence\",\"calendarId\":\"calendar-1\",\"title\":\"DST all-day\",\"start\":\"2026-03-27T23:00:00Z\",\"end\":\"2026-03-30T22:00:00Z\",\"allDay\":true,\"allDayStartDate\":\"2026-03-28\",\"allDayEndDateExclusive\":\"2026-03-31\",\"hasRecurrence\":true},{\"id\":\"legacy-all-day\",\"calendarId\":\"calendar-1\",\"title\":\"Legacy\",\"start\":\"2026-09-09T22:00:00Z\",\"end\":\"2026-09-10T22:00:00Z\",\"allDay\":true}]}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let events = backend
            .events(
                "2026-09-01T00:00:00Z".parse().unwrap(),
                "2026-10-01T00:00:00Z".parse().unwrap(),
                &[],
            )
            .await
            .unwrap();
        let range = |id| {
            events
                .iter()
                .find(|event| event.id == id)
                .unwrap()
                .all_day_date_range()
        };
        assert_eq!(
            range("weekly-occurrence"),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
            ))
        );
        assert_eq!(
            range("monthly-occurrence"),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 9, 12).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 9, 13).unwrap(),
            ))
        );
        assert_eq!(
            range("dst-occurrence"),
            Some((
                chrono::NaiveDate::from_ymd_opt(2026, 3, 28).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
            ))
        );
        assert_eq!(range("legacy-all-day"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_events_transports_optional_calendar_date_intent() {
        let helper = helper_script(
            "IFS= read -r line\ncase \"$line\" in\n  *fetchRequest*allDayRange*2026-09-10*) printf '{\\\"protocol\\\":2,\\\"id\\\":1,\\\"result\\\":[]}\\n' ;;\n  *) printf '{\\\"protocol\\\":2,\\\"id\\\":1,\\\"error\\\":{\\\"code\\\":\\\"invalid\\\",\\\"message\\\":\\\"missing fetch intent\\\"}}\\n' ;;\nesac",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let events = backend
            .fetch_events(
                crate::model::FetchRequest {
                    instant_range: crate::model::InstantRange {
                        start: "2026-09-10T00:00:00Z".parse().unwrap(),
                        end: "2026-09-11T00:00:00Z".parse().unwrap(),
                    },
                    all_day_range: Some(crate::model::CalendarDateRange {
                        start_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
                        end_date_exclusive: chrono::NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
                    }),
                },
                &[],
            )
            .await
            .unwrap();
        assert!(events.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_create_failure_codes_round_trip() {
        for (code, expected) in [
            ("invalid_title", crate::model::CalendarError::InvalidTitle),
            ("invalid_color", crate::model::CalendarError::InvalidColor),
            (
                "source_not_found",
                crate::model::CalendarError::SourceNotFound,
            ),
            (
                "permission_denied",
                crate::model::CalendarError::PermissionDenied,
            ),
        ] {
            let helper = helper_script(&format!(
                "IFS= read -r _line\nprintf '{{\"protocol\":2,\"id\":1,\"error\":{{\"code\":\"{code}\",\"message\":\"opaque\"}}}}\\n'"
            ));
            let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
            assert!(
                matches!(backend.create_calendar(crate::model::CreateCalendarRequest { title: "x".into(), color: "#336699".into(), source_id: "source-test-1".into() }).await, Err(BackendError::Calendar(error)) if error == expected)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_rename_round_trips_updated_metadata() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"result\":{\"id\":\"work\",\"sourceId\":\"source-test-1\",\"title\":\"Work Renamed\",\"color\":\"#336699\",\"isWritable\":true,\"permissions\":{\"canCreateEvents\":true,\"canModifyEvents\":true,\"canModifyMetadata\":true,\"canDelete\":false}}}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let calendar = backend
            .rename_calendar(RenameCalendarRequest {
                calendar_id: "work".into(),
                title: "Work Renamed".into(),
            })
            .await
            .unwrap();
        assert_eq!(calendar.id, "work");
        assert_eq!(calendar.title, "Work Renamed");
        assert!(calendar.permissions.can_modify_metadata);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_capabilities_advertise_only_verified_mutations() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"result\":{\"canListSources\":true,\"canCreate\":true,\"canUpdate\":true,\"canDelete\":true,\"canChangeColor\":true}}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let capabilities = backend.calendar_capabilities().await.unwrap();
        assert!(capabilities.can_create);
        assert!(capabilities.can_update);
        assert!(capabilities.can_change_color);
        assert!(capabilities.can_delete);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_rename_failure_codes_round_trip() {
        for (code, expected) in [
            ("invalid_title", crate::model::CalendarError::InvalidTitle),
            ("not_found", crate::model::CalendarError::NotFound),
            (
                "cannot_modify_metadata",
                crate::model::CalendarError::CannotModifyMetadata,
            ),
            (
                "permission_denied",
                crate::model::CalendarError::PermissionDenied,
            ),
        ] {
            let helper = helper_script(&format!(
                "IFS= read -r _line\nprintf '{{\"protocol\":2,\"id\":1,\"error\":{{\"code\":\"{code}\",\"message\":\"opaque\"}}}}\\n'"
            ));
            let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
            assert!(
                matches!(backend.rename_calendar(RenameCalendarRequest { calendar_id: "work".into(), title: "Work Renamed".into() }).await, Err(BackendError::Calendar(error)) if error == expected)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_set_color_round_trips_updated_metadata() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"result\":{\"id\":\"work\",\"sourceId\":\"source-test-1\",\"title\":\"Work\",\"color\":\"#336699\",\"isWritable\":true,\"permissions\":{\"canCreateEvents\":true,\"canModifyEvents\":true,\"canModifyMetadata\":true,\"canDelete\":false}}}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let calendar = backend
            .set_calendar_color(SetCalendarColorRequest {
                calendar_id: "work".into(),
                color: "#336699".into(),
            })
            .await
            .unwrap();
        assert_eq!(calendar.id, "work");
        assert_eq!(calendar.color, "#336699");
        assert!(calendar.permissions.can_modify_metadata);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_set_color_failure_codes_round_trip() {
        for (code, expected) in [
            ("invalid_color", crate::model::CalendarError::InvalidColor),
            ("not_found", crate::model::CalendarError::NotFound),
            (
                "cannot_modify_metadata",
                crate::model::CalendarError::CannotModifyMetadata,
            ),
            (
                "permission_denied",
                crate::model::CalendarError::PermissionDenied,
            ),
        ] {
            let helper = helper_script(&format!(
                "IFS= read -r _line\nprintf '{{\"protocol\":2,\"id\":1,\"error\":{{\"code\":\"{code}\",\"message\":\"opaque\"}}}}\\n'"
            ));
            let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
            assert!(
                matches!(backend.set_calendar_color(SetCalendarColorRequest { calendar_id: "work".into(), color: "#336699".into() }).await, Err(BackendError::Calendar(error)) if error == expected)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_delete_round_trips_acknowledged_id() {
        let helper = helper_script(
            "IFS= read -r _line\nprintf '{\"protocol\":2,\"id\":1,\"result\":{\"calendarId\":\"calendar-delete-test\"}}\\n'",
        );
        let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
        let response = backend
            .delete_calendar(DeleteCalendarRequest {
                calendar_id: "calendar-delete-test".into(),
            })
            .await
            .unwrap();
        assert_eq!(response.calendar_id, "calendar-delete-test");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn calendar_delete_failure_codes_round_trip() {
        for (code, expected) in [
            ("not_found", crate::model::CalendarError::NotFound),
            ("cannot_delete", crate::model::CalendarError::CannotDelete),
            (
                "permission_denied",
                crate::model::CalendarError::PermissionDenied,
            ),
            ("unsupported", crate::model::CalendarError::Unsupported),
        ] {
            let helper = helper_script(&format!(
                "IFS= read -r _line\nprintf '{{\"protocol\":2,\"id\":1,\"error\":{{\"code\":\"{code}\",\"message\":\"opaque\"}}}}\\n'"
            ));
            let backend = EventKitBackend::connect(Some(helper.path())).await.unwrap();
            assert!(
                matches!(backend.delete_calendar(DeleteCalendarRequest { calendar_id: "calendar-delete-test".into() }).await, Err(BackendError::Calendar(error)) if error == expected)
            );
        }
    }
}
