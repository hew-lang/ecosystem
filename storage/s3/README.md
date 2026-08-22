# hew.storage.s3

An actor-owned S3-compatible object storage client with typed errors, exact
byte payloads, explicit missing-object outcomes, paginated listings, and
automatic native cleanup.

## Example

Start an S3-compatible service and create `hew-example`, then save this as
`main.hew` in a project depending on `hew.storage.s3 = "0.3.0"`:

```hew
import hew.storage.s3;

fn main() {
    let store = spawn s3.Bucket(options: Options {
        endpoint: "http://127.0.0.1:9000",
        region: "us-east-1",
        bucket: "hew-example",
        access_key: "<ACCESS_KEY>",
        secret_key: "<SECRET_KEY>",
    });

    match await store.put("hello.txt", "hello".to_bytes(), "text/plain") {
        .Ok(result) => match result {
            .Ok(_) => println("uploaded"),
            .Err(error) => println(s3.error_message(error)),
        },
        .Err(_) => println("S3 actor stopped before replying"),
    }

    match await store.get("hello.txt") {
        .Ok(result) => match result {
            .Ok(.Found(body)) => println(body.to_string()),
            .Ok(.Missing) => println("object is missing"),
            .Err(error) => println(s3.error_message(error)),
        },
        .Err(_) => println("S3 actor stopped before replying"),
    }

    let _ = store.close();
}
```

The outer `Result` on an actor ask reports mailbox/actor failure. The inner
`Result` reports the S3 operation outcome.

The checked example at [`examples/basic.hew`](examples/basic.hew) is the same
program against the MinIO service in `docker-compose.yml`. Run it from the
ecosystem checkout:

```sh
docker compose up -d minio minio-init
hew run --pkg-path . storage/s3/examples/basic.hew
```

It prints `uploaded example/hello.txt` and then `hello`.

## API surface

`spawn s3.Bucket(options: Options)` connects and returns the actor handle.
`Options` fields: `endpoint`, `region`, `bucket`, `access_key`, `secret_key`
(all `string`).

Every operation below is `receive fn`, called as `await store.<name>(...)`,
and returns `Result<T, S3Error>` inside the actor-ask `Result`:

- `put(key: string, body: bytes, content_type: string) -> Result<i64, S3Error>`
  — upload exact bytes under `key`.
- `get(key: string) -> Result<ObjectLookup, S3Error>` — download exact bytes.
  `ObjectLookup` is `.Missing` or `.Found(bytes)`; a missing object is distinct
  from a present empty one.
- `delete(key: string) -> Result<i64, S3Error>` — deleting an absent key is
  successful S3 behaviour, not an error.
- `list(prefix: string) -> Result<Vec<ObjectInfo>, S3Error>` — every object
  under `prefix`, following continuation tokens. `ObjectInfo` has `key:
  string` and `size: i64`.
- `exists(key: string) -> Result<bool, S3Error>`.
- `presign(key: string, method: string, expires_seconds: i64) -> Result<string,
  S3Error>` — a signed URL for `"GET"`, `"PUT"`, `"DELETE"`, or `"HEAD"`.
- `close()` — release the native client. Idempotent; also runs automatically
  when the actor stops.

`s3.error_message(error: S3Error) -> string` returns the diagnostic carried by
any `S3Error` variant (`Connection`, `InvalidInput`, `NotFound`,
`AccessDenied`, `Throttled`, `ServerError(i64, string)`, `Network`,
`HttpStatus(i64, string)`, `Decode`, `Closed`, `Internal`).
