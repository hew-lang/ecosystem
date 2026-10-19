# hew.db.sql

`hew.db.sql` holds the types SQLite, MySQL and PostgreSQL share. `Param` is the
parameter type:

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

A program can handle results from any of the three drivers with one function
over `sql.QueryResult`.

The clients encode parameters using Hew's shared wire codec. The binary format
is an internal native boundary, not an application serialization API.
