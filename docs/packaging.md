# Packaging

## Layout

A package lives at the directory matching its dotted name: `hew.db.postgres`
is `db/postgres`, `hew.metrics` is `metrics`. Each package directory holds

```
hew.toml          package manifest (dotted name, version, compatibility range)
<segment>.hew     the module, named after the final path segment
README.md         one runnable example and the public surface
examples/         runnable programs, checked by CI like any other source
tests/            Hew test suites
src/, Cargo.toml  the native crate, when the package has one
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
