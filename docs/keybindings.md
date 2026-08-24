# Keyboard reference

## Navigation

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Select next / previous event |
| `h` / `l`, `←` / `→` | Previous / next day, week, or month |
| `gg` | Jump to today |
| `gd` | Day view |
| `gw` | Week view |
| `gm` | Month view |
| `ga` | Agenda view |

### Month view

| Key | Action |
| --- | --- |
| `h` / `l`, `←` / `→` | Move the active date one day backward / forward |
| `j` / `k`, `↓` / `↑` | Move the active date one week forward / backward |
| `Tab` / `Shift-Tab` | Select next / previous event on the active date |
| `Enter` | Open details for that selected event |

## Events

| Key | Action |
| --- | --- |
| `n` | Create an event |
| `a` | Open Quick Add Event |
| `e` | Edit selected event |
| `d` | Delete selected event or recurring scope |
| `D` | Duplicate selected event into the editor (without recurrence/attendees) |
| `u` | Undo the last confirmed reversible event action |
| `Ctrl-R` | Redo the last successfully undone event action |
| `Alt-h` / `Alt-l` | Move the selected event one day backward / forward; recurring events request a scope |
| `Alt-j` / `Alt-k` | Move selected timed event later / earlier by configured step; recurring events request a scope |
| `J` / `K` | Extend / shorten selected non-recurring event by configured step |
| `Enter` | Open event details |
| `o` | Open the event URL or location in macOS |
| `Tab` / `Shift-Tab` | Next / previous field in the event editor |
| `Ctrl-S` | Save the event editor |

When the event editor focus is on a safely editable **Alerts** field:

| Key | Action |
| --- | --- |
| `a` | Open relative-alarm presets (including Custom) |
| `j` / `k`, `↓` / `↑` | Select an alarm or preset |
| `Enter` | Select a preset or edit the selected alarm |
| `d` | Delete the selected alarm |
| `Esc` | Cancel a preset/custom edit without changing alarms |

Structured alarm editing supports basic relative alarms only. Absolute and
provider-specific alarms are displayed as protected and remain untouched when
other event fields are saved.

For a recurring event, `e` and `d` first open a scope prompt: `1` edits or
deletes **This occurrence**, `2` applies to **This and future events**, and
`Esc` cancels. **Entire series** is intentionally not exposed yet.

## Calendars and system

| Key | Action |
| --- | --- |
| `c` | Toggle/focus the calendar sidebar |
| `gc` | Open the read-only Calendar Manager |
| `:` | Command palette |
| `Space` | Show / hide the selected calendar |
| `/` | Global search |
| `r` | Refresh from EventKit |
| `R` | Authoritatively refresh the visible range (retry a failed load) |
| `[` / `]` | Select previous / next calendar in the sidebar |
| `Space` (sidebar) | Toggle local calendar visibility |
| `?` | In-app keyboard reference |
| `q` | Quit or close a panel |
| `Esc` | Cancel / close a panel |

## Command palette

Press `:` to search and run common actions without leaving the keyboard:
create or Quick Add an event, edit/duplicate/delete the selected event, undo or
redo a confirmed reversible action, search,
go to today or a specific date, switch views, refresh, or open the calendar
sidebar. Event mutations reuse their normal editor, recurring-scope, and
confirmation flows. Entries that need a selected writable event remain visible
but disabled with a reason; `Enter` never bypasses those protections.

Overlays retain their caller context. `Esc` cancels a child overlay and returns
to its immediate parent—for example, Command Palette → Go to date → `Esc`
returns to the palette, and Event Details → Edit → `Esc` returns to the same
details view with its selection and scroll position intact. A dirty editor
still opens the existing discard prompt; `Esc` there returns to the editor.

## UI action dispatch

Keyboard shortcuts, Command Palette entries, and Event Details actions all
produce the same internal `UserAction` before entering existing workflows.
The dispatcher validates the stable event identity and then invokes the
established editor, recurrence-scope, delete-confirmation, search, or view
transition. Future mouse and context-menu controls must use this same boundary;
they do not call mutation workflows directly.

## Event movement internals

`UserAction::MoveEvent` carries either timed start/end instants or a trusted
all-day `[start_date, end_date_exclusive)` range. Timed moves must preserve the
original duration. All-day moves preserve their calendar-date span and use
`EventTimeMutation::ReplaceAllDay`; they never derive dates from elapsed UTC
seconds. Legacy all-day events without trusted date metadata are rejected until
they can be refreshed. Recurring moves use the existing This Event / Future
Events scope prompt.

## Calendar grid hit testing

Day, week, and month layouts have a pure screen-geometry contract for future
pointer controls. Hit testing maps a terminal coordinate to an existing event,
a local-calendar all-day date, a local-calendar timed slot, an empty month
cell, or outside the calendar. It performs no selection or mutation; any later
move or edit still enters through `UserAction` and the established permission,
recurrence-scope, and temporal-safety checks.

## Drag session internals

