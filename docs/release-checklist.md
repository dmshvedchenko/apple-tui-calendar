# v1.0.0 release checklist

This checklist releases Terminal Calendar without changing Calendar data or
requiring a real account mutation.

## Before release

- [ ] Confirm `git status --short` is empty in the real repository checkout.
- [ ] Confirm `Cargo.toml` and `Cargo.lock` report `1.0.0`.
- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo clippy --all-targets --all-features --locked -- -D warnings`.
- [ ] Run `cargo test --all-targets --locked`.
- [ ] Run `cargo build --release --locked`.
- [ ] Run `swift build -c debug` and `swift build -c release` from
  `macos-calendar-service/`.
- [ ] Run `target/release/tui-calendar doctor --mock`.
- [ ] Run the release binary from a disposable install prefix and verify that
  `doctor` reports its runtime-relative `libexec/tui-calendar/tui-calendar-service`.
- [ ] Review `CHANGELOG.md`, `README.md`, and `docs/ipc.md` for public-facing
  accuracy.

## Release

- [ ] Commit the release engineering changes.
- [ ] Create and push the signed `v1.0.0` tag.
- [ ] Wait for the GitHub release workflow to complete its Rust and Swift
  builds.
- [ ] Create the GitHub release using the `1.0.0` section of `CHANGELOG.md`.
- [ ] Download the GitHub source archive and calculate its SHA-256.
- [ ] Replace the Homebrew formula checksum placeholder with that exact value.
- [ ] Copy the finalized formula to the `homebrew-tui-calendar` tap repository
  and publish the tap update.

## After release

- [ ] `brew tap <OWNER>/tui-calendar && brew install tui-calendar` on a clean
  macOS environment.
- [ ] Run `tui-calendar doctor --mock` from the Homebrew installation.
- [ ] Run `tui-calendar doctor` and verify helper discovery without creating or
  editing a calendar.
- [ ] Verify first-launch Calendar permission instructions and denied-access
  offline behavior.
- [ ] Verify cache recovery by following the documented non-destructive
  quarantine procedure.
