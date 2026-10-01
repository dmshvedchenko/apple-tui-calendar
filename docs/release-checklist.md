# v1.0.5 final release gate

Versions are independent: application **1.0.5**, IPC protocol **v2**, cache
schema **v3**.

## Before publication

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `TZ=UTC cargo test --all-targets --locked` (includes IPC and PTY tests)
- [ ] `TZ=Europe/Berlin cargo test --all-targets --locked`
- [ ] `make swift-release` (Swift release build)
- [ ] `cargo build --release --locked`
- [ ] `target/release/tui-calendar --version` reports `tui-calendar 1.0.5`
- [ ] Run [manual acceptance](release-acceptance-v1.0.5.md).
- [ ] Run a disposable staged-install test: `bin/tui-calendar` plus `libexec/tui-calendar/tui-calendar-service`.
- [ ] Verify repository owner/URLs are `dmshvedchenko/apple-tui-calendar`.
- [ ] Confirm `git status --short` only contains intended release changes.

## Archive and Homebrew finalization (post-tag only)

1. Create and publish the final `v1.0.5` GitHub tag/release. The source archive
   used by the template is named `v1.0.5.tar.gz` and has this URL:

   ```sh
   https://github.com/dmshvedchenko/apple-tui-calendar/archive/refs/tags/v1.0.5.tar.gz
   ```

2. Download that exact archive and calculate its macOS checksum:

   ```sh
   curl -L -o v1.0.5.tar.gz \
     https://github.com/dmshvedchenko/apple-tui-calendar/archive/refs/tags/v1.0.5.tar.gz
   shasum -a 256 v1.0.5.tar.gz
   ```

3. Update the existing canonical formula in `dmshvedchenko/homebrew-tui-calendar`
   with the exact published archive checksum, URL and version test. Preserve
   its wrapper/helper and IPC tests; never publish the repository template's
   all-zero checksum. Do not carry a revision into a new upstream version.
   Run Ruby syntax, `brew style` and
   `brew audit --strict --formula dmshvedchenko/tui-calendar/tui-calendar`.

4. On a clean macOS environment: tap/install, verify `tui-calendar --version`,
   run real `tui-calendar doctor`, then launch the application and complete
   manual acceptance. Record permission restrictions accurately.

Validate the exact final commit before publishing. Verify v1.0.5 is absent
locally and remotely before creating its annotated tag. Published v1.0.0,
v1.0.1, v1.0.2, v1.0.3 and v1.0.4 are immutable; never move tags or force-push.
Release notes must contain only the v1.0.5 CHANGELOG section.
