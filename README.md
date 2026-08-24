# Terminal Calendar

Terminal Calendar is a keyboard-first macOS calendar client built with Rust, Ratatui, Swift, and Apple's EventKit framework. It discovers the same iCloud, Google, Exchange, CalDAV, local, subscribed, and birthday calendars already configured in macOS—without handling account credentials or implementing a second synchronization stack.

The interaction model is deliberately close to “Neovim + Apple Calendar”: day, week, month, and agenda views; Vim-style commands; full event editing; native recurrence rules and alarms; calendar filters; global search; and a persistent offline cache.

## What works

- Day timeline with current-time marker, overlapping events, and calendar colors
- Shared day/week time grid with all-day lanes, vertical scrolling, overlap columns, selected-event contrast, and current-time marker
- Seven-column week view, a selectable month grid with ISO week numbers, and a date-grouped agenda with Today/Tomorrow labels
- Optional account-grouped calendar sidebar, context-aware searchable command palette (`:`), and fast date jump
- Read-only Calendar Manager (`gc`) with source, permissions, capability, color, and visibility details
- Capability-aware Calendar Manager create, rename, color, and cancel-by-default delete workflows with typed errors and metadata-only refreshes
- Offline deterministic Quick Add (`a`) with a live preview, explicit `Ctrl-S` save, `#Calendar` / `@Location` tokens, English/German date words, and structured-editor fallback
- Calendar discovery with account, provider, color, permission, and writable state
- Create, inspect, edit, and delete events, including one/future recurring occurrence scope
- Native `EKRecurrenceRule` daily, weekly, monthly, yearly, interval, ordinal-weekday, count, and end-date representation
- Structured multiple basic relative reminders, with native/provider-specific alarms preserved read-only
- Title, calendar, dates, all-day, location, notes, URL, timezone, availability, organizer, and participant display
- Offline-first global search over cached title, notes, location, and participants, with stable-calendar, date, all-day, and recurrence filters
- Debounced EventKit change notifications plus periodic background refresh over a daily-use cache window; rendering never waits for EventKit or SQLite
- SQLite WAL cache with atomic refreshes, schema migrations, fast startup, and offline browsing; no passwords, tokens, or calendar credentials
- First-run permission handling and clear denied/revoked states
- Mock backend via `--mock` for development and CI

### EventKit invitation limitation

Apple's public EventKit API exposes event organizers, attendees, and participant responses as read-only. It provides no public API for adding invitees or changing the current user's RSVP. Terminal Calendar clearly displays this read-only participant information and never presents attendee-editing or RSVP controls. Those actions must be completed in Calendar.app. All other supported event fields are saved through EventKit.

## Requirements

- macOS 13 or newer (macOS 14+ uses the full-access Calendar permission API)
- A current Rust toolchain
- Xcode Command Line Tools or Xcode with Swift Package Manager
- A true-color, Unicode-capable terminal

## Build and run

```sh
make build
./target/release/tui-calendar
```

Installed builds keep the Rust binary in `bin/` and the native helper in `libexec/tui-calendar/`. The binary resolves that runtime-relative helper automatically, so Homebrew installations do not depend on the source tree or the current working directory. `service_path` and `TUI_CALENDAR_SERVICE` remain explicit overrides for development and diagnostics.

To explore the UI without touching Calendar data:

```sh
cargo run -- --mock
```

Run the test suite and lints with:

```sh
make test
make lint
```

The suite includes child-process recovery coverage and macOS PTY smoke tests.
Those tests use disposable JSON-lines helpers injected through a temporary
`service_path`; they never modify or exercise failure behavior in the native
EventKit helper.

Check a local installation without opening the TUI:

```sh
tui-calendar doctor
```

It reports the binary version, macOS and architecture, config/database paths, SQLite integrity/schema version, EventKit availability, authorization state, calendar count, and IPC protocol version. It never prints event content.

