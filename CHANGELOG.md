# Changelog

All notable user-facing changes are documented here. This project follows
[Semantic Versioning](https://semver.org/).

## 1.0.0 — 2026-08-24

First public release of Terminal Calendar, a keyboard-first macOS calendar
client backed by Apple's EventKit.

### Installation

#### Homebrew (recommended)

```sh
brew tap dmshvedchenko/tui-calendar
brew install tui-calendar
tui-calendar
```

Terminal Calendar requires macOS Calendar permission on first launch. The
Homebrew formula bundles the native EventKit helper beside the executable.

### First run

1. Allow Calendar access when macOS prompts, or enable Terminal Calendar in
   **System Settings → Privacy & Security → Calendars**.
2. Verify the installation and its native helper:

   ```sh
   tui-calendar doctor
   ```

3. For an offline/mock-only diagnostic, run:

   ```sh
   tui-calendar doctor --mock
   ```

### Troubleshooting

- **Calendar permission:** enable Terminal Calendar in **System Settings →
  Privacy & Security → Calendars**, then run `tui-calendar doctor` again.
- **Helper or IPC issue:** `tui-calendar doctor` checks the executable, the
  bundled EventKit helper, IPC connectivity, and local cache state.
- **Cache recovery:** a corrupted cache is quarantined automatically and data
  is reloaded from EventKit. No macOS calendar events or calendars are
  deleted; only disposable local cache files are moved aside.

### Highlights

- Native EventKit integration for configured iCloud, Google, Exchange, CalDAV,
  local, subscribed, and birthday calendars.
- Day, rolling Week, Month, and Agenda views with keyboard-first navigation,
  command palette, search, details, and Calendar Manager workflows.
- Safe event create, edit, duplicate, delete, and move flows, including
  explicit This Event / Future Events handling for recurring events.
- Native recurrence representation, preserved unsupported recurrence data, and
  alarm editing that protects provider-specific or lossy alarm data.
- Trusted all-day calendar-date handling across create, update, duplicate,
  cache, search, recurrence, and drag/drop paths.
- Offline-first SQLite cache with schema migrations, integrity checking,
  corruption quarantine, and visible-range loading.
- Typed IPC v2 errors, helper crash/protocol recovery, and a reconnecting
  EventKit helper boundary.
- Provider-confirmed, bounded Undo/Redo for safe non-recurring mutations.
- Mouse/pointer drag/drop as another entry point to the existing safe movement
  workflow; no optimistic local mutation is used.

### Distribution

- Release builds package `tui-calendar` in `bin/` and the native
  `tui-calendar-service` helper in `libexec/tui-calendar/`.
- Homebrew distribution is available through
  `dmshvedchenko/tui-calendar`.
