# EventKit service protocol

The Rust UI and Swift service communicate through newline-delimited JSON over stdin/stdout. Protocol v2 is required on every message. Every request has a numeric `id`, a `method`, and a `params` object. Responses repeat the `id` and contain either `result` or a typed `error`; unknown fields are ignored for forward compatibility.

```json
{"protocol":2,"id":1,"method":"fetchEvents","params":{"start":"2026-08-01T00:00:00Z","end":"2026-09-01T00:00:00Z","calendarIds":[],"fetchRequest":{"instantRange":{"start":"2026-08-01T00:00:00Z","end":"2026-09-01T00:00:00Z"},"allDayRange":{"startDate":"2026-08-01","endDateExclusive":"2026-09-01"}}}}
{"protocol":2,"id":1,"result":[]}
```

The service may emit an unsolicited change notification:

```json
{"protocol":2,"notification":"storeChanged"}
```

Supported methods:

| Method | Purpose |
| --- | --- |
| `authorizationStatus` | Read the current EventKit permission state |
| `requestAccess` | Request full event access |
| `listCalendars` | Discover EventKit calendars and source metadata |
| `fetchEvents` | Fetch expanded EventKit occurrences using UTC transport plus optional calendar-date intent |
| `createEvent` | Create an `EKEvent` |
| `updateEvent` | Update one or future recurring occurrences |
| `deleteEvent` | Delete one or future recurring occurrences |
| `respondInvitation` | Reserved for RSVP; returns `unsupported` because the public EventKit API is read-only for responses |
| `calendar.capabilities` | Report helper-level calendar management support |
| `calendar.sources` | List stable EventKit sources |
| `calendar.create` | Create an event calendar in an explicitly selected source |
| `calendar.rename` | Rename one calendar by stable ID |
| `calendar.setColor` | Change one calendar's `#RRGGBB` display color |
| `calendar.delete` | Destructively remove one calendar by stable ID |

Calendar-domain errors use stable snake-case `error.code` values such as `permission_denied`, `not_found`, `unsupported`, `invalid`, `read_only`, and `source_not_found`. The message is informational; clients must branch on the code.

## IPC troubleshooting and recovery

The Rust client treats the helper boundary as fail-closed. A helper EOF is a
typed **helper exited** failure; a request deadline is a typed **timeout**;
invalid JSON is **malformed JSON**; an envelope without a valid notification
or response shape is **unknown message schema**; and any protocol version
other than v2 is a **version mismatch**. These are transport failures, not
calendar-domain errors, and no EventKit mutation is retried or synthesized.

For a helper exit, corrupted response, or version mismatch, all pending IPC
requests fail immediately, the backend reports its disconnected/restarting
state, and the existing reconnect supervisor starts a fresh helper when
possible. Cached UI state remains available while it reconnects. If a
version mismatch persists, rebuild or reinstall the matching helper; for a
helper crash or malformed response, use `tui-calendar doctor` and the helper
path reported there when collecting diagnostics.

## Alarm update intent

`createEvent` carries its concrete `event.alarms` array. `updateEvent` also
carries `alarmMutation`: `{"kind":"preserve"}` leaves the existing native
`EKAlarm` objects untouched, while `{"kind":"replace","alarms":[...]}`
replaces them atomically. An empty replacement intentionally clears alarms.
Each replacement alarm must contain exactly one of `relativeSeconds` or
`absoluteDate`; malformed entries fail the complete update rather than being
silently dropped. Event payloads include `isEditable`; missing metadata from
older cached payloads is treated conservatively as protected by the client.
Current clients always send an explicit mutation. For compatibility with an
older client, an omitted update field is interpreted as `preserve`, never as
an implicit replacement.

Event payloads may include `timeZoneProvenance`: `explicitEvent` means EventKit supplied `event.timeZone`; `helperFallback` means the helper supplied its current timezone only for compatibility; a missing field decodes as `unknown`. The distinction is additive under IPC v2 and prevents all-day absolute-alarm editing from treating a fallback timezone as provider metadata.

### All-day calendar-date identity

For newly fetched all-day events, the helper additionally provides optional
`allDayStartDate` and `allDayEndDateExclusive` fields as ISO calendar dates
(`YYYY-MM-DD`). They identify the authoritative floating interval
`[startDate, endDateExclusive)` at the EventKit boundary, independent of the
legacy UTC `start` and `end` compatibility instants. A one-day event on 10
September is therefore `2026-09-10` through `2026-09-11`; an event displayed
from 10 through 12 September ends exclusively on `2026-09-13`.

The fields are additive under IPC v2 and absent from older cached payloads.
Missing fields mean that trusted all-day date identity is unavailable: clients
must not reconstruct it from legacy UTC timestamps. The staged migration is:
metadata transport first; then rendering/search; then editor/save; then
Quick Add and provider mutations; finally range and recurring-boundary audit.

