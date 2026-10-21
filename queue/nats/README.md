# hew.queue.nats

A typed, actor-owned NATS client for publish/subscribe and request/reply.
Subscriptions are owned by their connection actor, receive timeouts are
`Receive.Missing`, and messages are immutable `#[wire]` snapshots.

Run a server and the example from the repository root:

```sh
docker run --rm -d --name hew-nats -p 14222:4222 nats:2.11.8-alpine3.22
hew run --pkg-path . queue/nats/examples/pubsub.hew
```

Each `Conn` method is a `receive fn` that returns `T fails NatsError`, so a
call replies with `Result<T, ActorError<NatsError>>`: `.Ok(value)` on success,
`.Err(.Failed(error))` when the NATS operation fails, and another `.Err` when
the actor could not answer.

```hew
import hew.queue.nats;

fn main() {
    let client = spawn nats.Conn { options: nats.Options { url: "nats://127.0.0.1:14222" } };
    match client.subscribe("hew.readme") {
        .Ok(subscription) => {
            let _ = client.publish("hew.readme", "hello");
            match client.next_message(subscription.clone(), 1000) {
                .Ok(.Message(message)) => println(message.data),
                .Ok(.Missing) => println("no message arrived"),
                .Err(.Failed(error)) => println(nats.error_message(error)),
                .Err(_) => println("NATS actor stopped before replying"),
            }
            let _ = client.unsubscribe(subscription);
        }
        .Err(.Failed(error)) => println(nats.error_message(error)),
        .Err(_) => println("NATS actor stopped before replying"),
    }
    stop(client);
    stopped(client);
}
```

## API surface

- `actor Conn` — owns one NATS connection and every subscription it creates.
  - `init(options: Options)`
  - `receive fn publish(subject: string, data: string) fails NatsError`
  - `receive fn subscribe(subject: string) -> Subscription fails NatsError`
  - `receive fn next_message(subscription: Subscription, timeout_ms: i64) -> Receive fails NatsError`
  - `receive fn request(subject: string, data: string, timeout_ms: i64) -> Receive fails NatsError`
  - `receive fn reply(message: Delivery, data: string) fails NatsError`
  - `receive fn unsubscribe(subscription: Subscription)`
- `stop(client); stopped(client);` — gracefully stop the actor and wait for connection cleanup.
- `type Options { url: string; }`
- `type Subscription { handle: i64; }`
- `type Delivery { subject: string; data: string; reply_to: ReplyTo; }`
- `enum ReplyTo { Missing; Subject(string); }`
- `enum Receive { Missing; Message(Delivery); }`
- `enum NatsError { Connection; InvalidInput; Operation; Closed; Internal; }` — each variant carries a diagnostic `string`.
- `fn error_message(error: NatsError) -> string` — extracts the diagnostic from any `NatsError` variant.
