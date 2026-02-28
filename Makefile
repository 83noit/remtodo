.PHONY: all build build-release swift-build rust-build rust-build-release \
        test rust-test smoke-test lint fmt install clean

# Run make targets from within `nix develop` or a rustup environment.

# Default: release build of both binaries
all: build-release

# ── Build ────────────────────────────────────────────────────────────────────

build: rust-build

build-release: swift-build rust-build-release

# Swift CLI — MUST be run outside nix develop (uses system toolchain)
swift-build:
	cd swift && swift build -c release

rust-build:
	cargo build

rust-build-release:
	cargo build --release

# ── Install ───────────────────────────────────────────────────────────────────

# Copy release binaries to ~/.local/bin.
# Both binaries land as siblings so remtodo finds reminders-helper
# without relying on CWD or REMINDERS_HELPER env var.
install: rust-build-release
	@test -f swift/.build/release/reminders-helper || \
		(echo "Error: Swift binary not found. Run outside nix develop first:" && \
		 echo "  cd swift && swift build -c release" && exit 1)
	mkdir -p ~/.local/bin
	cp target/release/remtodo ~/.local/bin/remtodo
	cp swift/.build/release/reminders-helper ~/.local/bin/reminders-helper
	@echo "Installed remtodo and reminders-helper to ~/.local/bin"

# ── Smoke test ───────────────────────────────────────────────────────────────

smoke-test: build-release
	REMINDERS_HELPER=swift/.build/release/reminders-helper \
	RUST_LOG=info \
	./target/release/remtodo sync --dry-run

# ── Test ─────────────────────────────────────────────────────────────────────

test: rust-test

rust-test:
	cargo test

# ── Lint / Format ─────────────────────────────────────────────────────────────

lint:
	cargo clippy -- -D warnings
	cargo fmt --check

fmt:
	cargo fmt

# ── Clean ─────────────────────────────────────────────────────────────────────

clean:
	cargo clean
	rm -rf swift/.build
