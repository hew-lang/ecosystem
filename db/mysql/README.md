# hew.db.mysql

`hew.db.mysql` owns each MySQL connection in an actor. Operations return
typed `Result` values, query replies are immutable wire-safe snapshots, and SQL
`NULL` is represented by `CellValue.Null` rather than an empty-string sentinel.

```hew
import hew.db.mysql;

fn main() {
    let db = spawn mysql.Conn(url: "mysql://hew:hew@127.0.0.1:3306/hew_test");
    match await db.query("SELECT 41 AS value") {
        .Ok(result) => match result {
            .Ok(query) => match query.rows[0].values[0] {
                CellValue.Text(value) => println(value.to_string()),
                CellValue.Null => println("value is NULL"),
            },
            .Err(error) => println(mysql.error_message(error)),
        },
        .Err(_) => println("MySQL actor stopped before replying"),
    }
    let _ = db.close();
}
```

The checked example lives at [`examples/basic.hew`](examples/basic.hew). With
MySQL listening locally and the `hew_test` database/user configured, run it from
the ecosystem checkout:

```sh
hew run --pkg-path . db/mysql/examples/basic.hew
```

Parameterized methods accept `Vec<Param>` from `hew.db.sql`. For example,
`[Param.Int(42), Param.Text("First line\nSecond line"), Param.Null]` binds three
values. Use `Param.Bytes(data)` for arbitrary binary data, including invalid
UTF-8. Empty values and NULL stay distinct. See [typed SQL parameters](../sql/README.md)
for all six variants and database-specific conversions. Cell payloads remain
`bytes`, preserving exact values. SQL strings cross the native boundary with
explicit lengths.
