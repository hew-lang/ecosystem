# hew.queue.mqtt

A typed, actor-owned MQTT 3.1.1 client for Hew. Publish and subscribe errors
are values, invalid QoS levels are rejected, and receive timeouts are represented
by `Receive.Missing`.

Run a broker and the example from the repository root:

```sh
docker run --rm -d --name hew-mqtt -p 1883:1883 eclipse-mosquitto:2.0.22 \
  mosquitto -c /mosquitto-no-auth.conf
hew run --pkg-path . queue/mqtt/examples/pubsub.hew
```

Each `Conn` method is a `receive fn` that returns `T fails MqttError`, so a
call replies with `Result<T, ActorError<MqttError>>`: `.Ok(value)` on success,
`.Err(.Failed(error))` when the MQTT operation fails, and another `.Err` when
the actor could not answer.

```hew
import hew.queue.mqtt;

fn main() {
    let client = spawn mqtt.Conn { options: mqtt.Options {
        host: "127.0.0.1", port: 1883,
        client_id: "hew-readme", keepalive_seconds: 30,
    } };
    let _ = client.subscribe("hew/readme", mqtt.QoS.AtLeastOnce);
    let _ = client.publish("hew/readme", "hello", mqtt.QoS.AtLeastOnce, false);
    match client.next_message(1000) {
        .Ok(.Message(message)) => println(message.payload),
        .Ok(.Missing) => println("no message arrived"),
        .Err(.Failed(error)) => println(mqtt.error_message(error)),
        .Err(_) => println("MQTT actor stopped before replying"),
    }
    stop(client);
    stopped(client);
}
```
