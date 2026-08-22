# hew.math.stats

Typed descriptive, streaming, correlation, regression, moving-average, and
histogram statistics for Hew. Operations that are undefined for their inputs
return `StatsError` instead of a numeric or empty-collection sentinel.

```hew
import hew.math.stats;

fn main() {
    let values = [1.0, 2.0, 3.0, 4.0, 5.0];
    match stats.mean(values) {
        .Ok(mean) => println(f"mean = {mean}"),
        .Err(error) => println(stats.error_message(error)),
    }
}
```

Run the example with `hew run math/stats/examples/basic.hew` from the ecosystem
repository root.

## API surface

Descriptive statistics — `mean`, `sum`, `min_val`, `max_val`, `range`,
`variance`, `variance_sample`, `std_dev`, `std_dev_sample`, `median`.

Percentiles & quartiles — `percentile`, `quartiles`, `iqr`.

Correlation & regression — `correlation`, `covariance`, `linear_regression`
(returns `LinearFit { slope, intercept, r_squared }`).

Moving averages — `simple_moving_avg`, `exponential_moving_avg`.

Histograms — `histogram` (returns `Histogram { edges, counts }`).

Streaming statistics — `running_new()` returns a `Running` accumulator with
`push`, `count`, `mean`, `variance`, `std_dev`, `min`, `max` methods, computed
incrementally via Welford's online algorithm.

Errors — every fallible function returns `Result<T, StatsError>`;
`error_message(error)` renders a `StatsError` as a diagnostic string.
