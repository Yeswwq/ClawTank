# ClawTank build entry points. See README.md → "Building".
SHELL := /bin/bash
# Prefer the official rustup-managed toolchain when it is installed, even when
# make is launched from a non-login shell that has not sourced ~/.cargo/env.
RUSTUP_BIN := $(HOME)/.cargo/bin
ifneq ($(wildcard $(RUSTUP_BIN)/cargo),)
export PATH := $(RUSTUP_BIN):$(PATH)
endif
ARCH  ?= $(shell uname -m)
CONFIG ?= Release
ifeq ($(ARCH),x86_64)
RUST_TARGET ?= x86_64-apple-darwin
else ifeq ($(ARCH),arm64)
RUST_TARGET ?= aarch64-apple-darwin
else
$(error ARCH must be arm64 or x86_64, got '$(ARCH)')
endif
DERIVED := app/build
APP := $(DERIVED)/Build/Products/$(CONFIG)/ClawTank.app
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' core/Cargo.toml | head -1)

.PHONY: build run full-run core app project test test-core test-app lint audit package release clean spike-vd dev-signing bench

build: app ## Build Rust core + macOS app

run: build ## Build, clear the previous capture, stream unified logs and launch the new app
	scripts/run-local.sh "$(APP)" "/Applications/ClawTank.app"

full-run: ## Run lint, all tests and Release build, then replace /Applications app and monitor it
	$(MAKE) lint ARCH=$(ARCH)
	$(MAKE) test ARCH=$(ARCH)
	$(MAKE) build ARCH=$(ARCH)
	scripts/run-local.sh "$(APP)" "/Applications/ClawTank.app"

core: ## Build the Rust static library and regenerate the C header
	cd core && cargo build --release --target "$(RUST_TARGET)"

project: ## Generate the Xcode project from app/project.yml
	cd app && xcodegen generate --quiet

app: core project
	xcodebuild -project app/ClawTank.xcodeproj -scheme ClawTank -configuration $(CONFIG) \
		-derivedDataPath $(DERIVED) -destination "platform=macOS,arch=$(ARCH)" ARCHS=$(ARCH) RUST_TARGET=$(RUST_TARGET) -quiet build
	@if [ -z "$(RELEASE)" ] && [ -f "$$HOME/Library/Keychains/yeswwq.keychain-db" ]; then \
		scripts/dev-signing.sh sign "$(APP)"; \
	elif [ -z "$(RELEASE)" ]; then echo "note: ad-hoc signed (run 'make dev-signing' to keep permissions across rebuilds)"; fi
	@echo "Built $(APP)"

dev-signing: ## One-time: create the local "yeswwq" signing identity (ADR-015)
	scripts/dev-signing.sh setup

test: test-core test-app ## Run all unit tests

test-core:
	cd core && cargo test --target "$(RUST_TARGET)"
	cd core && cargo test -p ironrdp-server --target "$(RUST_TARGET)" rdp6_wire_header_uses_byte_scan_width_for_full_and_edge_tiles

# Each run replaces one result bundle; Xcode would otherwise keep every run under Logs/ (GBs).
RESULTS := $(DERIVED)/TestResults.xcresult

test-app: core project
	rm -rf $(RESULTS) $(DERIVED)/Logs/Test
	xcodebuild -resultBundlePath $(RESULTS) -project app/ClawTank.xcodeproj -scheme ClawTank -configuration Debug \
		-derivedDataPath $(DERIVED) -destination "platform=macOS,arch=$(ARCH)" ARCHS=$(ARCH) RUST_TARGET=$(RUST_TARGET) -quiet test
	@if [ -z "$(RELEASE)" ] && [ -f "$$HOME/Library/Keychains/yeswwq.keychain-db" ]; then \
		scripts/dev-signing.sh sign "$(DERIVED)/Build/Products/Debug/ClawTank.app"; \
	else echo "note: test app remains ad-hoc signed (run 'make dev-signing' to preserve TCC grants)"; fi

bench: core project ## Encoder benchmark (2560x1600 @ 60 fps): latency, bitrate, fps
	rm -rf $(RESULTS) $(DERIVED)/Logs/Test
	CLAWTANK_BENCH=1 xcodebuild -resultBundlePath $(RESULTS) -project app/ClawTank.xcodeproj -scheme ClawTank -configuration Debug \
		-derivedDataPath $(DERIVED) -destination "platform=macOS,arch=$(ARCH)" ARCHS=$(ARCH) RUST_TARGET=$(RUST_TARGET) \
		-only-testing:ClawTankTests/EncoderBenchmarkTests test 2>&1 | grep -E "BENCH|error|TEST (SUCCEEDED|FAILED)"

lint: ## clippy, rustfmt, swiftlint
	cd core && cargo clippy --no-deps -p clawtank-core -p clawtank-mcp --all-targets --target "$(RUST_TARGET)" -- -D warnings
	cd core && cargo fmt --check -p clawtank-core -p clawtank-mcp
	swiftlint lint --strict --quiet

audit: ## RustSec advisory check
	cd core && cargo audit

package: build ## Unsigned (ad-hoc) DMG in dist/
	scripts/package-dmg.sh "$(APP)" "dist/ClawTank-$(VERSION).dmg"

# Release builds are ad-hoc signed (never with the local dev identity) unless Developer ID
# credentials are set; see scripts/release.sh and docs/releasing.md.
# Built in its own folder so it never replaces the dev-signed app that holds your TCC grants.
release: ## Versioned DMG + SHA256SUMS + release notes in dist/
	$(MAKE) app RELEASE=1 DERIVED=build/release
	scripts/release.sh "$(VERSION)" "build/release/Build/Products/$(CONFIG)/ClawTank.app"

spike-vd: ## Phase 0 virtual-display spike (needs Screen Recording permission)
	mkdir -p build && swiftc -O -import-objc-header spikes/virtual-display/CGVirtualDisplay.h \
		-framework CoreGraphics -framework ScreenCaptureKit spikes/virtual-display/main.swift -o build/vd-spike
	./build/vd-spike build/virtual-display.png

clean:
	cd core && cargo clean
	rm -rf $(DERIVED) app/ClawTank.xcodeproj dist build