If multiple Xcode/Command Line Tools SDKs are installed, select the matching toolchain with `xcode-select`. `SWIFT_BUILD_FLAGS` can pass an explicit SDK to Make when diagnosing a mismatched local toolchain:

```sh
make build SWIFT_BUILD_FLAGS="--sdk $(xcrun --sdk macosx --show-sdk-path)"
```

## Calendar permission

On first launch, the native service displays the macOS Calendar permission prompt with this explanation:

> Terminal Calendar requires access to macOS calendars.

Choose full access. If permission is denied or later revoked, the application remains usable for offline browsing and shows the corrective System Settings path:

`System Settings → Privacy & Security → Calendars`

The usage description is embedded directly in the Swift command-line executable. Moving or rebuilding an unsigned development binary can cause macOS to ask again because TCC identifies the executable by its code identity.

## Configuration and data

The default configuration is created at `~/.config/tui-calendar/config.toml`:

```toml
theme = "dark"
week_start = "monday"
time_format = "24h"
default_view = "week"
show_week_numbers = true
refresh_seconds = 60
backend = "eventkit"

[event]
default_duration_minutes = 60
default_start_time = "09:00"
time_rounding_minutes = 15
move_step_minutes = 15

[cache]
past_days = 365
future_days = 730
```

Set `backend = "mock"` for permanent demo mode. `service_path` can point to a non-adjacent native service. `TUI_CALENDAR_CONFIG`, `TUI_CALENDAR_SERVICE`, and `TUI_CALENDAR_DATA_DIR` are useful overrides for packaging and test isolation.

New events use the selected event time when one is active; otherwise they use the configured day default or a rounded current time. `D` duplicates an event into the editor while deliberately dropping recurrence and attendee state. Fast move/resize shortcuts are limited to non-recurring writable events, so a series is never changed silently.

The editor keeps existing native alarms intact when an unrelated field is saved. Basic relative alarms have a structured multi-reminder manager; custom, absolute, or provider-specific alarms are shown as **Custom / protected alarm** and are preserved read-only. Duplicating an event with protected alarms is blocked so those alarms are never silently discarded.

For all-day events, relative reminders are preserved but cannot be edited or created: EventKit represents their anchor as a floating date, so the current elapsed-seconds model cannot truthfully describe calendar-relative reminders. All-day absolute alarms are editable only when EventKit supplied an explicit event timezone; fallback or old-cache timezone context remains protected. Quick Add rejects `all-day` combined with `/alert:`.

All-day events fetched from current helpers also carry trusted, provider-normalized
calendar-date metadata. Read-side calendar membership uses that date-only range
when available; older cached events retain a compatibility fallback. Event
editing updates retain their explicit compatibility boundary, while structured
creation, Quick Add, and trusted all-day duplicates create events from
date-only ranges. A legacy all-day event without trusted date metadata cannot
be duplicated safely and prompts for a refresh instead of guessing from UTC.

Editing or deleting a recurring event first asks for an explicit scope: `1` for
this occurrence, `2` for this and future events, or `Esc` to cancel. Entire
series actions are not currently exposed.

Event selection is keyed by the provider's stable event ID, never by title or
time matching. After an authoritative refresh, a returned mutation identity is
preferred when it exists; otherwise the normal deterministic visible-list
fallback is used. A removed event closes its details view rather than showing
stale details for a fallback row.

The SQLite cache lives under the platform application-data directory (normally `~/Library/Application Support/tui-calendar/cache.sqlite3`). Calendar visibility preferences are retained across metadata refreshes. Deleting the cache only removes offline data and local visibility choices; EventKit calendars and events are unaffected.

### Cache recovery

At startup, the cache is migrated and checked for SQLite integrity. If SQLite
reports corruption, Terminal Calendar never deletes it: it moves
`cache.sqlite3` and any `cache.sqlite3-wal` / `cache.sqlite3-shm` companions
aside in the same directory as uniquely named `*.corrupt.<timestamp>` files,
then creates an empty cache. The app reports that calendar data will be
reloaded from EventKit. These quarantined files contain only local offline
data; inspect or remove them manually after confirming recovery. Filesystem
permission failures are not treated as corruption and remain explicit errors.
A cache written by a newer schema version is also left untouched rather than
quarantined; update Terminal Calendar before opening it.

