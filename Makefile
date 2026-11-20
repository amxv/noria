SHELL := /bin/bash

CARGO ?= cargo
GO ?= go
DIST_DIR ?= dist
VERSION ?= $(shell cat VERSION)
EXE := $(if $(filter Windows_NT,$(OS)),.exe,)

.PHONY: help fmt fmt-check test test-race test-go-parity vet lint check docs-install docs-dev docs-check docs-build docs-preview build build-tray build-all release-native install-local clean release-tag

help:
	@echo "Noria development and release commands"
	@echo "  make fmt           - format Rust sources"
	@echo "  make check         - Rust formatting, tests, Clippy, installer/release checks"
	@echo "  make test-go-parity - run the retained Go reference tests with the race detector"
	@echo "  make build         - build the Rust CLI to dist/noria"
	@echo "  make build-tray    - build the Rust Windows/Linux tray (macOS uses Swift)"
	@echo "  make release-native - build this host's release binary and desktop archive"
	@echo "  make docs-check    - validate the separate Astro documentation workspace"
	@echo "  make release-tag VERSION=x.y.z - validate and push a release tag"

fmt:
	@$(CARGO) fmt --all

fmt-check:
	@$(CARGO) fmt --all -- --check

test:
	@$(CARGO) test --locked --all-targets --features desktop

test-go-parity:
	@$(GO) test -race ./...

# Compatibility alias while the Go implementation remains the parity oracle.
test-race: test-go-parity

vet:
	@$(CARGO) clippy --locked --all-targets --features desktop -- -D warnings

lint:
	@sh -n scripts/install.sh
	@diff -q scripts/install.sh docs/public/install.sh
	@diff -q scripts/install.ps1 docs/public/install.ps1
	@python3 -m unittest discover -s scripts -p 'test_*.py'

check: fmt-check test vet lint

docs-install:
	@cd docs && bun install --frozen-lockfile

docs-dev:
	@cd docs && bun run dev

docs-check:
	@cd docs && bun run test:deployment
	@cd docs && bun run test:design
	@cd docs && bun run check

docs-build:
	@cd docs && bun run build

docs-preview:
	@cd docs && bun run preview

build:
	@$(CARGO) build --locked --release --bin noria
	@mkdir -p "$(DIST_DIR)"
	@cp "target/release/noria$(EXE)" "$(DIST_DIR)/noria$(EXE)"

build-tray:
	@case "$$(uname -s)" in Darwin) echo "macOS uses Swift: apps/menubar/build.sh"; exit 1 ;; esac
	@$(CARGO) build --locked --release --features desktop --bin noria-tray
	@mkdir -p "$(DIST_DIR)"
	@cp "target/release/noria-tray$(EXE)" "$(DIST_DIR)/noria-tray$(EXE)"

release-native:
	@python3 scripts/build-release.py --output "$(DIST_DIR)"

# Native GUI signing and platform libraries cannot be validated by a single
# Linux cross-compile. The release workflow builds every target on its own OS.
build-all:
	@echo "All six targets are built by .github/workflows/release.yml on native runners."
	@echo "Use make release-native to build and validate this host's artifacts."
	@exit 1

install-local: build
	@mkdir -p "$$HOME/.local/bin"
	@install -m 755 "$(DIST_DIR)/noria$(EXE)" "$$HOME/.local/bin/noria$(EXE)"

clean:
	@rm -rf "$(DIST_DIR)"

release-tag:
	@test -n "$(VERSION)" || (echo "Usage: make release-tag VERSION=x.y.z" && exit 1)
	@python3 scripts/release.py check "v$(VERSION)"
	@git tag "v$(VERSION)"
	@git push origin "v$(VERSION)"
