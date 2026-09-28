# homebrew-tui-calendar

Official Homebrew tap for [Terminal Calendar](https://github.com/dmshvedchenko/apple-tui-calendar), a keyboard-first macOS EventKit calendar client.

## Installation

```sh
brew tap dmshvedchenko/tui-calendar
brew install tui-calendar
```

Run it with:

```sh
tui-calendar
```

## Troubleshooting

- **Calendar permission:** On first launch choose full Calendar access. If it
  was denied, enable Terminal Calendar in `System Settings > Privacy & Security
  > Calendars`.
- **Diagnostics:** Run `tui-calendar doctor --mock` to validate the installed
  binary, SQLite cache, and mock backend without accessing Calendar data. Run
  `tui-calendar doctor` to validate native helper discovery and EventKit access.
- **Cache recovery:** A corrupted local cache is moved aside as uniquely named
  `*.corrupt.<timestamp>` files and is rebuilt from EventKit. This never changes
  calendars or events; inspect or remove quarantined files manually.
- **Helper discovery:** Homebrew installs the native helper next to the binary
  under `libexec/tui-calendar/`. `tui-calendar doctor` prints the resolved path.
  Reinstall the formula if the helper is missing or protocol versions differ.

## Maintainer release update

Update the canonical tap's existing `Formula/tui-calendar.rb` with the v1.0.3
URL, version assertion, and published archive SHA-256. Preserve its wrapper,
helper and IPC tests; the application repository formula is a template only.
The immutable tag must exist before calculating the checksum:

```sh
curl -fL -o v1.0.3.tar.gz \
  https://github.com/dmshvedchenko/apple-tui-calendar/archive/refs/tags/v1.0.3.tar.gz
shasum -a 256 v1.0.3.tar.gz
brew tap dmshvedchenko/tui-calendar
brew audit --strict --formula dmshvedchenko/tui-calendar/tui-calendar
```
