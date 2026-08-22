# hew.queue.nats

A typed, actor-owned NATS client for publish/subscribe and request/reply.
Subscriptions are owned by their connection actor, receive timeouts are
`Receive.Missing`, and messages are immutable `#[wire]` snapshots.

Run a server and the example from the repository root:

```sh
docker run --rm -d --name hew-nats -p 14222:4222 nats:2.11.8-alpine3.22
hew run --pkg-path . queue/nats/examples/pubsub.hew
```

Awaiting an actor call returns `Result<R, AskError>`; here `R` is itself a
`Result<T, NatsError>`, so every match nests one level: the outer `Result`
covers the actor call, the inner one covers the NATS operation.

```hew
import hew.queue.nats;

fn main() {
    let client = spawn nats.Conn(options: Options { url: "nats://127.0.0.1:14222" });
    match await client.subscribe("hew.readme") {
        .Ok(result) => match result {
            .Ok(subscription) => {
                let _ = await client.publish("hew.readme", "hello");
                match await client.next_message(subscription.clone(), 1000) {
                    .Ok(receive_result) => match receive_result {
                        .Ok(.Message(message)) => println(message.data),
                        .Ok(.Missing) => println("no message arrived"),
                        .Err(error) => println(nats.error_message(error)),
                    },
                    .Err(_) => println("NATS actor stopped before replying"),
                }
                let _ = client.unsubscribe(subscription);
            },
            .Err(error) => println(nats.error_message(error)),
        },
        .Err(_) => println("NATS actor stopped before replying"),
    }
    let _ = client.close();
}
```

## API surface

- `actor Conn` — owns one NATS connection and every subscription it creates.
  - `init(options: Options)`
  - `receive fn publish(subject: string, data: string) -> Result<(), NatsError>`
  - `receive fn subscribe(subject: string) -> Result<Subscription, NatsError>`
  - `receive fn next_message(subscription: Subscription, timeout_ms: i64) -> Result<Receive, NatsError>`
  - `receive fn request(subject: string, data: string, timeout_ms: i64) -> Result<Receive, NatsError>`
  - `receive fn reply(message: Delivery, data: string) -> Result<(), NatsError>`
  - `receive fn unsubscribe(subscription: Subscription)`
  - `receive fn close()`
- `type Options { url: string }`
- `type Subscription { handle: i64 }`
- `type Delivery { subject: string, data: string, reply_to: ReplyTo }`
- `enum ReplyTo { Missing, Subject(string) }`
- `enum Receive { Missing, Message(Delivery) }`
- `enum NatsError { Connection, InvalidInput, Operation, Timeout, Closed, Internal }` — each variant carries a diagnostic `string`.
- `fn error_message(error: NatsError) -> string` — extracts the diagnostic from any `NatsError` variant.
