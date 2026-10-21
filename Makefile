# Local entry points for the checks and publish path the workflows run.
#
#   make lint                                    Rust formatting and clippy
#   HEW_SOURCE=<hew checkout> make toolchain     build the pinned hew binary
#   make check                                   hew fmt and hew check, every source
#   make test                                    every package's Hew tests and examples
#   make test-native                             the same for NATIVE_PACKAGES
#   make test-crates                             the native crates' Rust tests
#   make publish-check                           dry-run the workspace publish
#   make publish                                 publish every new member version
#   make native-sqlite                           build a Linux x86_64 native archive
#
# `make test`, `make test-native` and `make test-crates` need the
# docker-compose.yml services: `docker compose up --detach --wait`. `make test-native` also needs the
# system libraries its packages link: ImageMagick 7 development files with
# pkg-config, and OpenSSL.

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

include toolchain.env

TOOLCHAIN_TARGET := $(CURDIR)/.tooling/target
HEW_BIN := $(TOOLCHAIN_TARGET)/release-lib/hew
# The compiler every Hew target runs; set HEW to use another build.
HEW ?= $(HEW_BIN)
# The MQTT test broker's host port. docker-compose.yml holds the default; set
# HEW_MQTT_PORT for both `docker compose up` and make to use another port.
HEW_MQTT_PORT ?= $(shell sed -n 's/.*HEW_MQTT_PORT:-\([0-9]*\)}.*/\1/p' docker-compose.yml)
export HEW_MQTT_PORT
# Where publish credentials come from: `credentials` (hew login and hew key
# generate) or `github-actions` (trusted publishing from publish.yml).
PUBLISH_AUTH ?= credentials
# Optional archives matched to their source packages by the publisher.
NATIVE_ARCHIVES ?=
# Archive construction is independent of registry authentication/publication.
NATIVE_SQLITE_ARCHIVE ?= $(CURDIR)/.tooling/native-packages/hew.db.sqlite.tar.zst

# Tracked Hew sources. The hew/ mirror is symlinks, so nothing under it is
# listed twice.
HEW_SOURCES = $(shell git ls-files '*.hew')
# Every package: each directory holding a hew.toml, outside the hew/ mirror.
PACKAGES := $(filter-out hew/%,$(patsubst %/hew.toml,%,$(wildcard */hew.toml */*/hew.toml)))
# Packages whose native crates link system libraries beyond the base image.
NATIVE_PACKAGES := image/magick storage/s3
# Packages that depend on another member. `hew install` links that member into
# the package's .hew/packages, following the tracked hew.lock.
LOCKED_PACKAGES := $(filter-out hew/%,$(patsubst %/hew.lock,%,$(wildcard */hew.lock */*/hew.lock)))

.PHONY: lint toolchain deps check test test-native test-crates publish-check publish native-sqlite

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings

deps:
	for package in $(LOCKED_PACKAGES); do \
		(cd "$$package" && "$(HEW)" install --locked); \
	done

# `hew check` takes one input, so every source is checked in turn and every
# failure is reported before the target fails.
check: deps
	"$(HEW)" fmt --check $(HEW_SOURCES)
	status=0; \
	for source in $(HEW_SOURCES); do \
		"$(HEW)" check --Werror "$$source" || status=1; \
	done; \
	exit "$$status"

# `hew test` finds every #[test] below each package: inline module tests,
# tests/, and the test in each example that runs it.
test: deps
	"$(HEW)" test $(filter-out $(NATIVE_PACKAGES),$(PACKAGES))

test-native: deps
	"$(HEW)" test $(NATIVE_PACKAGES)

test-crates:
	cargo test --locked --workspace --all-features

# Builds hew at the pinned revision from the checkout named by HEW_SOURCE.
# HEW_SOURCE must already be at HEW_REVISION: this target never moves someone
# else's checkout, it only refuses to build the wrong commit.
toolchain:
	$(if $(HEW_SOURCE),,$(error HEW_SOURCE is unset; point it at a hew checkout at $(HEW_REVISION)))
	git -C "$(HEW_SOURCE)" rev-parse --git-dir >/dev/null
	test "$$(git -C "$(HEW_SOURCE)" rev-parse HEAD)" = "$(HEW_REVISION)"
	@test -z "$$(git -C "$(HEW_SOURCE)" status --porcelain)" || { \
		echo "HEW_SOURCE has uncommitted changes; the built toolchain would not be $(HEW_REVISION)" >&2; \
		git -C "$(HEW_SOURCE)" status --short >&2; \
		exit 1; \
	}
	CARGO_TARGET_DIR="$(TOOLCHAIN_TARGET)" cargo build --locked --profile release-lib \
		-p hew-cli -p hew-lib --manifest-path "$(HEW_SOURCE)/Cargo.toml"
	test -x "$(HEW_BIN)"
	rm -rf "$(TOOLCHAIN_TARGET)/std"
	cp -a "$(HEW_SOURCE)/std" "$(TOOLCHAIN_TARGET)/std"
	@actual_version="$$("$(HEW_BIN)" --version)"; \
	if [ "$$actual_version" != "hew $(HEW_VERSION)" ]; then \
		printf 'toolchain version mismatch\nexpected: %s\nactual: %s\n' \
			"hew $(HEW_VERSION)" "$$actual_version" >&2; \
		exit 1; \
	fi
	@echo "pinned toolchain: $(HEW_BIN)"

# Validation, checking, packing and the registry query happen offline or
# read-only; nothing uploads.
publish-check:
	"$(HEW)" publish --workspace --dry-run $(foreach archive,$(NATIVE_ARCHIVES),--native-archive "$(archive)")

publish:
	"$(HEW)" publish --workspace --auth $(PUBLISH_AUTH) $(foreach archive,$(NATIVE_ARCHIVES),--native-archive "$(archive)")

# hew builds from the packed source and records that source checksum, the
# compiler/runtime identity and the target inside the archive.
native-sqlite:
	archive="$(NATIVE_SQLITE_ARCHIVE)"; \
	case "$$archive" in /*) ;; *) archive="$(CURDIR)/$$archive" ;; esac; \
	mkdir -p "$$(dirname "$$archive")"; \
	cd db/sqlite && "$(HEW)" native build \
		--target x86_64-unknown-linux-gnu --output "$$archive"
