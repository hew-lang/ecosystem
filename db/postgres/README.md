# hew.db.postgres

`hew.db.postgres` owns each PostgreSQL connection in an actor. Operations
return typed `Result` values, query replies are immutable wire-safe snapshots,
and SQL `NULL` is represented by `CellValue.Null`.

The package wraps the native Rust `postgres` crate over a C ABI: cell values
cross that boundary as UTF-8 `bytes` text snapshots, not typed columns.

```hew
import hew.db.postgres;

fn main() {
    let db = spawn postgres.Conn(connstr: "host=127.0.0.1 user=hew password=hew dbname=hew_test");
    match await db.query("SELECT 41::bigint AS value") {
        .Ok(result) => match result {
            .Ok(query) => match query.rows[0].values[0] {
                .Text(value) => println(value.to_string()),
                .Null => println("value is NULL"),
            },
            .Err(error) => println(postgres.error_message(error)),
        },
        .Err(_) => println("PostgreSQL actor stopped before replying"),
    }
    db.close();
}
```

The checked example lives at [`examples/basic.hew`](examples/basic.hew). With
PostgreSQL listening locally and the `hew_test` database/user configured, run it
from the ecosystem checkout:

```sh
hew run --pkg-path . db/postgres/examples/basic.hew
```

Parameterized methods accept newline-delimited UTF-8 `bytes` values for `$1`,
`$2`, and later placeholders. Cell payloads are `bytes`; SQL strings cross the
native boundary with explicit lengths.
