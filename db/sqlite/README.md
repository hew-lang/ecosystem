# hew.db.sqlite

`hew.db.sqlite` is an actor-owned embedded SQL client. Operations fail with a
typed `SqliteError`, query replies are immutable wire-safe snapshots, and SQL
`NULL` is represented by `CellValue.Null`. The query result types come from
[`hew.db.sql`](../sql/README.md), shared with MySQL and PostgreSQL.

Every method is a `receive fn ... -> T fails SqliteError`, so a call replies
with `Result<T, ActorError<SqliteError>>`: `.Ok(value)` on success,
`.Err(.Failed(error))` for a SQL failure, and another `.Err` when the actor
could not answer (for example, `.Dead` after it stopped).

```hew
import hew.db.sqlite;
import std.encoding.utf8;

fn main() {
    let db = spawn sqlite.Db { path: ":memory:" };
    let _ = db.execute("CREATE TABLE values_table (value TEXT)");
    let _ = db.execute("INSERT INTO values_table VALUES ('hello')");
    match db.query("SELECT value FROM values_table") {
        .Ok(query) => match query.rows[0].values[0] {
            .Text(value) => println(utf8.decode_lossy(value)),
            .Null => println("value is NULL"),
        }
        .Err(.Failed(error)) => println(sqlite.error_message(error)),
        .Err(_) => println("SQLite actor stopped before replying"),
    }
    stop(db);
    stopped(db);
}
```

The checked, self-contained example lives at
[`examples/basic.hew`](examples/basic.hew). Run it from the ecosystem checkout:

```sh
hew run --pkg-path . db/sqlite/examples/basic.hew
```

Parameterized methods accept `Vec<Param>` from `hew.db.sql`. For example,
`[Param.Int(42), Param.Text("First line\nSecond line"), Param.Null]` binds three
values. Use `Param.Bytes(data)` for arbitrary binary data, including invalid
UTF-8. Empty values and NULL stay distinct. See [typed SQL parameters](../sql/README.md)
for all six variants and database-specific conversions. Cell payloads remain
`bytes`, preserving exact values. SQL strings cross the native boundary with
explicit lengths.

## API surface

`spawn sqlite.Db { path: filename }` opens (or creates) the database file at
`path`; pass `":memory:"` for an in-process database. `Db` is an actor — every
call below is made with `db.<method>(...)`.

| Method | Returns | Notes |
| --- | --- | --- |
| `execute(sql: string)` | `i64 fails SqliteError` | Runs a statement with no `?` placeholders; the `i64` is the affected row count. |
| `execute_params(sql: string, params: Vec<Param>)` | `i64 fails SqliteError` | Same as `execute`, with typed `?` bindings. |
| `query(sql: string)` | `QueryResult fails SqliteError` | Runs a `SELECT` with no placeholders. |
| `query_params(sql: string, params: Vec<Param>)` | `QueryResult fails SqliteError` | Same as `query`, with typed `?` bindings. |
| `stop(db); stopped(db);` | — | Gracefully stops the actor and waits for native connection cleanup. |

Public types, with `QueryResult`, `Row` and `CellValue` from `hew.db.sql`:

- `QueryResult { columns: Vec<string>; rows: Vec<Row>; }`
- `Row { values: Vec<CellValue>; }`
- `CellValue` — `.Text(bytes)` or `.Null`
- `SqliteError` — `.Open`, `.InvalidInput`, `.Query`, `.Closed`, `.Internal`, each carrying a `string` message
- `error_message(error: SqliteError) -> string` — extracts the message from any `SqliteError` variant
