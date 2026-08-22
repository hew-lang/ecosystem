# Hew ecosystem

Hew is a statically typed language whose concurrency is built from actors:
state is owned by an actor, and callers reach it by `await`ing a `receive fn`.
This repository holds the official Hew packages — thin, typed wrappers over the
databases, brokers, and services a service usually needs, each one small enough
to read in a sitting.

## Packages

Every package publishes under its dotted name (`hew.math.stats`) and lives in
the directory that matches it.

| Package | Directory | What it gives you |
| --- | --- | --- |
| `hew.auth.oauth` | `auth/oauth` | OAuth 2.0 client: PKCE authorization code, state validation, client credentials, refresh |
| `hew.dag` | `dag` | Runtime DAG parsing, validation, topological sort, and dependency-ordered routing |
| `hew.db.mongodb` | `db/mongodb` | MongoDB client with typed errors and JSON document queries |
| `hew.db.mysql` | `db/mysql` | MySQL client with typed errors and query snapshots |
| `hew.db.postgres` | `db/postgres` | PostgreSQL client with typed errors and query snapshots |
| `hew.db.redis` | `db/redis` | Redis client with typed errors, explicit misses, pipelines, and bounded pub/sub |
| `hew.db.sqlite` | `db/sqlite` | SQLite client with typed errors and query snapshots |
| `hew.image.magick` | `image/magick` | ImageMagick 7 images with typed transformation and I/O errors |
| `hew.math.stats` | `math/stats` | Descriptive statistics, correlation, regression, streaming aggregates |
| `hew.metrics` | `metrics` | Prometheus metrics: typed counters, gauges, histograms, labels, export |
| `hew.net.http` | `net/http` | HTTP/1.1 server with typed errors and one-request handling |
| `hew.queue.mqtt` | `queue/mqtt` | MQTT 3.1.1 publish/subscribe client |
| `hew.queue.nats` | `queue/nats` | NATS pub/sub and request/reply client |
| `hew.storage.s3` | `storage/s3` | S3-compatible object storage with byte-exact values and typed errors |
| `hew.template` | `template` | Mustache-lite HTML templating with automatic escaping |

## Two minutes of Hew

`hew.math.stats` is pure Hew with no service behind it, so it is the shortest
thing here that runs. Clone this repository, make a directory beside the clone,
and write `main.hew` in it:

```sh
git clone https://github.com/hew-lang/ecosystem.git
mkdir hello && cd hello
```

```hew
import hew.math.stats;

fn main() {
    let values = [1.0, 2.0, 3.0, 4.0, 5.0];
    match stats.mean(values) {
        .Ok(mean) => println(f"mean = {mean}"),
        .Err(error) => println(stats.error_message(error)),
    }
}
```

Run it against the clone:

```sh
hew run --pkg-path ../ecosystem main.hew
```

```
mean = 3
```

`--pkg-path` points the compiler at the clone, where the `hew/` mirror tree
described under [Layout](#layout) resolves `import hew.math.stats` without
anything being installed. That is the route that works from a clone today.
Once these packages are published, `hew add hew.math.stats` followed by
`hew install` resolves the same import from the registry and `--pkg-path` is no
longer needed; the registry has no 0.3.0 to serve yet, and `hew add` against a
version the registry does not hold will resolve to whatever the local package
cache happens to contain instead.

Three things in that snippet carry across every package here. A fallible call
returns `Result<T, E>` with a package-specific error enum, never a sentinel
value. Variants are matched in dotted form (`.Ok`, `.Err`) because the
scrutinee's type already selects the enum. And every error enum has an
`error_message` that turns it into a printable string, so a match arm never has
to reach inside the variant to report a failure.

The service-backed packages add one wrapper: they own their connection in an
actor, so a call is `await conn.query(sql)` and its answer is
`Result<Result<T, E>, AskError>` — the outer `Result` reports whether the actor
answered at all, the inner one whether the operation succeeded.

```hew
let conn = spawn postgres.Conn(connstr: "host=127.0.0.1 user=hew password=hew dbname=hew_test");
match await conn.query("select 1") {
    .Ok(result) => match result {
        .Ok(query) => println(f"{query.rows.len()} rows"),
        .Err(error) => println(postgres.error_message(error)),
    },
    .Err(_) => println("connection actor stopped"),
}
conn.close();
```

Every example in this repository writes that pair the same way: one `match` per
`Result`, the inner one nested in the outer `.Ok` arm. Nesting the two patterns
into a single `.Ok(.Ok(...))` arm also compiles, but mixing the two forms across
a corpus teaches nothing, so the packages here pick one and keep it.

Those credentials are the ones `docker-compose.yml` starts PostgreSQL with, so
the snippet runs as written after `docker compose up -d postgres`.

Each package's `README.md` opens with a runnable example of exactly this shape,
and `examples/` holds the same program as a file you can run.

## Build, check, publish

```sh
make verify                                   # toolchain pins + package contract
HEW_SOURCE=<hew checkout> make toolchain      # build the pinned compiler
docker compose up -d                          # services the examples and tests need
hew check -A dead_code --pkg-path "$PWD" math/stats/examples/basic.hew
hew run --pkg-path "$PWD" math/stats/examples/basic.hew
hew test dag/dag.hew                          # pure-Hew suites
cargo test --locked --workspace               # native crates
make magick-example                           # the one example needing link flags
make publish-local                            # publish with the pinned compiler
```

`-A dead_code` is not optional decoration. Each file is checked on its own, so
without it the lint fires on every exported function whose only callers live in
a sibling example, test, or downstream program — 127 warnings across this
corpus. The alternative the compiler suggests, a `// hew:allow(dead_code)` line
per function, buries the same fact in 127 places instead of one.

`make verify` is the cheap gate to run first: it checks the toolchain pins and
the package metadata contract, the same two scripts CI runs before it builds
anything. The full corpus check CI performs, the compiler revision this
repository is pinned to, and how to move that pin are in [`docs/toolchain.md`](docs/toolchain.md);
package layout and the native-crate contract are in
[`docs/packaging.md`](docs/packaging.md).

`hew.image.magick` is the one package whose example needs more than `hew run`:
ImageMagick is a system library, and its link directives do not survive the trip
through the package's staticlib. `make magick-example` supplies the flags; see
[`image/magick/README.md`](image/magick/README.md).

## Layout

Packages live at their dotted path (`db/postgres`), and `hew/` mirrors each
top-level segment back with a symlink (`hew/db -> ../db`) so that
`import hew.db.postgres` resolves inside this repository without installing
anything. A new package needs both: its own directory and, for a new top-level
segment, a matching symlink under `hew/`.

## Documentation

The language itself is documented at
[hew-lang/hew](https://github.com/hew-lang/hew) — start with
`docs/hew-language-guide.md` there for the type system, actors, and ownership
rules the packages here assume you know.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. See [CONTRIBUTING.md](CONTRIBUTING.md) to add a package and
[SECURITY.md](SECURITY.md) to report a vulnerability.
