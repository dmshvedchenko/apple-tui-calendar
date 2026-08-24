# Changelog

All notable user-facing changes are documented here. This project follows
[Semantic Versioning](https://semver.org/).

## 1.0.0 — 2026-08-24

First public release of Terminal Calendar, a keyboard-first macOS calendar
client backed by Apple's EventKit.

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
- The Homebrew formula and separate tap template are ready for checksum update
  once the `v1.0.0` GitHub release tag is published.