Use `tui-calendar cache info` to inspect its non-private metadata, `tui-calendar cache vacuum` to compact it, and `tui-calendar cache prune` to transactionally remove events outside the configured retention window.

Successful EventKit fetches are tracked as half-open UTC intervals (`[start, end)`) independently of the number of returned events. This prevents empty, already-fetched periods from being requested repeatedly; overlapping or adjacent fetch coverage is compacted automatically.

Post-mutation reconciliation remains narrow for ordinary and **This
occurrence** changes. **This and future events** changes instead
authoritatively refresh the already-loaded cache coverage beginning at the
affected occurrence. EventKit remains the source of truth because it may split
a series and assign replacement occurrence IDs; unloaded cache gaps are never
filled merely because a future-series mutation occurred.

## Event editor syntax

Date/time fields use local time in `YYYY-MM-DD HH:MM` format.

Alerts use a structured manager for basic relative reminders. Focus **Alerts**, then use `a` to add a preset or custom `5m` / `2h` / `1d` duration, `j`/`k` to select an entry, `Enter` to edit it, and `d` to remove it. Existing absolute and custom provider alarms remain protected rather than being converted or rewritten.

Recurrence is mapped to native EventKit rules:

- `none`
- `daily:1`
- `weekly:1:MO,WE,FR`
- `weekly:2:TU`
- `monthly:1:2TU` (second Tuesday)
- `yearly:1`

See [the complete keyboard reference](docs/keybindings.md).

## Architecture

```text
┌──────────────────────────────┐        JSON lines        ┌───────────────────────────┐
│ Rust / Tokio / Ratatui       │ ◀──────────────────────▶ │ Swift EventKit service    │
│ views, commands, background  │   requests + changes     │ permissions, CRUD, native │
│ worker, provider-neutral DTO │                          │ recurrence and alarms     │
└──────────────┬───────────────┘                          └─────────────┬─────────────┘
               │                                                       │
               ▼                                                       ▼
       SQLite WAL cache                                     macOS Calendar database
       + preferences                                        + configured accounts
```

The UI never imports or links EventKit. One long-lived `EKEventStore` lives in the Swift process, and request IDs allow the Rust client to correlate asynchronous responses. The newline-delimited protocol is explicitly versioned (currently v2); mismatched helper binaries fail clearly. `EKEventStoreChanged` notifications trigger atomic cache refreshes. See [the IPC protocol](docs/ipc.md).

## Homebrew

Install the public `v1.0.0` release from the dedicated tap:

```sh
brew tap dmshvedchenko/tui-calendar
brew install tui-calendar
tui-calendar
```

The formula installs the Rust executable in `bin/` and the matching native
EventKit helper in `libexec/tui-calendar/`; `tui-calendar doctor` reports the
resolved helper path. If installation or launch fails, first run:

```sh
tui-calendar doctor --mock
```

Then review [Calendar permission](#calendar-permission),
[Cache recovery](#cache-recovery), and [IPC troubleshooting](docs/ipc.md#ipc-troubleshooting-and-recovery).
The bundled helper is native EventKit code and is installed beside the binary;
no source-tree helper path is required. The public formula lives in the
[Homebrew tap](https://github.com/dmshvedchenko/homebrew-tui-calendar).

For a local install without Homebrew:

```sh
make install PREFIX=/usr/local
```

## Security and privacy

- Calendar access stays inside the native EventKit service.
- The application never reads or stores account passwords, OAuth tokens, or calendar credentials.
- The IPC service accepts only local stdin and emits only local stdout; it opens no socket.
- Cached event content is stored unencrypted with the user's normal filesystem permissions. Protect the macOS account and disk with FileVault when calendar contents are sensitive.

## License

MIT