For updates of existing events, `timeMutation` is explicit. `{"kind":"preserve"}`
leaves EventKit `startDate`, `endDate`, `isAllDay`, and `timeZone` untouched.
`{"kind":"replaceAllDay","startDate":"YYYY-MM-DD","endDateExclusive":"YYYY-MM-DD"}`
replaces a trusted floating all-day interval using its exclusive domain end;
the helper performs provider-date construction and never requires Rust to send
UTC midnight instants. Existing timed updates retain `replaceLegacy` behavior.
Ambiguous legacy all-day payloads use `preserve` for unrelated edits rather
than being silently reconstructed from compatibility timestamps.

## Create-time temporal input

`EventDraft.time` is the sole temporal authority for new events. A timed draft
uses `{"kind":"timed","start":"<ISO-8601 UTC>","end":"<ISO-8601 UTC>"}`.
An all-day draft uses
`{"kind":"allDay","startDate":"YYYY-MM-DD","endDateExclusive":"YYYY-MM-DD"}`
and represents the floating, half-open calendar-date interval
`[startDate, endDateExclusive)`. Rust must not construct all-day events from
UTC midnight timestamps.

The `createEvent` IPC request carries this typed `time` object directly. Swift
parses timed ISO instants exactly as before, while all-day creation converts
only `startDate` and `endDateExclusive` through the shared EventKit calendar
component helper. Rust never creates all-day UTC midnight timestamps. Old
serialized all-day drafts decode only as `legacyAllDayUnknown`, an explicit
compatibility state whose UTC instants are not calendar-date identity; create
requests with that state are rejected safely rather than guessed.

The structured EventForm now emits `allDay` create input directly from its
`YYYY-MM-DD` start and inclusive end-date buffers, converting the UI end to the
exclusive domain end with calendar-date arithmetic. Existing-event updates
continue to use `timeMutation`. Quick Add and duplicate creation remain on
their separately staged compatibility migrations.

Quick Add now follows the same split: all-day input resolves natural-language
dates into `EventTimeInput::AllDay` directly, while timed input remains
instant-based in the configured local time zone. Relative alerts for all-day
Quick Add events remain rejected. Duplicate creation likewise uses trusted
all-day date metadata directly; an old all-day event without that metadata is
rejected rather than guessed from its compatibility instants.

## Fetch and cache boundaries

RangeLoader and `fetchEvents` retain their existing half-open UTC transport
intervals for provider queries, fetched-range coverage, and timed events. Once
the helper returns an all-day event, its trusted
`[allDayStartDate, allDayEndDateExclusive)` interval is authoritative for
cache reload and UI membership: intersection is `eventStart < queryEnd &&
eventEnd > queryStart`. This deliberately ignores the compatibility `start` /
`end` instants, whose timezone-dependent values are not all-day identity.

EventKit returns expanded occurrences from its event predicate and serializes
each through the same `eventJSON` path, so recurring all-day instances carry
the same optional trusted date metadata. Older payloads without it remain on
the isolated legacy compatibility path.

The current provider request protocol carries UTC instants only. Therefore it
does not itself express a helper-timezone-independent calendar-date query; the
trusted dates become authoritative only after an event has been returned. A
future cross-timezone inclusion guarantee would require an additive
calendar-date span alongside each UTC transport range, propagated through
RangeLoader gap handling and merged/deduplicated at the EventKit predicate.
This migration deliberately does not infer that span from timezone offsets.

`fetchEvents` now additionally carries `fetchRequest`:

```json
{
  "instantRange": {"start":"2026-09-10T00:00:00Z","end":"2026-09-11T00:00:00Z"},
  "allDayRange": {"startDate":"2026-09-10","endDateExclusive":"2026-09-11"}
}
```

`instantRange` remains required for timed-event transport and cache coverage.
`allDayRange` is optional and has independent half-open calendar-date
semantics. The current Swift helper accepts this forward-compatible request
but still uses its established UTC predicate; it does not claim that the
instant range is equivalent to floating all-day inclusion.

### EventKit date-query audit

The public EventKit query used by the helper is
`EKEventStore.predicateForEvents(withStart:end:calendars:)`. Its `start` and
`end` parameters are absolute `Date` values. The helper parses the legacy
ISO-8601 `start`/`end` request fields and sends this one predicate, which
returns both timed and all-day occurrences together. There is no public
EventKit calendar-date or floating-all-day predicate to consume
`allDayRange` directly.

Consequently, a second all-day query would have to convert the date range into
absolute `Date` bounds in a chosen time zone. That is precisely the offset or
synthetic-midnight approach this protocol forbids, so the provider-side
all-day path is intentionally deferred. The current one-predicate path needs
no explicit merge or deduplication; should EventKit later provide a true
calendar-date predicate, the two result sets must be merged by
`eventIdentifier` (with the existing `calendarItemIdentifier` fallback) before
the sole `eventJSON` normalization path runs.

Internally, the editor distinguishes `RelativeBasic`, `AbsoluteBasic`, and
`Protected` alarms. A `RelativeBasic` alarm is an editable whole-minute
relative offset. An `AbsoluteBasic` alarm is a losslessly represented UTC
instant with no unsupported provider metadata. Absolute alarms are currently
structurally represented only: no date/time controls expose them yet, and the
relative-alarm manager keeps a collection containing one read-only. Older
cached alarm records and any provider alarm without explicit editability
metadata remain `Protected`.

