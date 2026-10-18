# Local entry points for the checks and publish path the workflows run.
#
#   make lint                                    formatting, clippy, shellcheck
#   HEW_SOURCE=<hew checkout> make toolchain     build the pinned hew binary
#   make publish-local                           publish with that binary

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

include toolchain.env

TOOLCHAIN_TARGET := $(CURDIR)/.tooling/target
HEW_BIN := $(TOOLCHAIN_TARGET)/release-lib/hew

.PHONY: lint toolchain publish-local

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	shellcheck scripts/*.sh

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
	test "$$("$(HEW_BIN)" --version)" = "hew $(HEW_VERSION)"
	@echo "pinned toolchain: $(HEW_BIN)"

publish-local:
	@test -x "$(HEW_BIN)" || { \
		echo "no pinned toolchain at $(HEW_BIN); run: HEW_SOURCE=<hew checkout> make toolchain" >&2; \
		exit 1; \
	}
	HEW="$(HEW_BIN)" scripts/publish-packages.sh
