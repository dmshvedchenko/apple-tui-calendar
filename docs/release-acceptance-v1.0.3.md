# v1.0.3 manual acceptance

Use the Homebrew-installed binary in Ghostty. This checklist performs no
calendar mutations. Historical acceptance documents describe earlier releases.

- [ ] `tui-calendar --version` reports `tui-calendar 1.0.3`.
- [ ] Real `tui-calendar doctor` reports the bundled helper, IPC v2 and cache
  schema v3. Record the actual permission and health status; do not substitute
  mock validation for EventKit access.
- [ ] Open Agenda with `ga`, navigate away from today, then press `.`. The date
  returns to today. Focus survives only if the same occurrence is still visible.
- [ ] With an existing multi-day timed event, inspect both overlapping dates:
  the start day shows its time and the next day shows `↳ cont.`. Simultaneous
  events remain present as independent rows.
- [ ] Select the continuing event and open Details. It remains the same
  underlying occurrence; cancel safely with Esc.
- [ ] Quit with `q` and verify normal terminal input is restored.

Record binary version, installed Cellar/helper paths, doctor status, and which
checks were actually executed. Do not publish private calendar content.
