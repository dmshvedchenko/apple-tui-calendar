# v1.0.4 manual acceptance

Use the Homebrew-installed binary in Ghostty. This checklist performs no
calendar mutations. Historical acceptance documents describe earlier releases.

- [ ] `tui-calendar --version` reports `tui-calendar 1.0.4`.
- [ ] Real `tui-calendar doctor` reports the bundled helper, IPC v2 and cache
  schema v3. Record the actual permission and health status; do not substitute
  mock validation for EventKit access.
- [ ] In Day, Week, Month, and Agenda, navigate away from today and press `t`.
  Each view returns to today's local date. Focus survives only if the same
  concrete occurrence remains visible.
- [ ] In each calendar view, press `.` and verify it does not trigger Today.
- [ ] Open Search and Quick Add, enter a lowercase `t`, and verify it is treated
  as text rather than as a navigation command.
- [ ] Quit with `q` and verify normal terminal input is restored.

Record binary version, installed Cellar/helper paths, doctor status, and which
checks were actually executed. Do not publish private calendar content.