The future pointer adapter has three deliberately separate responsibilities:
hit testing answers **where** the pointer is; a `DragSession` records **what**
event is being previewed; and `UserAction::MoveEvent` answers **which safe
operation** should run. `DragSession` owns the renderable preview state. It
never sends a backend request. Its drop operation only returns an action after
rechecking that the event still exists, is writable, and has a compatible timed
or trusted all-day target.

## Pointer input boundary

Terminal-specific mouse and focus events are translated at the outer input
boundary into provider-neutral `PointerEvent` values before application state
sees them. The application therefore does not depend on Crossterm mouse types:

`terminal input → PointerEvent → application state`

Primary-button press, move, release, and cancel now drive only the local drag
session through calendar hit testing. Release is another source of the existing
`UserAction::MoveEvent` workflow, so it uses the same permission, recurring
scope, authoritative-refresh, and failure handling as keyboard movement.
There is no optimistic local move. The internal undo/redo foundation records
only provider-confirmed, non-recurring event mutations at this boundary; it
does not yet expose key bindings. Recurring series/split operations are left
out until EventKit can offer a safe reversible identity contract. Existing wheel navigation
remains mapped through the same neutral event model.

## Internal undo/redo foundation

`UserAction::Undo` and `UserAction::Redo` are dispatched by `u` (undo) and
`Ctrl-R` (redo). A mutation creates an `UndoRecord` only after the worker has
received a successful provider response; failed requests never enter history.

The first bounded history layer supports non-recurring creates, updates
(including typed timed and trusted all-day moves), and deletions where the
original event can be recreated without protected alarms. Redo is likewise
confirmed by the provider before the record returns to the undo stack.

Recurring `thisEvent`/`futureEvents` mutations are deliberately excluded for
now. EventKit may split a series and change identities without offering an
atomic revision/precondition suitable for safe reversal. All-day records use
their floating `[start_date, end_date_exclusive)` values, and protected alarms
use `AlarmMutation::Preserve` for reversible updates.

History is cache-backed and is rechecked against the latest authoritative
snapshot before issuing an inverse command. If an event was deleted externally
or its calendar became read-only, the inverse is not sent and its record stays
available for a later retry after refresh. EventKit offers no atomic
compare-and-save revision token, so an externally modified event can still be
changed by a later accepted inverse; the provider response and subsequent
authoritative refresh remain the source of truth. The application does not
claim conflict-free offline undo.

The Calendar Manager displays calendar visibility, color, source, stable ID,
per-calendar permissions, and helper capabilities. When the backend advertises
calendar creation support, `c` opens the Create Calendar form. When the
selected calendar permits metadata changes and the backend supports rename,
`e` opens the Rename Calendar form. `C` opens the Color form when metadata
changes are permitted. `d` opens a cancel-by-default destructive confirmation
only when the selected calendar explicitly permits deletion and the backend
advertises it.

| Key | Action |
| --- | --- |
| `c` (Calendar Manager) | Create calendar when supported by the backend |
| `e` (Calendar Manager) | Rename the selected calendar when permitted and supported |
| `C` (Calendar Manager) | Change the selected calendar color when permitted and supported |
| `d` (Calendar Manager) | Open destructive calendar deletion confirmation when permitted and supported |
| `Tab` / `Shift-Tab` | Next / previous Create Calendar field |
| `←` / `→` | Choose an explicit source by stable source ID |
| `Ctrl-S` | Submit Create Calendar |

Mouse-wheel scrolling is supported, but no feature requires a mouse.

## Recurrence editor structure

The structured event editor derives its visible recurrence fields from the
current rule: weekly rules add **Days**, while supported rules add **Ends**.
The **End date** and **Occurrences** rows appear only for their respective end
conditions. These rows are currently structural placeholders; editing their
values is intentionally introduced separately.

Structured recurrence is materialized through one validation boundary before
persistence: `RecurrenceEditorData → RecurrenceRule`. It preserves the base
frequency, interval, and weekly days while requiring a valid end date or a
positive occurrence count for the selected end condition.

## Quick Add

Quick Add is local, deterministic, and offline. Use `a` or the **Quick Add
Event** command-palette entry, inspect the preview, then press `Ctrl-S` to
save or `Ctrl-E` to open the existing structured editor. It supports `today` /
`tomorrow`, `heute` / `morgen`, English and German weekday names, `YYYY-MM-DD`
or `DD.MM.YYYY`, 24-hour times and ranges (`18:00-20:00`), `all-day`,
`#Calendar`, and `@Location`. Quoted control values such as
`#"Work Projects"` and `@"Munich Office"` are supported. This is a small
explicit grammar, not arbitrary natural-language parsing.

## Search filters

Search is offline-first: it searches cached events only and never triggers an
EventKit fetch while typing. Text matches title, notes, location, and attendee
metadata case-insensitively with Unicode-aware whitespace normalization.
Optional deterministic filters are `/calendar:<stable-id>`,
`/location:Munich`, `/attendee:Ada`, `/from:2026-09-01`, `/to:2026-09-30`,
`/all-day:true|false`, and `/recurring:true|false`. Quote values containing
spaces, for example `/attendee:"Ada Lovelace" planning`. Calendar filtering
uses the stable calendar ID rather than its display title. `Enter` jumps to the
cached event; `Shift-Enter` opens its details.
