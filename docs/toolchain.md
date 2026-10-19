# Toolchain pin

Every package in this repository is validated against one exact Hew compiler
build, pinned in [`toolchain.env`](../toolchain.env) together with the LLVM
release; that file's header says what each pin means.
CI builds the pinned revision from source before it checks anything here, so a
green run means the packages work against that compiler and not merely against
whatever the runner happened to have.

## Bumping the pin

1. Edit `toolchain.env`: `HEW_REVISION`, `HEW_VERSION`, and `LLVM_VERSION` if
   the compiler's LLVM requirement moved.
2. Set the `hew-cabi` `rev` in the workspace `Cargo.toml` to the new
   `HEW_REVISION`, then run `cargo update --package hew-cabi`.
3. Rebuild and re-check: `HEW_SOURCE=<hew checkout> make toolchain`, then the
   corpus gate below.

## The Rust dependency closure

`Cargo.lock` is tracked. The compiler is pinned to one revision, so the native
crates' dependency graph is pinned the same way rather than being resolved
afresh on every machine. Every `cargo` build, test and clippy run in the
Makefile, the docs and the workflows passes `--locked`, which fails when the
lockfile would have to change instead of silently changing it. Updating a
dependency is therefore a reviewed edit to `Cargo.lock`, not a side effect of
running a test.

## Compatibility

Every package's `hew.toml` declares a `hew` compatibility range that starts at
the pinned compiler's version and excludes the next breaking minor release. CI
validates the exact pinned revision.

## The corpus gate

`scripts/corpus-gate.sh` is the corpus gate.

```sh
# Build the pinned compiler from source.
. ./toolchain.env
git clone https://github.com/hew-lang/hew.git /tmp/hew
git -C /tmp/hew checkout "$HEW_REVISION"
HEW_SOURCE=/tmp/hew make toolchain

# Start the services the suites and examples connect to.
docker compose up --detach --wait

# Run the gate.
HEW="$PWD/.tooling/target/release-lib/hew" scripts/corpus-gate.sh all

# Native crate tests.
cargo test --locked --workspace --all-features
```

The script has three stages, and `all` runs them in order:

| Stage | What it does | Needs |
| --- | --- | --- |
| `check` | `hew fmt --check` and `hew check -A dead_code` over every tracked `.hew` file | nothing |
| `hew` | the pure-Hew suites, every package test suite, and every example, each one *run* rather than only checked | the `docker-compose.yml` services |
| `native` | the ImageMagick and S3 suites and examples | the above, plus ImageMagick 7 development files and `pkg-config` |

CI also installs the system libraries, builds ImageMagick from a pinned source
archive, and runs `make lint`. Those steps live in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

`make toolchain` builds the pinned binary into `.tooling/` and refuses to build
if the checkout named by `HEW_SOURCE` is not at `HEW_REVISION` or is dirty.
