# hew.db.redis

An actor-owned Redis client for Hew. It returns typed `Result` values,
distinguishes missing values from present empty byte buffers, and automatically
releases connection, reply, collection, and pipeline resources.

## Example

Run Redis locally on its default port, then save this as `main.hew` in a
project that depends on `hew.db.redis = "0.3.0"`:

```hew
import hew.db.redis;

fn main() {
    let client = spawn redis.Conn(options: Options { url: "redis://127.0.0.1/" });

    match await client.set("greeting", "hello".to_bytes()) {
        .Ok(result) => match result {
            .Ok(_) => {},
            .Err(error) => println(redis.error_message(error)),
        },
        .Err(_) => println("Redis actor stopped before replying"),
    }

    match await client.get("greeting") {
        .Ok(result) => match result {
            .Ok(.Value(value)) => println(value.to_string()),
            .Ok(.Missing) => println("greeting is missing"),
            .Err(error) => println(redis.error_message(error)),
        },
        .Err(_) => println("Redis actor stopped before replying"),
    }

    let _ = client.close();
}
```

The checked example at [`examples/basic.hew`](examples/basic.hew) is the same
program against the Redis service in `docker-compose.yml`. Run it from the
ecosystem checkout:

```sh
docker compose up -d redis
hew run --pkg-path . db/redis/examples/basic.hew
```

It prints `hello`.

All actor calls have two error layers: the outer `Result` is Hew's actor ask
result; the inner `Result` is the Redis operation result. Redis payloads use
Hew's `bytes` ABI end to end, so embedded NUL and non-text bytes round-trip
without C-string truncation.

A plain `import hew.db.redis;` brings in the module, not its type names
unqualified. `Options { url: ... }` still works above because the `spawn`
parameter's type selects it, and `.Value`/`.Missing` work because the match
scrutinee's type does. Writing one of those names where nothing selects it — a
`let` annotation, or a `Vec<Lookup>` — needs either a qualified `redis.Lookup`
or a brace import:

```hew
import hew.db.redis.{Lookup};
```

That is the whole rule, and it is why some files here carry a brace import and
most do not.

## Pipelines

Pipelines are submitted as a complete wire-safe command list. The actor owns
the native queue and releases it on success or error:

```hew
let commands: Vec<redis.PipelineCommand> = Vec.new();
commands.push(redis.PipelineCommand.Set("one", "1".to_bytes()));
commands.push(redis.PipelineCommand.Set("two", "2".to_bytes()));
let _ = await client.pipeline_exec(commands);
```

## API surface

`pub actor Conn { init(options: Options) ... }` — one connection per actor
instance. Every method below is a `receive fn` and returns
`Result<T, RedisError>` unless noted:

| Method | Returns |
| --- | --- |
| `set(key, value: bytes)` | number of keys set |
| `get(key)` | `Lookup` |
| `del(key)` | number of keys removed |
| `set_ex(key, value: bytes, ttl_seconds)` | number of keys set |
| `expire(key, ttl_seconds)` | `bool` — whether the key existed |
| `ttl(key)` | `KeyTtl` |
| `incr(key)` | the incremented `i64` |
| `lpush(key, value: bytes)` | new list length |
| `rpop(key)` | `Lookup` |
| `hset(key, field, value: bytes)` | `bool` — newly added |
| `hget(key, field)` | `Lookup` |
| `hdel(key, field)` | `bool` — whether it existed |
| `hgetall(key)` | `Vec<HashEntry>` |
| `sadd(key, member: bytes)` | `bool` — newly inserted |
| `srem(key, member: bytes)` | `bool` — whether it existed |
| `smembers(key)` | `Vec<SetMember>` |
| `sismember(key, member: bytes)` | `bool` |
| `publish(channel, message: bytes)` | subscriber count |
| `subscribe_once(channel, timeout_ms)` | `bytes` — one message, or `RedisError.Timeout` |
| `pipeline_exec(commands: Vec<PipelineCommand>)` | pipeline result count |
| `close()` | closes the native connection; idempotent |

Types: `RedisError` (`Connection`, `InvalidInput`, `Command`, `Timeout`,
`Closed`, `Internal` — use `error_message(error)` for the diagnostic string),
`Lookup` (`Missing` / `Value(bytes)`), `KeyTtl` (`Missing` / `Persistent` /
`Seconds(i64)`), `HashEntry { field, value }`, `SetMember { value }`,
`Options { url }`, and `PipelineCommand` (`Ping`, `Del`, `Set`, `Hset`).
