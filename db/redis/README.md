# hew.db.redis

An actor-owned Redis client for Hew. Its commands fail with a typed `RedisError`,
distinguishes missing values from present empty byte buffers, and automatically
releases connection, reply, collection, and pipeline resources.

## Example

Run Redis locally on its default port, then save this as `main.hew` in a
project that depends on `hew.db.redis = "0.4.0"`:

```hew
import hew.db.redis;
import std.encoding.utf8;

fn main() {
    let client = spawn redis.Conn { options: redis.Options { url: "redis://127.0.0.1/" } };

    match client.set("greeting", "hello".to_bytes()) {
        .Ok(_) => {}
        .Err(.Failed(error)) => println(redis.error_message(error)),
        .Err(_) => println("Redis actor stopped before replying"),
    }

    match client.get("greeting") {
        .Ok(.Value(value)) => println(utf8.decode_lossy(value)),
        .Ok(.Missing) => println("greeting is missing"),
        .Err(.Failed(error)) => println(redis.error_message(error)),
        .Err(_) => println("Redis actor stopped before replying"),
    }

    stop(client);
    stopped(client);
}
```

The checked example at [`examples/basic.hew`](examples/basic.hew) is the same
program, plus a test that runs it, against the Redis service in
`docker-compose.yml`. Run it from the
ecosystem checkout:

```sh
docker compose up -d redis
hew run --pkg-path . db/redis/examples/basic.hew
```

It prints `hello`.

Every command declares `fails RedisError`, so a call replies with
`Result<T, ActorError<RedisError>>`: `.Ok(value)` on success,
`.Err(.Failed(error))` when Redis reports a failure, and another `ActorError`
variant, such as `.Dead`, when the actor could not answer. Redis payloads use
Hew's `bytes` ABI end to end, so embedded NUL and non-text bytes round-trip
without C-string truncation.

A plain `import hew.db.redis;` brings in the module, not its type names
unqualified. The example writes `redis.Options { url: ... }` for that reason.
`.Value` and `.Missing` work because the match scrutinee's type selects the
enum. A type annotation, such as `Vec<Lookup>`, needs either a qualified
`redis.Lookup` or a brace import:

```hew
import hew.db.redis.{Lookup};
```

That is the whole rule, and it is why some files here carry a brace import and
most do not.

## Pipelines

Pipelines are submitted as a complete wire-safe command list. The actor owns
the native queue and releases it on success or error:

```hew
var commands: Vec<redis.PipelineCommand> = Vec.new();
commands.push(redis.PipelineCommand.Set("one", "1".to_bytes()));
commands.push(redis.PipelineCommand.Set("two", "2".to_bytes()));
let _ = client.pipeline_exec(commands);
```

## API surface

`pub actor Conn { init(options: Options) ... }` — one connection per actor
instance. Every method below is a `receive fn` declared `-> T fails RedisError`,
where `T` is the return listed:

| Method | Returns |
| --- | --- |
| `set(key, value: bytes)` | `()` |
| `get(key)` | `Lookup` |
| `del(key)` | number of keys removed |
| `set_ex(key, value: bytes, ttl_seconds)` | `()` |
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
| `stop(client); stopped(client);` | Gracefully stop the actor and wait for native connection cleanup. |

Types: `RedisError` (`Connection`, `InvalidInput`, `Command`, `Timeout`,
`Closed`, `Internal` — use `error_message(error)` for the diagnostic string),
`Lookup` (`Missing` / `Value(bytes)`), `KeyTtl` (`Missing` / `Persistent` /
`Seconds(i64)`), `HashEntry { field, value }`, `SetMember { value }`,
`Options { url }`, and `PipelineCommand` (`Ping`, `Del`, `Set`, `Hset`).
