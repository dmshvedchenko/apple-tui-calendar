# v1.0.1 manual acceptance

Run this in Ghostty on macOS. Use `tui-calendar doctor` first; it must report
the helper path, **FullAccess**, and `Status: healthy`. Do not use important
calendar data for any item labelled **MUTATING**.

## Fast real-calendar smoke (safe)

```sh
tui-calendar doctor
tui-calendar
```

Inside the app: press `t`, then `gd`, `gw`, `gm`, and `ga`; search for a known
event with `/`; open it with `Enter`; return with `Esc`; manually scroll a Day
timeline with `PageDown`; open Help with `?`; open the command palette with
`:`; then quit with `q`. Confirm Ghostty accepts normal shell input afterward.

## Full checklist

### Startup and recovery (safe)

- [ ] Start with normal configuration; no panic; helper connects; calendars and events load.
- [ ] Start with an empty disposable data directory: `TUI_CALENDAR_DATA_DIR="$(mktemp -d)" tui-calendar`; allow access if prompted.
- [ ] With access denied/revoked, confirm cached data remains browsable and the footer/doctor gives the System Settings path; it must not say healthy.
- [ ] Quit during normal use and after a helper/reconnect error if safely reproducible; raw mode, cursor, alternate screen, and mouse capture are restored.

### Day and Week (safe)

- [ ] Day date/title matches the active date; all-day rows stay above timed appointments.
- [ ] Long-duration events use subdued rails; ordinary and overlapping appointments remain readable/selectable.
- [ ] One-row and compact events retain identifying labels; current-time marker is sensible.
- [ ] `h`/`l` move calendar days; Day `j`/`k` selects events; Week `j`/`k` moves weeks.
- [ ] Manual Day/Week scrolling does not snap back after refresh; automatic positioning works after date navigation.
- [ ] Week shows seven correct dates; labels do not cross another day column.

### Month and Agenda (safe)

- [ ] Month has a stable six-week grid, clear today/active date, timed `HH:MM Title` and all-day `• Title` rows.
- [ ] A dense day shows deterministic `+N more`, not clipped or missing labels.
- [ ] `Tab` selects a visible Month event. A hidden-overflow event reached by search/details still opens on its correct date in Day/Week.
- [ ] Agenda is chronological, scrollable, and opens the selected event with `Enter`.

### Cross-view selection (safe)

- [ ] Verify `Day → Week → Day`, `Day → Month → Day`, and `Day → Agenda → Day` preserve the same concrete occurrence when it is representable.
- [ ] Search for an occurrence, activate it, then switch views; the same occurrence ID remains selected rather than a positional neighbour.

### Details, sidebar, Help, palette, mouse, and resize (safe)

- [ ] Details opens the visibly selected event; `Esc` returns to the same context. Check URL/location actions only on a suitable event.
- [ ] Sidebar: `c`, `j`/`k`, `Space`, and `Esc`; verify read-only indication and local visibility only.
- [ ] Help: `?`, scroll with `j`/`k`, `PageUp`/`PageDown`, then close. Palette `:` key hints match actual shortcuts and disabled actions cannot run.
- [ ] Mouse: click a Day/Week/Month event, long rail, and a short foreground overlap; the topmost visible event wins. Click outside calendar geometry safely.
- [ ] Resize Ghostty near 160, 120, 100, and 80 columns; no panic, stale glyphs, or out-of-bounds output.

### Isolated mutation tests (**MUTATING**)

Create an expendable writable calendar named `Terminal Calendar RC Test`, then
create only test events there (for example prefix every title with `RC TEST`).
Never edit/delete an arbitrary existing event.

- [ ] Create, edit, duplicate, and delete an `RC TEST` timed event; cancel an editor and verify dirty-discard confirmation.
- [ ] Create an `RC TEST` recurring event; test **This Event** edit/delete and **Future Events** edit; inspect neighbouring occurrences and selection restoration.
- [ ] If a harmless failure can be reproduced (for example temporarily revoke access), confirm an editor keeps its entered fields and does not claim success.
- [ ] Delete the temporary event/series and the temporary calendar only after all checks pass.

## Release evidence to attach

Record macOS version, Ghostty version, `tui-calendar --version`, `doctor`
output (without event content), test calendar used, and any failed checklist
item. Do not include event notes, attendees, or titles in public reports.
