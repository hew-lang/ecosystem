# hew.db.mongodb

`hew.db.mongodb` is an actor-owned MongoDB client. Documents, filters, and
updates are JSON objects. All fallible operations return `Result`; missing
documents are represented by `DocumentLookup.Missing`, separately from
connection and query failures.

```hew
import hew.db.mongodb;

fn main() {
    let db = spawn mongodb.Conn(
        uri: "mongodb://127.0.0.1:27017",
        database: "example",
    );

    match await db.insert_one("users", "{\"name\":\"Alice\",\"age\":30}") {
        .Ok(result) => match result {
            .Ok(id) => println(f"inserted {id}"),
            .Err(error) => println(mongodb.error_message(error)),
        },
        .Err(_) => println("MongoDB actor stopped before replying"),
    }

    match await db.find_one("users", "{\"name\":\"Alice\"}") {
        .Ok(result) => match result {
            .Ok(.Found(document)) => println(document),
            .Ok(.Missing) => println("user not found"),
            .Err(error) => println(mongodb.error_message(error)),
        },
        .Err(_) => println("MongoDB actor stopped before replying"),
    }

    db.close();
}
```

The checked example lives at [`examples/basic.hew`](examples/basic.hew). Run
it from the ecosystem checkout with a MongoDB server listening locally:

```sh
hew run --pkg-path . db/mongodb/examples/basic.hew
```

Construction performs a server ping. Add a bounded
`serverSelectionTimeoutMS` URI option when the application needs a deadline
shorter than the MongoDB driver's default. An unreachable server is preserved
as `MongoError.Connect`; later calls do not disguise it as an empty result.
The native package accepts direct `mongodb://` seed-list URIs; DNS SRV
discovery through `mongodb+srv://` is not enabled in this portable static-link
profile.

`find_one` returns `DocumentLookup.Missing` when no document matches. `find`
returns an immutable `QueryResult` containing every matching document. The
native MongoDB cursor is a private Hew `#[resource]`: it is always closed
before the actor replies, including query-row and serialization failure paths.
The actor similarly closes its client from both `close()` and its stop hook.

All Hew-to-native string inputs use explicit byte lengths, and all native data
results return as owned Hew `bytes` triples before Hew decodes their UTF-8 JSON
or identifier text. Document values are therefore never truncated at embedded
NUL bytes; valid JSON escapes are preserved, while invalid raw NUL in JSON is
reported as `MongoError.InvalidJson` instead of changing the submitted value.

Native unit tests run without a server:

```sh
cargo test -p hew-ecosystem-db-mongodb
```

The public Hew test deliberately uses an unreachable bounded-time URI to cover
connection failure and post-close behavior without external services:

```sh
hew run --pkg-path . db/mongodb/tests/public_api.hew
```

With MongoDB listening on `127.0.0.1:27017`, the service-backed suite covers
successful CRUD, missing documents, invalid JSON for every operation, exact
counts, query contents, and repeated cursor lifecycle behavior:

```sh
hew run --pkg-path . db/mongodb/tests/integration.hew
```
