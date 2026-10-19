# hew.db.sql

`hew.db.sql` holds the types SQLite, MySQL and PostgreSQL share: `Param` for
query parameters, and `QueryResult`, `Row` and `CellValue` for replies. Because
every driver replies with the same `QueryResult`, one function handles results
from any of them:

```hew
import std.encoding.utf8;

import hew.db.sql.{Param, CellValue, Row, QueryResult};

// One line per row, columns separated by ` | `, SQL NULL shown as `NULL` and
// an empty value shown as `''` so the two cannot be confused.
fn render(result: QueryResult) -> string {
    var out = result.columns.join(" | ");
    for row in result.rows {
        var cells: Vec<string> = Vec.new();
        for value in row.values {
            match value {
                .Null => cells.push("NULL"),
                .Text(text) => {
                    if text.len() == 0 {
                        cells.push("''");
                    } else {
                        cells.push(utf8.decode_lossy(text));
                    }
                }
            }
        }
        out = out + "\n" + cells.join(" | ");
    }
    out
}

// The reply a driver gives for `SELECT id, name, note FROM people WHERE id IN
// (?, ?)` with the parameters in `main`.
fn people() -> QueryResult {
    let ada = Row { values: [CellValue.Text("1".to_bytes()), CellValue.Text("Ada".to_bytes()), CellValue.Null] };
    let grace = Row { values: [CellValue.Text("2".to_bytes()), CellValue.Text("Grace".to_bytes()), CellValue.Text(bytes.new())] };
    QueryResult { columns: ["id", "name", "note"], rows: [ada, grace] }
}

fn main() {
    let params = [Param.Int(1), Param.Int(2)];
    println(f"bound {params.len()} parameters");
    println(render(people()));
}
```

The same program is [`examples/render_rows.hew`](examples/render_rows.hew); run
it from the ecosystem checkout with `hew run db/sql/examples/render_rows.hew`.

## Parameters

```hew
import hew.db.sql.{Param};

let params = [Param.Int(42), Param.Text("First line\nSecond line"), Param.Null];
```

Use `Null`, `Bool(bool)`, `Int(i64)`, `Float(f64)`, `Text(string)` or
`Bytes(bytes)` in `Vec<Param>`. Empty text, empty binary data and SQL NULL are
distinct values. Binary data need not be UTF-8. Parameters are bound separately
from SQL, never interpolated into it.

Use `?` placeholders with SQLite and MySQL, and `$1`, `$2`, etc. with
PostgreSQL. PostgreSQL binds integers as `bigint`, floats as `double precision`,
text as `text` and binary data as `bytea`. The database applies explicit casts
and column conversions; incompatible values and out-of-range conversions return
a query error. An untyped NULL needs enough SQL context to infer its type (for
example, `CAST($1 AS text)`). SQLite stores booleans as 0 or 1; MySQL follows its
boolean alias for a small integer.

## Query results

Every driver's `query` and `query_params` reply with the same `QueryResult`:

- `QueryResult { columns: Vec<string>; rows: Vec<Row>; }`
- `Row { values: Vec<CellValue>; }`, one value per column
- `CellValue` — `.Null` for SQL NULL, or `.Text(bytes)` with the database's
  text form of the value, which need not be UTF-8

The clients encode parameters using Hew's shared wire codec. The binary format
is an internal native boundary, not an application serialization API.
