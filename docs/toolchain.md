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
   checks below.

## The Rust dependency closure

`Cargo.lock` is tracked. The compiler is pinned to one revision, so the native
crates' dependency graph is pinned the same way rather than being resolved
afresh on every machine. Every `cargo` build, test and clippy run in the
Makefile, the docs and the workflows passes `--locked`, which fails when the
lockfile would have to change instead of silently changing it. Updating a
dependency is therefore a reviewed edit to `Cargo.lock`, not a side effect of
running a test.

## Compatibility

Every package's `hew.toml` declares its supported compiler range. Native
packages require Hew 0.6.2 because their Rust crates use its managed-string
ABI; update the compiler and `hew-cabi` pins together before publishing them.
Pure Hew packages retain their older minimum where their source remains
compatible. CI validates the exact pinned revision.

## Checks

```sh
# Build the pinned compiler from source.
. ./toolchain.env
hew_source="$(mktemp -d)"
git clone https://github.com/hew-lang/hew.git "$hew_source"
git -C "$hew_source" checkout "$HEW_REVISION"
HEW_SOURCE="$hew_source" make toolchain

make check

# Start the services the tests and examples connect to.
docker compose up --detach --wait
make test test-native

# Native crate tests.
make test-crates
```

The Makefile runs `.tooling/target/release-lib/hew`; set `HEW` to use another
build.

| Target | What it does | Needs |
| --- | --- | --- |
| `check` | `hew fmt --check` and `hew check --Werror` over every tracked `.hew` file | nothing |
| `test` | `hew test` over every package directory except `NATIVE_PACKAGES`: inline module tests, `tests/`, and the test in every example | the `docker-compose.yml` services |
| `test-native` | the same over `NATIVE_PACKAGES` (ImageMagick and S3) | the above, plus ImageMagick 7 development files, `pkg-config` and OpenSSL |
| `test-crates` | `cargo test` over every native crate, with the integration tests | the `docker-compose.yml` services and the `test-native` libraries |
| `publish-check` | `hew publish --workspace --dry-run` | read access to the registry |

The MQTT broker listens on the host port `HEW_MQTT_PORT` names, with its
default in `docker-compose.yml`. The Makefile reads that default and exports
`HEW_MQTT_PORT` to the tests, so set the variable for both
`docker compose up` and `make` to move the broker. Running `hew test
queue/mqtt` directly needs `HEW_MQTT_PORT` set.

Packages are found by their `hew.toml`, and `hew test` finds every `#[test]`
below them, so a new package, test file or example needs no list updated.
Before checking or testing, `make` runs `hew install --locked` in each package
that tracks a `hew.lock`, which links the members it depends on into its
`.hew/packages`.

CI also installs the system libraries, builds ImageMagick from a pinned source
archive, and runs `make lint`. Those steps live in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

`make toolchain` builds the pinned binary into `.tooling/` and refuses to build
if the checkout named by `HEW_SOURCE` is not at `HEW_REVISION` or is dirty.