Event start/end editor fields are local wall-clock input converted to UTC by
the Rust `Local` time zone. Absolute alarms remain canonical UTC instants. A
future absolute-alarm UI must explicitly select its input time zone and handle
DST gaps and overlaps before constructing the instant; this protocol does not
infer either.

## Recurring mutation scopes

Prepared update and delete requests may include `recurrenceScope` only as
`thisEvent` or `futureEvents`. A scope is required before a recurring mutation
can be dispatched; non-recurring mutations omit it. `entireSeries` is not a
supported protocol value and must be rejected as unsupported rather than
silently mapped to another scope. The Rust `RecurrenceMutationScope` maps
directly to EventKit's `EKSpan.thisEvent` and `EKSpan.futureEvents` at the
provider boundary.

### Recurring all-day identity

EventKit predicate expansion returns `EKEvent` occurrences. Each returned
occurrence is independently passed through `eventJSON`; when it is all-day,
the existing `normalizedAllDayDateRange` serializer emits its trusted
`allDayStartDate` and `allDayEndDateExclusive` fields. This applies equally to
weekly and monthly occurrences, including intervals crossing DST, without
elapsed-hour arithmetic or a Rust-side time-zone conversion.

`updateEvent` likewise serializes the EventKit object after saving it with
either `EKSpan.thisEvent` or `EKSpan.futureEvents`. The recurrence scope is
not an alternate all-day serialization path. Cache reconciliation stores the
same raw payload, and search and visible-date filters use the trusted
half-open date interval when it is present. Detached/split series are
identified by their provider event ID; selection reconciliation remains based
on that stable ID rather than title or time.

### `calendar.create`

Creates an event calendar in the explicitly selected EventKit source. Request params are `{"calendar":{"title":"Work","color":"#336699","sourceId":"<EKSource.sourceIdentifier>"}}`. Title must be non-blank, color is `#RRGGBB`, and `sourceId` is mandatory; the helper never falls back to a source title, default source, or first writable source. The result is `CalendarInfo` with its ID, title, color, `sourceId`, and permissions. Stable errors include `invalid_title`, `invalid_color`, `source_not_found`, `permission_denied`, and `unsupported`.

The service is the only component linked to EventKit. The UI model and cache are provider-neutral and can be exercised with the mock backend.

## Offline conflict-handling audit

The SQLite cache stores provider event payloads by stable event ID and uses
authoritative range refreshes after mutations, but it is not an authoritative
event store and has no mutation queue. An update or delete currently sends the
cached event ID directly to EventKit; successful provider responses and the
following targeted refresh replace cache state.

EventKit exposes `EKCalendarItem.lastModifiedDate`, but the public
`saveEvent` and `removeEvent` APIs accept no expected-version token or
conditional-save precondition. The current IPC payload deliberately does not
transport that date. Comparing a cached timestamp with a separately fetched
timestamp before saving would leave a time-of-check/time-of-use race and could
still overwrite a newer remote change. Therefore the application does not
advertise stale-mutation detection or create a timestamp-only conflict model.

A future conflict-safe offline-editing design needs a provider-supported
opaque revision / compare-and-save contract (or a provider operation with an
equivalent atomic precondition), propagated in event payloads and mutation
requests. Until then, provider responses win after each mutation and remote
deletions are surfaced by authoritative refresh rather than recreated locally.

### `calendar.rename`

Renames a calendar by its stable `calendarId`: `{"calendar":{"calendarId":"<EKCalendar.calendarIdentifier>","title":"Work Renamed"}}`. The response is the updated provider-neutral `CalendarInfo`; a blank title is rejected with `invalid_title`. The helper returns `not_found`, `permission_denied`, or `cannot_modify_metadata` when the calendar cannot be changed. Clients must consult `calendar.capabilities.canUpdate` before calling this method.

### `calendar.setColor`

Changes a calendar's display color with `{"calendar":{"calendarId":"<EKCalendar.calendarIdentifier>","color":"#336699"}}`. The response is the updated `CalendarInfo`. Only `#RRGGBB` is accepted; malformed values return `invalid_color`. The helper returns `not_found`, `permission_denied`, or `cannot_modify_metadata` when the calendar cannot be updated. Clients must consult `calendar.capabilities.canChangeColor` before calling this method.

### `calendar.delete`

Deletes a calendar with `{"calendar":{"calendarId":"<EKCalendar.calendarIdentifier>"}}` and returns `{"calendarId":"..."}` after EventKit confirms removal. This is destructive: the future UI must require explicit user confirmation before calling it, and calendar deletion is distinct from deleting an event. The helper does not select a fallback calendar or source. It returns `not_found`, `permission_denied`, `cannot_delete`, or `unsupported` when the operation is rejected. Clients must consult `calendar.capabilities.canDelete` before calling this method.
