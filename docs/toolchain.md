# Toolchain pin

Every package in this repository is validated against one exact Hew compiler
build: `v0.6.0-rc2-dev.107+84c34d2bd`, at revision
`84c34d2bd895fdaeb4c78b1774715f8576335f34`. CI builds that revision from source
before it checks anything here, so a green run means the packages work against
that compiler and not merely against whatever the runner happened to have.

## Where the pin lives

`toolchain.env` is the only place the pins are written. It is sourced, never
executed, and each consumer reads the subset it needs:

| Variable | Read by | Purpose |
| --- | --- | --- |
| `HEW_REVISION` | CI, `make toolchain` | Compiler commit to build and check against |
| `HEW_VERSION` | CI, `make toolchain` | Version string the built binary must report |
| `LLVM_VERSION` | CI | LLVM toolchain the compiler build needs |
| `REGISTRY_API` | `scripts/publish-packages.sh` | Registry endpoint to publish to |
| `REGISTRY_AUDIENCE` | `scripts/publish-packages.sh` | OIDC audience for trusted publishing |

Three consumers cannot read `toolchain.env`: a workflow `uses:` ref and a Cargo
git `rev` must both be literals, and this document states the pin in prose.
`scripts/verify-pins.sh` asserts each of those copies against `toolchain.env`
rather than trusting them, so a repin that misses one fails closed instead of
shipping a split toolchain.

## Bumping the pin

1. Edit `toolchain.env` — `HEW_REVISION`, `HEW_VERSION`, and `LLVM_VERSION` if
   the compiler's LLVM requirement moved.
2. Update the literal copies: the `uses: hew-lang/hew/.github/actions/setup-llvm@<sha>`
   line in `.github/workflows/ci.yml` and `.github/workflows/publish.yml`, the
   `hew-cabi` `rev` in the workspace `Cargo.toml`, and the revision and version
   named at the top of this file.
3. Run `make verify`. It fails naming any copy you missed.
4. Rebuild and re-check: `HEW_SOURCE=<hew checkout> make toolchain`, then the
   corpus gate below.

Workflow filenames carry no version, so a repin never renames a file.

## The Rust dependency closure

`Cargo.lock` is tracked. The compiler is pinned to one revision, so the native
crates' dependency graph is pinned the same way rather than being resolved
afresh on every machine. Every `cargo` invocation in the gate, the Makefile and
the workflows passes `--locked`, which fails when the lockfile would have to
change instead of silently changing it. Updating a dependency is therefore a
reviewed edit to `Cargo.lock`, not a side effect of running a test.

## Compatibility

Every package declares:

```toml
hew = ">=0.6.0-rc1, <0.7.0"
```

The range accepts rc1 and later compatible 0.6 releases while excluding the
next breaking minor release. Native crates pin `hew-cabi` to the same exact
revision as the compiler, keeping their C ABI aligned with it.

## The corpus gate

`scripts/corpus-gate.sh` is the gate. CI runs that script and nothing else
Hew-side, so this document does not restate the command list — it would drift
from the workflow within a release.

```sh
# Build the pinned compiler from source.
git clone https://github.com/hew-lang/hew.git /tmp/hew
git -C /tmp/hew checkout 84c34d2bd895fdaeb4c78b1774715f8576335f34
cargo build --locked --profile release-lib -p hew-cli -p hew-lib --manifest-path /tmp/hew/Cargo.toml

# Start the services the suites and examples connect to.
docker compose up -d

# Run the gate.
HEW=/tmp/hew/target/release-lib/hew scripts/corpus-gate.sh all

# Native crate tests.
cargo test --locked --workspace
```

The script has three stages, and `all` runs them in order:

| Stage | What it does | Needs |
| --- | --- | --- |
| `check` | `hew check -A dead_code` over every tracked `.hew` file | nothing |
| `hew` | the pure-Hew suites, every package test suite, and every example, each one *run* rather than only checked | `docker compose up -d` |
| `native` | the ImageMagick and S3 suites and examples | the above, plus ImageMagick 7 development files and `pkg-config` |

Running the examples, not merely checking them, is deliberate: an example that
type-checks and then prints nothing, or that times out before a person can
reach it, is a defect the gate has to see. The metrics example is asserted on
its output and the HTTP example is sent a real request for that reason.

`-A dead_code` is on the sweep because every file is checked on its own, so the
lint would otherwise fire on every exported function whose callers live in a
sibling example, test, or downstream program.

CI additionally does what only a runner can: it builds the pinned compiler,
installs the system libraries, builds ImageMagick from a pinned source archive,
starts the service containers, and runs `cargo test` split across the
integration-gated crates. Those steps live in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

`make toolchain` builds the same binary into `.tooling/` and refuses to build
if the checkout named by `HEW_SOURCE` is not at `HEW_REVISION` or is dirty.
