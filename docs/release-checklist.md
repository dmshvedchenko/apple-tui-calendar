# v1.0.2 final release gate

Versions are independent: application **1.0.2**, IPC protocol **v2**, cache
schema **v3**.

## Before publication

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo test --all-targets --locked` (includes IPC and PTY tests)
- [ ] `make debug` (Swift debug build)
- [ ] `make swift-release` (Swift release build)
- [ ] `cargo build --release --locked`
- [ ] `target/release/tui-calendar --version` reports `tui-calendar 1.0.2`
- [ ] Run [manual acceptance](release-acceptance-v1.0.2.md).
- [ ] Run a disposable staged-install test: `bin/tui-calendar` plus `libexec/tui-calendar/tui-calendar-service`.
- [ ] Verify repository owner/URLs are `dmshvedchenko/apple-tui-calendar`.
- [ ] Confirm `git status --short` only contains intended release changes.

## Archive and Homebrew finalization (post-tag only)

1. Create and publish the final `v1.0.2` GitHub tag/release. The source archive
   used by the template is named `v1.0.2.tar.gz` and has this URL:

   ```sh
   https://github.com/dmshvedchenko/apple-tui-calendar/archive/refs/tags/v1.0.2.tar.gz
   ```

2. Download that exact archive and calculate its macOS checksum:

   ```sh
   curl -L -o v1.0.2.tar.gz \
     https://github.com/dmshvedchenko/apple-tui-calendar/archive/refs/tags/v1.0.2.tar.gz
   shasum -a 256 v1.0.2.tar.gz
   ```

3. Replace the all-zero `sha256` placeholder in
   `docs/homebrew-tap/Formula/tui-calendar.rb`, copy the finalized formula to
   `dmshvedchenko/homebrew-tui-calendar`, then run `brew style` and
   `brew audit --strict --formula dmshvedchenko/tui-calendar/tui-calendar`.

4. On a clean macOS environment: tap/install, verify `tui-calendar --version`,
   run `tui-calendar doctor`, then launch the application. Do not publish/push
   this repository or the tap as part of the release-candidate preparation.
