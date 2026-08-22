# hew.metrics

Actor-owned Prometheus counters, gauges, histograms, labels, and text export.
All registration and mutation APIs return `Result`, and stopping or closing the
actor releases its native registry.

Each labeled metric retains at most 1024 distinct label-value tuples. Once the
limit is reached, existing tuples remain mutable and operations with a new
tuple return `MetricsError.SeriesLimit`.

```hew
import hew.metrics;

fn main() {
    let registry = spawn metrics.Registry;
    match await registry.counter("requests_total", "Handled requests") {
        .Ok(result) => match result {
            .Ok(counter) => {
                let _ = await registry.counter_inc(counter);
                let _ = await registry.counter_add(counter, 2.0);
                match await registry.export() {
                    .Ok(export_result) => match export_result {
                        .Ok(text) => println(text),
                        .Err(error) => println(metrics.error_message(error)),
                    },
                    .Err(_) => println("metrics actor stopped before replying"),
                }
            },
            .Err(error) => println(metrics.error_message(error)),
        },
        .Err(_) => println("metrics actor stopped before replying"),
    }
    let _ = registry.close();
}
```

This is the same program as [`examples/basic.hew`](examples/basic.hew). Run it
from the ecosystem checkout:

```sh
hew run --pkg-path . metrics/examples/basic.hew
```

It prints the Prometheus text exposition of everything the registry holds:

```text
# HELP requests_total Handled requests
# TYPE requests_total counter
requests_total 3
```

`export()` is what makes a registry observable, so the example calls it rather
than incrementing a counter and exiting silently.
