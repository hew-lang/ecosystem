# Local driver for the ecosystem publish path. Every target here executes the
# same scripts the publish workflow executes, against the same pins, so a
# publish run is reproducible off a runner.
#
#   make verify                                  contract check only
#   HEW_SOURCE=<hew checkout> make toolchain     build the pinned hew binary
#   make publish-local                           publish with that binary
#   make magick-example                          run the ImageMagick example

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

# Pins come from toolchain.env and nowhere else.
include toolchain.env
export HEW_REVISION HEW_VERSION LLVM_VERSION REGISTRY_API REGISTRY_AUDIENCE

TOOLCHAIN_TARGET := $(CURDIR)/.tooling/target
HEW_BIN := $(TOOLCHAIN_TARGET)/release-lib/hew

# The compiler the packages are checked with. Override to use a build other
# than the one on PATH: `HEW=.tooling/target/release-lib/hew make magick-example`.
HEW ?= hew

.PHONY: verify toolchain publish-local magick-example

verify:
	scripts/verify-pins.sh
	python3 scripts/verify_package_contract.py
	cargo clippy --locked --workspace --all-targets -- -D warnings

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
	test "$$("$(HEW_BIN)" --version)" = "hew $(HEW_VERSION)"
	@echo "pinned toolchain: $(HEW_BIN)"

publish-local:
	@test -x "$(HEW_BIN)" || { \
		echo "no pinned toolchain at $(HEW_BIN); run: HEW_SOURCE=<hew checkout> make toolchain" >&2; \
		exit 1; \
	}
	HEW="$(HEW_BIN)" scripts/publish-packages.sh

# ImageMagick is a system dependency, and magick_rust's link directives do not
# survive the trip through this package's staticlib, so the MagickWand
# libraries have to be named on the link line. pkg-config knows where they are.
# This target exists because that incantation is otherwise only written down
# inside the CI workflow.
magick-example:
	@pkg-config --exists MagickWand || { \
		echo "ImageMagick 7 development files not found; see image/magick/README.md" >&2; \
		exit 1; \
	}
	link_args=(); \
	for library in $$(pkg-config --libs MagickWand); do \
		link_args+=(--link-lib "$$library"); \
	done; \
	"$(HEW)" run --pkg-path "$(CURDIR)" "$${link_args[@]}" \
		image/magick/examples/basic.hew
