# Packaging

## Layout

A package lives at the directory matching its dotted name: `hew.db.postgres`
is `db/postgres`, `hew.metrics` is `metrics`. Each package directory holds

```
hew.toml          package manifest (dotted name, version, compatibility range)
<segment>.hew     the module, named after the final path segment
README.md         one runnable example and the public surface
examples/         runnable programs, each with a #[test] that runs or checks it
tests/            #[test] suites
src/, Cargo.toml  the native crate, when the package has one
hew.lock          when the package depends on another member
```

`hew/` mirrors each top-level segment back with a symlink (`hew/db -> ../db`),
which is what lets `import hew.db.postgres` resolve inside this repository
without installing anything. **A new top-level segment needs a matching symlink
under `hew/`**, or every consumer of the new package fails to resolve it.

## Native crates

A package that wraps a Rust library keeps the crate in the package directory
itself — `src/lib.rs` beside `Cargo.toml` — and names it in `hew.toml`:

```toml
[native]
crate = "."
```

`Cargo.toml` alone names the library and its `staticlib` crate type; `hew`
reads both from Cargo. Every package spells `crate` as `"."`: one crate per
package, in one place, never a second manifest for the same sources.

The crate is a member of the workspace `Cargo.toml`, inherits its version from
`[workspace.package]`, exposes a C ABI over `hew-cabi` pinned at the same
revision as the compiler (see [`toolchain.md`](toolchain.md)), and owns its
resources behind opaque integer handles rather than raw pointers. Closing a
handle invalidates it: a later call against a closed handle returns a typed
error, it does not use freed memory.

## Package workflow

The [getting-started example](../README.md#two-minutes-of-hew) shows both a
clone and a registry-backed project.

```sh
hew add hew.math.stats    # add a published package
hew install               # resolve and fetch
```

Within a package directory, `hew build` builds a declared `[native]` crate,
`hew check` validates a manifest or a source file, and `hew test` runs Hew
tests.

## Publishing

The root [`hew.toml`](../hew.toml) lists every package as a workspace member.
Inside the workspace a member's requirement on another member, such as
`"hew.db.sql" = "0.4.0"`, resolves to that member's directory, so packages
bumped together are checked together before anything is on the registry.

`make publish-check` runs `hew publish --workspace --dry-run`: it validates
every member, checks and packs each one in dependency order, and asks the
registry which versions it already holds. `make publish` does the same and
then uploads, signs and reads back each new version. A version the registry
already holds with the same tree is skipped; a version it holds with a
different tree fails with `E_PUBLISH_VERSION_DRIFT`, so a package that changes
needs a new version. The publish workflow runs `make publish` with trusted
publishing (`--auth github-actions`) on every push to `main`; a local
`make publish` uses the credentials from `hew login` and `hew key generate`.

## Consuming local native sources

To build an application against an unpublished ecosystem checkout, put both
under a virtual Hew workspace. List the application and the packages it uses,
including transitive members such as `hew.db.sql`:

```text
workspace/
  hew.toml
  ecosystem/       # ecosystem checkout with matching compiler/C ABI pins
  applications/   # application sources and their package manifest
```

For SQLite, the root `workspace/hew.toml` is:

```toml
[workspace]
members = ["ecosystem/db/sql", "ecosystem/db/sqlite", "applications"]
```

The application keeps ordinary version requirements:

```toml
[dependencies]
"hew.db.sql" = "0.4.0"
"hew.db.sqlite" = "0.4.0"
```

From `applications/`, run `hew install --offline`, then
`hew install --offline --locked` to check the resulting lock. `hew build
main.hew` builds the native crate from the workspace member. Cargo and rustc
must match the compiler/runtime toolchain, and the ecosystem's `hew-cabi` pin
must match that compiler. No prebuilt archive or remote publication is needed.
Add other package directories to the same workspace as needed.

`--pkg-path` changes module lookup; it does not change a dependency's locked
registry identity. Using it to shadow a registry dependency with different
local source correctly fails checksum validation. A standalone path dependency
also does not override another package's registry requirement on that name;
the workspace supplies one source for the entire dependency graph.

The current `hew publish --local` native consumer path is incomplete: its
local cache entry has no verified remote source provenance, while native
registry resolution requires that provenance before choosing source fallback.
Use workspace members for local native development; do not add provenance
metadata by hand or weaken the registry integrity check.

## SQLite native archive

After updating both compiler and `hew-cabi` pins to the same released Hew
0.6.2 revision, `make native-sqlite` builds the Linux x86_64 SQLite archive at
`.tooling/native-packages/hew.db.sqlite.tar.zst`. Set `NATIVE_SQLITE_ARCHIVE`
to choose another output path. The existing `hew native build` command packs
the source, builds its Rust static library and records the source checksum,
target and compiler/runtime identity in the archive. Cargo and rustc are
required on the build machine.

This target only constructs an archive. The workspace publisher currently
publishes source packages; it cannot include native archives in its publish
plan or share its trusted-publishing grant with `hew native publish`. Native
publication remains pending that CLI support and registry native routes.
The initial consumer qualification scope is Linux x86_64 SQLite: O0/O2 queries
without Cargo or rustc, locked/offline installation, source fallback, integrity
and identity refusal, and repeated publication with matching source checksums.
An archive built with a development SDK or an older `hew-cabi` pin does not
qualify the released 0.6.2 packages.
