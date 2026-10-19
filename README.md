# Hew ecosystem

Hew is a statically typed language whose concurrency is built from actors:
state is owned by an actor, and callers reach it through a `receive fn` call.
This repository holds the official Hew packages — thin, typed wrappers over the
databases, brokers, and services a service usually needs, each one small enough
to read in a sitting.

## Packages

Every package uses a dotted name (`hew.math.stats`) and lives in the directory
that matches it. The current source release is `0.4.0`, compatible with Hew
0.6.1. Check a package's published versions with `hew info <PACKAGE>`.

| Package | Directory | What it gives you |
| --- | --- | --- |
| `hew.auth.oauth` | `auth/oauth` | OAuth 2.0 client: PKCE authorization code, state validation, client credentials, refresh |
| `hew.dag` | `dag` | Runtime DAG parsing, validation, topological sort, and dependency-ordered routing |
| `hew.db.mongodb` | `db/mongodb` | MongoDB client with typed errors and JSON document queries |
| `hew.db.mysql` | `db/mysql` | MySQL client with typed errors and query snapshots |
| `hew.db.postgres` | `db/postgres` | PostgreSQL client with typed errors and query snapshots |
| `hew.db.redis` | `db/redis` | Redis client with typed errors, explicit misses, pipelines, and bounded pub/sub |
| `hew.db.sql` | `db/sql` | Typed SQL parameters shared by SQLite, MySQL and PostgreSQL |
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

Use the compiler pinned in the [toolchain guide](docs/toolchain.md).

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
described in [packaging](docs/packaging.md#layout) resolves
`import hew.math.stats` without anything being installed. For a
registry-backed project, add the published package:

```sh
hew init hello-registry
cd hello-registry
hew add hew.math.stats
hew install
```

Save the program as `main.hew`, then run `hew run main.hew` without `--pkg-path`.

Three things in that snippet carry across every package here. A fallible call
returns `Result<T, E>` with a package-specific error enum, never a sentinel
value. Variants are matched in dotted form (`.Ok`, `.Err`) because the
scrutinee's type already selects the enum. And every error enum has an
`error_message` that turns it into a printable string, so a match arm never has
to reach inside the variant to report a failure.

The service-backed packages add one wrapper: they own their connection in an
actor, so a call is `conn.query(sql)` and its answer is
`Result<Result<T, E>, ActorError>` — the outer `Result` reports whether the actor
answered at all, the inner one whether the operation succeeded.

```hew
let conn = spawn postgres.Conn { connstr: "host=127.0.0.1 user=hew password=hew dbname=hew_test" };
match conn.query("select 1") {
    .Ok(result) => match result {
        .Ok(query) => println(f"{query.rows.len()} rows"),
        .Err(error) => println(postgres.error_message(error)),
    },
    .Err(_) => println("connection actor stopped"),
}
stop(conn);
stopped(conn);
```

Those credentials are the ones `docker-compose.yml` starts PostgreSQL with, so
the snippet runs as written after `docker compose up --detach --wait postgres`.

Each package's `README.md` opens with a runnable example of exactly this shape,
and `examples/` holds the same program as a file you can run.

## Build, check, publish

[CONTRIBUTING.md](CONTRIBUTING.md) lists what to run before pushing.
[`docs/toolchain.md`](docs/toolchain.md) covers the pinned compiler, how to
move the pin, and the corpus gate; [`docs/packaging.md`](docs/packaging.md)
covers package layout and the native-crate contract.

`hew.image.magick` needs the ImageMagick 7 development files installed; see
[`image/magick/README.md`](image/magick/README.md).

## Documentation

The language itself is documented at
[hew-lang/hew](https://github.com/hew-lang/hew) — start with
`docs/hew-language-guide.md` there for the type system, actors, and ownership
rules the packages here assume you know.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. See [CONTRIBUTING.md](CONTRIBUTING.md) to add a package and
[SECURITY.md](SECURITY.md) to report a vulnerability.
