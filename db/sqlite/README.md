# hew.db.sqlite

`hew.db.sqlite` is an actor-owned embedded SQL client. Operations return
typed `Result` values, query replies are immutable wire-safe snapshots, and SQL
`NULL` is represented by `CellValue.Null`.

Because `await` itself can fail (the actor may have stopped), every reply is
`Result<Result<T, SqliteError>, AskError>` — match the outer `Result` for ask
failure, the inner for SQL failure.

```hew
import hew.db.sqlite;

fn main() {
    let db = spawn sqlite.Db(path: ":memory:");
    let _ = await db.execute("CREATE TABLE values_table (value TEXT)");
    let _ = await db.execute("INSERT INTO values_table VALUES ('hello')");
    match await db.query("SELECT value FROM values_table") {
        .Ok(result) => match result {
            .Ok(query) => match query.rows[0].values[0] {
                CellValue.Text(value) => println(value.to_string()),
                CellValue.Null => println("value is NULL"),
            },
            .Err(error) => println(sqlite.error_message(error)),
        },
        .Err(_) => println("SQLite actor stopped before replying"),
    }
    let _ = db.close();
}
```

The checked, self-contained example lives at
[`examples/basic.hew`](examples/basic.hew). Run it from the ecosystem checkout:

```sh
hew run --pkg-path . db/sqlite/examples/basic.hew
```

Parameterized methods accept newline-delimited UTF-8 `bytes` values for `?`
placeholders. Cell payloads are also `bytes`, preserving empty and embedded-NUL
values exactly. SQL strings cross the native boundary with explicit lengths.

## API surface

`spawn sqlite.Db(path: string)` opens (or creates) the database file at
`path`; pass `":memory:"` for an in-process database. `Db` is an actor — every
call below is made with `await db.<method>(...)`.

| Method | Returns | Notes |
| --- | --- | --- |
| `execute(sql: string)` | `Result<i64, SqliteError>` | Runs a statement with no `?` placeholders; the `i64` is the affected row count. |
| `execute_params(sql: string, params: bytes)` | `Result<i64, SqliteError>` | Same as `execute`, with newline-delimited `?` bindings. |
| `query(sql: string)` | `Result<QueryResult, SqliteError>` | Runs a `SELECT` with no placeholders. |
| `query_params(sql: string, params: bytes)` | `Result<QueryResult, SqliteError>` | Same as `query`, with newline-delimited `?` bindings. |
| `close()` | — | Releases the native connection. Called automatically on actor stop if skipped. |

Public types:

- `QueryResult { columns: Vec<string>, rows: Vec<Row> }`
- `Row { values: Vec<CellValue> }`
- `CellValue` — `.Text(bytes)` or `.Null`
- `SqliteError` — `.Open`, `.InvalidInput`, `.Query`, `.Closed`, `.Internal`, each carrying a `string` message
- `error_message(error: SqliteError) -> string` — extracts the message from any `SqliteError` variant
