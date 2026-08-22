# hew.queue.mqtt

A typed, actor-owned MQTT 3.1.1 client for Hew. Publish and subscribe errors
are values, invalid QoS levels are rejected, and receive timeouts are represented
by `Receive.Missing`.

Run a broker and the example from the repository root:

```sh
docker run --rm -d --name hew-mqtt -p 11883:1883 eclipse-mosquitto:2.0.22 \
  mosquitto -c /mosquitto-no-auth.conf
hew run --pkg-path . queue/mqtt/examples/pubsub.hew
```

Awaiting an actor call returns `Result<R, MqttError>` wrapped in the
`Result<_, AskError>` the ask itself can fail with, so every reply is matched
one layer at a time: the outer `Result` covers the actor call, the inner one
covers the MQTT operation.

```hew
import hew.queue.mqtt;

fn main() {
    let client = spawn mqtt.Conn(options: Options {
        host: "127.0.0.1", port: 11883,
        client_id: "hew-readme", keepalive_seconds: 30,
    });
    let _ = await client.subscribe("hew/readme", mqtt.QoS.AtLeastOnce);
    let _ = await client.publish("hew/readme", "hello", mqtt.QoS.AtLeastOnce, false);
    match await client.next_message(1000) {
        .Ok(result) => match result {
            .Ok(.Message(message)) => println(message.payload),
            .Ok(.Missing) => println("no message arrived"),
            .Err(error) => println(mqtt.error_message(error)),
        },
        .Err(_) => println("MQTT actor stopped before replying"),
    }
    let _ = client.close();
}
```
