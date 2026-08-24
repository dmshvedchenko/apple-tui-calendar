PREFIX ?= /usr/local
CARGO ?= cargo
SWIFT ?= swift
SWIFT_BUILD_FLAGS ?= --disable-sandbox

.PHONY: build rust-release swift-release debug test fmt lint install uninstall clean

build: rust-release swift-release
	@mkdir -p target/release
	@cp macos-calendar-service/.build/release/tui-calendar-service target/release/tui-calendar-service

rust-release:
	$(CARGO) build --release --locked

swift-release:
	cd macos-calendar-service && $(SWIFT) build -c release $(SWIFT_BUILD_FLAGS)

debug:
	$(CARGO) build
	cd macos-calendar-service && $(SWIFT) build $(SWIFT_BUILD_FLAGS)

test:
	cd macos-calendar-service && $(SWIFT) build $(SWIFT_BUILD_FLAGS)
	TUI_CALENDAR_SERVICE="$(CURDIR)/macos-calendar-service/.build/debug/tui-calendar-service" $(CARGO) test --all-targets --locked

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) clippy --all-targets -- -D warnings

install: build
	install -d "$(PREFIX)/bin"
	install -d "$(PREFIX)/libexec/tui-calendar"
	install -m 755 target/release/tui-calendar "$(PREFIX)/bin/tui-calendar"
	install -m 755 target/release/tui-calendar-service "$(PREFIX)/libexec/tui-calendar/tui-calendar-service"

uninstall:
	@rm -f "$(PREFIX)/bin/tui-calendar" "$(PREFIX)/libexec/tui-calendar/tui-calendar-service"

clean:
	$(CARGO) clean
	cd macos-calendar-service && $(SWIFT) package clean
