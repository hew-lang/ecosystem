# hew.metrics

Actor-owned Prometheus counters, gauges, histograms, labels, and text export.
All registration and mutation APIs return `Result`, and stopping or closing the
actor releases its native registry.

Each labeled metric retains at most 1024 distinct label-value tuples. Once the
limit is reached, existing tuples remain mutable and operations with a new
tuple return `MetricsError.SeriesLimit`.

```hew
import hew.metrics;

fn export_requests() -> string {
    let registry = spawn metrics.Registry;
    let text = match registry.counter("requests_total", "Handled requests") {
        .Ok(result) => match result {
            .Ok(counter) => {
                let _ = registry.counter_inc(counter);
                let _ = registry.counter_add(counter, 2.0);
                match registry.export() {
                    .Ok(export_result) => match export_result {
                        .Ok(text) => text,
                        .Err(error) => metrics.error_message(error),
                    }
                    .Err(_) => "metrics actor stopped before replying",
                }
            }
            .Err(error) => metrics.error_message(error),
        }
        .Err(_) => "metrics actor stopped before replying",
    };
    stop(registry);
    stopped(registry);
    text
}

fn main() {
    println(export_requests());
}

#[test]
#[real_time]
fn exports_the_incremented_counter() {
    assert(export_requests().contains("\nrequests_total 3\n"));
}
```

This is [`examples/basic.hew`](examples/basic.hew) without its header comment. Run it
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
than incrementing a counter and exiting silently, and its test checks the
`requests_total 3` line.
