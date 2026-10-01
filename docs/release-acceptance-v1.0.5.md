# v1.0.5 manual acceptance

Use the Homebrew-installed binary in Ghostty. This checklist performs no
calendar mutations. Historical acceptance documents describe earlier releases.

- [ ] `tui-calendar --version` reports `tui-calendar 1.0.5`.
- [ ] Real `tui-calendar doctor` reports the bundled helper, IPC v2 and cache
  schema v3. Record actual Calendar permission and health status.
- [ ] In Agenda, navigate significantly into the future and select an event.
  Press `t`: the first visible section is anchored at today, and the selected
  future occurrence cannot scroll the list away from today. Its focus clears.
- [ ] Select a Today occurrence visible within the first viewport and press
  `t`: Today remains visible, and the same occurrence stays selected.
- [ ] After Today, use `j`/`k`: ordinary event selection and scrolling resume.
- [ ] In Day, Week and Month, navigate away and press `t`: return to today.
- [ ] In Agenda, `.` does not trigger Today.
- [ ] Search and Quick Add accept literal lowercase `t` as text. Cancel each
  safely with Esc without saving.
- [ ] Existing multi-day events retain their `↳ cont.` rows and concrete
  occurrence identity across overlapping dates.
- [ ] Quit with `q` and verify normal terminal input is restored.

Record version, installed Cellar/helper paths, doctor status, and which checks
were actually executed. Do not publish private calendar event names.
