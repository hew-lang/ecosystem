//! Hew runtime: Prometheus-compatible metrics.
//!
//! Wraps the `prometheus` crate to provide counters, gauges, and histograms
//! for compiled Hew programs. Registry handles are opaque process-local `i64`
//! ids. A return of -1 indicates failure.
//!
//! Strings cross the boundary as managed Hew strings: inbound handles are
//! borrowed for the call, and returned handles are freshly allocated owners
//! the caller releases.

use hew_cabi::string::{string_as_str, string_from_str, HewString};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
};

/// Maximum number of distinct label-value tuples retained by one metric vector.
///
/// Once this limit is reached, operations on existing tuples continue to work,
/// while a new tuple is rejected with the series-limit status code.
pub const MAX_SERIES_PER_METRIC: usize = 1024;

const STATUS_INVALID: i32 = -1;
const STATUS_SERIES_LIMIT: i32 = -2;
const STATUS_KIND_MISMATCH: i32 = -3;
const METRIC_KIND_COUNT: i64 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i64)]
enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    CounterVec,
    GaugeVec,
    HistogramVec,
}

/// A per-registry collection of Prometheus metrics.
#[derive(Debug)]
pub struct HewMetricsRegistry {
    inner: Mutex<HewMetricsInner>,
}

#[derive(Debug)]
struct HewMetricsInner {
    registry: prometheus::Registry,
    counters: Vec<prometheus::Counter>,
    gauges: Vec<prometheus::Gauge>,
    histograms: Vec<prometheus::Histogram>,
    counter_vecs: Vec<BoundedMetricVec<prometheus::CounterVec>>,
    gauge_vecs: Vec<BoundedMetricVec<prometheus::GaugeVec>>,
    histogram_vecs: Vec<BoundedMetricVec<prometheus::HistogramVec>>,
}

#[derive(Debug)]
struct BoundedMetricVec<T> {
    metric: T,
    label_count: usize,
    series: HashSet<Vec<String>>,
}

impl<T> BoundedMetricVec<T> {
    fn new(metric: T, label_count: usize) -> Self {
        Self {
            metric,
            label_count,
            series: HashSet::new(),
        }
    }

    fn admit(&mut self, labels: &[String]) -> Result<(), i32> {
        if labels.len() != self.label_count {
            return Err(STATUS_INVALID);
        }
        if self.series.contains(labels) {
            return Ok(());
        }
        if self.series.len() >= MAX_SERIES_PER_METRIC {
            return Err(STATUS_SERIES_LIMIT);
        }
        self.series.insert(labels.to_vec());
        Ok(())
    }
}

static NEXT_REGISTRY: AtomicI64 = AtomicI64::new(1);
static REGISTRIES: OnceLock<Mutex<HashMap<i64, Arc<HewMetricsRegistry>>>> = OnceLock::new();

fn registries() -> &'static Mutex<HashMap<i64, Arc<HewMetricsRegistry>>> {
    REGISTRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn registry_for(handle: i64) -> Option<Arc<HewMetricsRegistry>> {
    if handle <= 0 {
        return None;
    }
    lock_or_recover(registries()).get(&handle).cloned()
}

fn push_handle<T>(items: &mut Vec<T>, item: T, kind: MetricKind) -> i64 {
    let Ok(idx) = i64::try_from(items.len()) else {
        return -1;
    };
    let Some(handle) = idx
        .checked_mul(METRIC_KIND_COUNT)
        .and_then(|base| base.checked_add(kind as i64))
    else {
        return -1;
    };
    items.push(item);
    handle
}

fn metric_index(handle: i64, expected: MetricKind) -> Result<usize, i32> {
    if handle < 0 {
        return Err(STATUS_INVALID);
    }
    if handle % METRIC_KIND_COUNT != expected as i64 {
        return Err(STATUS_KIND_MISMATCH);
    }
    usize::try_from(handle / METRIC_KIND_COUNT).map_err(|_| STATUS_INVALID)
}

/// Split a managed string into its delimited, non-empty parts.
///
/// # Safety
/// `value` must be null (empty) or a live managed Hew string.
unsafe fn parse_string_list(value: *const HewString) -> Vec<String> {
    // SAFETY: the caller supplies a live managed handle borrowed for this call.
    unsafe { string_as_str(value) }
        .split([',', '\n', '\r', '\t', ' '])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Parse a bucket list, falling back to the Prometheus defaults when empty.
///
/// # Safety
/// `value` must be null (empty) or a live managed Hew string.
unsafe fn parse_buckets(value: *const HewString) -> Option<Vec<f64>> {
    // SAFETY: forwards this function's own safety precondition unchanged.
    let values = unsafe { parse_string_list(value) };
    if values.is_empty() {
        return Some(prometheus::DEFAULT_BUCKETS.to_vec());
    }
    let mut buckets = Vec::with_capacity(values.len());
    for value in values {
        let bucket = value.parse::<f64>().ok()?;
        if !bucket.is_finite() {
            return None;
        }
        buckets.push(bucket);
    }
    Some(buckets)
}

fn label_refs(labels: &[String]) -> Vec<&str> {
    labels.iter().map(String::as_str).collect()
}

// ---------------------------------------------------------------------------
// Registry lifecycle
// ---------------------------------------------------------------------------

/// Create a new metrics registry.
///
/// Returns an opaque handle that can be freed with [`hew_metrics_close`].
#[no_mangle]
pub extern "C" fn hew_metrics_new() -> i64 {
    let handle = NEXT_REGISTRY.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        return -1;
    }
    let reg = prometheus::Registry::new();
    let registry = Arc::new(HewMetricsRegistry {
        inner: Mutex::new(HewMetricsInner {
            registry: reg,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
            counter_vecs: Vec::new(),
            gauge_vecs: Vec::new(),
            histogram_vecs: Vec::new(),
        }),
    });
    lock_or_recover(registries()).insert(handle, registry);
    handle
}

/// Free a registry and all its associated resources.
///
/// Passing 0, a negative handle, or an already-closed handle is a no-op.
#[no_mangle]
pub extern "C" fn hew_metrics_close(handle: i64) {
    if handle <= 0 {
        return;
    }
    lock_or_recover(registries()).remove(&handle);
}

/// Return the number of currently live registry handles.
#[no_mangle]
pub extern "C" fn hew_metrics_registry_count() -> i64 {
    i64::try_from(lock_or_recover(registries()).len()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// Metric registration
// ---------------------------------------------------------------------------

/// Register a counter with the given name and help string.
///
/// Returns the metric handle (≥ 0) on success, or -1 on error.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_new(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name` is a managed handle borrowed for this call.
    let name_str = unsafe { string_as_str(name) };
    // SAFETY: `help` is a managed handle borrowed for this call.
    let help_str = unsafe { string_as_str(help) };
    let Ok(counter) = prometheus::Counter::with_opts(prometheus::Opts::new(name_str, help_str))
    else {
        return -1;
    };
    let mut reg = lock_or_recover(&registry.inner);
    if reg.registry.register(Box::new(counter.clone())).is_err() {
        return -1;
    }
    push_handle(&mut reg.counters, counter, MetricKind::Counter)
}

/// Register a labeled counter vector.
///
/// `label_names` is a comma/newline/whitespace-delimited list of label names.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_vec_new(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
    label_names: *const HewString,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name` is a managed handle borrowed for this call.
    let name_str = unsafe { string_as_str(name) };
    // SAFETY: `help` is a managed handle borrowed for this call.
    let help_str = unsafe { string_as_str(help) };
    // SAFETY: `label_names` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_names) };
    if labels.is_empty() {
        return -1;
    }
    let label_refs = label_refs(&labels);
    let Ok(counter) =
        prometheus::CounterVec::new(prometheus::Opts::new(name_str, help_str), &label_refs)
    else {
        return -1;
    };
    let mut reg = lock_or_recover(&registry.inner);
    if reg.registry.register(Box::new(counter.clone())).is_err() {
        return -1;
    }
    push_handle(
        &mut reg.counter_vecs,
        BoundedMetricVec::new(counter, labels.len()),
        MetricKind::CounterVec,
    )
}

/// Register a gauge with the given name and help string.
///
/// Returns the metric handle (≥ 0) on success, or -1 on error.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_new(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name` is a managed handle borrowed for this call.
    let name_str = unsafe { string_as_str(name) };
    // SAFETY: `help` is a managed handle borrowed for this call.
    let help_str = unsafe { string_as_str(help) };
    let Ok(gauge) = prometheus::Gauge::with_opts(prometheus::Opts::new(name_str, help_str)) else {
        return -1;
    };
    let mut reg = lock_or_recover(&registry.inner);
    if reg.registry.register(Box::new(gauge.clone())).is_err() {
        return -1;
    }
    push_handle(&mut reg.gauges, gauge, MetricKind::Gauge)
}

/// Register a labeled gauge vector.
///
/// `label_names` is a comma/newline/whitespace-delimited list of label names.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_new(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
    label_names: *const HewString,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name` is a managed handle borrowed for this call.
    let name_str = unsafe { string_as_str(name) };
    // SAFETY: `help` is a managed handle borrowed for this call.
    let help_str = unsafe { string_as_str(help) };
    // SAFETY: `label_names` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_names) };
    if labels.is_empty() {
        return -1;
    }
    let label_refs = label_refs(&labels);
    let Ok(gauge) =
        prometheus::GaugeVec::new(prometheus::Opts::new(name_str, help_str), &label_refs)
    else {
        return -1;
    };
    let mut reg = lock_or_recover(&registry.inner);
    if reg.registry.register(Box::new(gauge.clone())).is_err() {
        return -1;
    }
    push_handle(
        &mut reg.gauge_vecs,
        BoundedMetricVec::new(gauge, labels.len()),
        MetricKind::GaugeVec,
    )
}

/// Register a histogram with default buckets.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_new(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
) -> i64 {
    // SAFETY: forwards this function's own safety precondition unchanged to
    // `hew_metrics_histogram_with_buckets`.
    unsafe { hew_metrics_histogram_with_buckets(reg_handle, name, help, std::ptr::null()) }
}

/// Register a histogram with custom buckets, or defaults when `buckets` is null.
///
/// `buckets` is a comma/newline/whitespace-delimited list of finite f64 values.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle. An empty bucket list selects the default buckets.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_with_buckets(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
    buckets: *const HewString,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name` is a managed handle borrowed for this call.
    let name_str = unsafe { string_as_str(name) };
    // SAFETY: `help` is a managed handle borrowed for this call.
    let help_str = unsafe { string_as_str(help) };
    // SAFETY: `buckets` is a managed handle borrowed for this call.
    let Some(bucket_values) = (unsafe { parse_buckets(buckets) }) else {
        return -1;
    };
    let Ok(histogram) = prometheus::Histogram::with_opts(
        prometheus::HistogramOpts::new(name_str, help_str).buckets(bucket_values),
    ) else {
        return -1;
    };
    let mut reg = lock_or_recover(&registry.inner);
    if reg.registry.register(Box::new(histogram.clone())).is_err() {
        return -1;
    }
    push_handle(&mut reg.histograms, histogram, MetricKind::Histogram)
}

/// Register a labeled histogram vector with default buckets.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_vec_new(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
    label_names: *const HewString,
) -> i64 {
    // SAFETY: forwards this function's own safety precondition unchanged to
    // `hew_metrics_histogram_vec_with_buckets`.
    unsafe {
        hew_metrics_histogram_vec_with_buckets(reg_handle, name, help, label_names, std::ptr::null())
    }
}

/// Register a labeled histogram vector with custom buckets.
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle. An empty bucket list selects the default buckets.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_vec_with_buckets(
    reg_handle: i64,
    name: *const HewString,
    help: *const HewString,
    label_names: *const HewString,
    buckets: *const HewString,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name` is a managed handle borrowed for this call.
    let name_str = unsafe { string_as_str(name) };
    // SAFETY: `help` is a managed handle borrowed for this call.
    let help_str = unsafe { string_as_str(help) };
    // SAFETY: `label_names` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_names) };
    if labels.is_empty() {
        return -1;
    }
    // SAFETY: `buckets` is a managed handle borrowed for this call.
    let Some(bucket_values) = (unsafe { parse_buckets(buckets) }) else {
        return -1;
    };
    let label_refs = label_refs(&labels);
    let Ok(histogram) = prometheus::HistogramVec::new(
        prometheus::HistogramOpts::new(name_str, help_str).buckets(bucket_values),
        &label_refs,
    ) else {
        return -1;
    };
    let mut reg = lock_or_recover(&registry.inner);
    if reg.registry.register(Box::new(histogram.clone())).is_err() {
        return -1;
    }
    push_handle(
        &mut reg.histogram_vecs,
        BoundedMetricVec::new(histogram, labels.len()),
        MetricKind::HistogramVec,
    )
}

// ---------------------------------------------------------------------------
// Counter operations
// ---------------------------------------------------------------------------

/// Increment a counter by 1. Returns 0 on success, -1 on invalid input.
#[no_mangle]
pub extern "C" fn hew_metrics_counter_inc(reg_handle: i64, metric: i64) -> i32 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::Counter) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let reg = lock_or_recover(&registry.inner);
    let Some(c) = reg.counters.get(index) else {
        return -1;
    };
    c.inc();
    0
}

/// Add a non-negative, finite value to a counter. Returns 0 on success, -1 on invalid input.
#[no_mangle]
pub extern "C" fn hew_metrics_counter_add(reg_handle: i64, metric: i64, value: f64) -> i32 {
    if !value.is_finite() || value < 0.0 {
        return -1;
    }
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::Counter) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let reg = lock_or_recover(&registry.inner);
    let Some(c) = reg.counters.get(index) else {
        return -1;
    };
    c.inc_by(value);
    0
}

/// Increment a labeled counter by 1. Returns 0 on success, -1 on invalid input.
///
/// # Safety
///
/// `label_values` must be null (the empty string) or a live managed Hew
/// string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_vec_inc(
    reg_handle: i64,
    metric: i64,
    label_values: *const HewString,
) -> i32 {
    // SAFETY: forwards this function's own safety precondition unchanged to
    // `hew_metrics_counter_vec_add`.
    unsafe { hew_metrics_counter_vec_add(reg_handle, metric, label_values, 1.0) }
}

/// Add a non-negative, finite value to a labeled counter.
///
/// # Safety
///
/// `label_values` must be null (the empty string) or a live managed Hew
/// string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_vec_add(
    reg_handle: i64,
    metric: i64,
    label_values: *const HewString,
    value: f64,
) -> i32 {
    if !value.is_finite() || value < 0.0 {
        return -1;
    }
    // SAFETY: `label_values` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_values) };
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::CounterVec) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let mut reg = lock_or_recover(&registry.inner);
    let Some(counter) = reg.counter_vecs.get_mut(index) else {
        return STATUS_INVALID;
    };
    if let Err(status) = counter.admit(&labels) {
        return status;
    }
    let label_refs = label_refs(&labels);
    let Ok(c) = counter.metric.get_metric_with_label_values(&label_refs) else {
        return STATUS_INVALID;
    };
    c.inc_by(value);
    0
}

// ---------------------------------------------------------------------------
// Gauge operations
// ---------------------------------------------------------------------------

/// Set a gauge to an absolute finite value.
#[no_mangle]
pub extern "C" fn hew_metrics_gauge_set(reg_handle: i64, metric: i64, value: f64) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::Gauge) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let reg = lock_or_recover(&registry.inner);
    let Some(g) = reg.gauges.get(index) else {
        return -1;
    };
    g.set(value);
    0
}

/// Add a finite value to a gauge.
#[no_mangle]
pub extern "C" fn hew_metrics_gauge_add(reg_handle: i64, metric: i64, value: f64) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::Gauge) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let reg = lock_or_recover(&registry.inner);
    let Some(g) = reg.gauges.get(index) else {
        return -1;
    };
    g.add(value);
    0
}

/// Decrement a gauge by a finite value.
#[no_mangle]
pub extern "C" fn hew_metrics_gauge_dec(reg_handle: i64, metric: i64, value: f64) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::Gauge) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let reg = lock_or_recover(&registry.inner);
    let Some(g) = reg.gauges.get(index) else {
        return -1;
    };
    g.sub(value);
    0
}

/// Set a labeled gauge to an absolute finite value.
///
/// # Safety
///
/// `label_values` must be null (the empty string) or a live managed Hew
/// string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_set(
    reg_handle: i64,
    metric: i64,
    label_values: *const HewString,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_values) };
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::GaugeVec) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let mut reg = lock_or_recover(&registry.inner);
    let Some(gauge) = reg.gauge_vecs.get_mut(index) else {
        return STATUS_INVALID;
    };
    if let Err(status) = gauge.admit(&labels) {
        return status;
    }
    let label_refs = label_refs(&labels);
    let Ok(g) = gauge.metric.get_metric_with_label_values(&label_refs) else {
        return STATUS_INVALID;
    };
    g.set(value);
    0
}

/// Add a finite value to a labeled gauge.
///
/// # Safety
///
/// `label_values` must be null (the empty string) or a live managed Hew
/// string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_add(
    reg_handle: i64,
    metric: i64,
    label_values: *const HewString,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_values) };
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::GaugeVec) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let mut reg = lock_or_recover(&registry.inner);
    let Some(gauge) = reg.gauge_vecs.get_mut(index) else {
        return STATUS_INVALID;
    };
    if let Err(status) = gauge.admit(&labels) {
        return status;
    }
    let label_refs = label_refs(&labels);
    let Ok(g) = gauge.metric.get_metric_with_label_values(&label_refs) else {
        return STATUS_INVALID;
    };
    g.add(value);
    0
}

/// Decrement a labeled gauge by a finite value.
///
/// # Safety
///
/// `label_values` must be null (the empty string) or a live managed Hew
/// string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_dec(
    reg_handle: i64,
    metric: i64,
    label_values: *const HewString,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_values) };
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::GaugeVec) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let mut reg = lock_or_recover(&registry.inner);
    let Some(gauge) = reg.gauge_vecs.get_mut(index) else {
        return STATUS_INVALID;
    };
    if let Err(status) = gauge.admit(&labels) {
        return status;
    }
    let label_refs = label_refs(&labels);
    let Ok(g) = gauge.metric.get_metric_with_label_values(&label_refs) else {
        return STATUS_INVALID;
    };
    g.sub(value);
    0
}

// ---------------------------------------------------------------------------
// Histogram operations
// ---------------------------------------------------------------------------

/// Record a finite histogram observation.
#[no_mangle]
pub extern "C" fn hew_metrics_histogram_observe(reg_handle: i64, metric: i64, value: f64) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::Histogram) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let reg = lock_or_recover(&registry.inner);
    let Some(h) = reg.histograms.get(index) else {
        return -1;
    };
    h.observe(value);
    0
}

/// Record a finite labeled histogram observation.
///
/// # Safety
///
/// `label_values` must be null (the empty string) or a live managed Hew
/// string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_vec_observe(
    reg_handle: i64,
    metric: i64,
    label_values: *const HewString,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values` is a managed handle borrowed for this call.
    let labels = unsafe { parse_string_list(label_values) };
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    let index = match metric_index(metric, MetricKind::HistogramVec) {
        Ok(index) => index,
        Err(status) => return status,
    };
    let mut reg = lock_or_recover(&registry.inner);
    let Some(histogram) = reg.histogram_vecs.get_mut(index) else {
        return STATUS_INVALID;
    };
    if let Err(status) = histogram.admit(&labels) {
        return status;
    }
    let label_refs = label_refs(&labels);
    let Ok(h) = histogram.metric.get_metric_with_label_values(&label_refs) else {
        return STATUS_INVALID;
    };
    h.observe(value);
    0
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Export all metrics in Prometheus text format.
///
/// Returns an owned managed Hew string; a closed registry or an encoder
/// failure exports as the empty string.
#[no_mangle]
pub extern "C" fn hew_metrics_export(reg_handle: i64) -> *mut HewString {
    let Some(registry) = registry_for(reg_handle) else {
        return string_from_str("");
    };
    let reg = lock_or_recover(&registry.inner);
    let encoder = prometheus::TextEncoder::new();
    let metric_families = reg.registry.gather();
    let mut output = String::new();
    if encoder.encode_utf8(&metric_families, &mut output).is_err() {
        return string_from_str("");
    }
    string_from_str(&output)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hew_cabi::string::string_release;

    /// Allocate one managed string argument for a boundary call.
    fn managed(value: &str) -> *mut HewString {
        string_from_str(value)
    }

    /// Read and release the managed string a boundary call returned.
    unsafe fn owned_text(value: *mut HewString) -> String {
        // SAFETY: `value` is the owner a crate entry point just returned.
        let text = unsafe { string_as_str(value) }.to_owned();
        // SAFETY: the same owner, released exactly once here.
        unsafe { string_release(value) };
        text
    }

    /// Register a counter from Rust-side managed strings.
    fn counter(reg: i64, name: &str, help: &str) -> i64 {
        let name = managed(name);
        let help = managed(help);
        // SAFETY: both handles are live managed strings for the call.
        let handle = unsafe { hew_metrics_counter_new(reg, name, help) };
        // SAFETY: this test module owns both handles.
        unsafe {
            string_release(name);
            string_release(help);
        }
        handle
    }

    #[test]
    fn closed_registry_exports_the_empty_string() {
        let reg = hew_metrics_new();
        assert!(reg > 0);
        hew_metrics_close(reg);
        // SAFETY: the export owner is read and released once.
        assert_eq!(unsafe { owned_text(hew_metrics_export(reg)) }, "");
        // SAFETY: as above, for the never-registered handle.
        assert_eq!(unsafe { owned_text(hew_metrics_export(0)) }, "");
    }

    #[test]
    fn close_is_idempotent_for_absent_and_repeated_handles() {
        hew_metrics_close(0);
        hew_metrics_close(-1);
        let reg = hew_metrics_new();
        hew_metrics_close(reg);
        hew_metrics_close(reg);
        // SAFETY: the export owner is read and released once.
        assert_eq!(unsafe { owned_text(hew_metrics_export(reg)) }, "");
    }

    #[test]
    fn counter_round_trips_a_multibyte_help_string_through_the_export() {
        let reg = hew_metrics_new();
        let handle = counter(
            reg,
            "requetes_traitees_par_le_serveur_total",
            "Requêtes traitées — 雪",
        );
        assert!(handle >= 0);
        assert_eq!(hew_metrics_counter_inc(reg, handle), 0);
        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(
            output.contains("requetes_traitees_par_le_serveur_total 1"),
            "{output}"
        );
        assert!(output.contains("Requêtes traitées — 雪"), "{output}");
        hew_metrics_close(reg);
    }

    #[test]
    fn counter_add_rejects_negative_and_nan() {
        let reg = hew_metrics_new();
        let handle = counter(reg, "safe_add_counter", "Counter with checked add");
        assert!(handle >= 0);
        assert_eq!(hew_metrics_counter_add(reg, handle, 5.0), 0);
        assert_eq!(hew_metrics_counter_add(reg, handle, -1.0), -1);
        assert_eq!(hew_metrics_counter_add(reg, handle, f64::NAN), -1);
        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(output.contains("safe_add_counter 5"), "{output}");
        hew_metrics_close(reg);
    }

    #[test]
    fn gauge_set_add_and_dec_accumulate() {
        let reg = hew_metrics_new();
        let name = managed("test_gauge");
        let help = managed("A test gauge");
        // SAFETY: both handles are live managed strings for the call.
        let handle = unsafe { hew_metrics_gauge_new(reg, name, help) };
        // SAFETY: this test module owns both handles.
        unsafe {
            string_release(name);
            string_release(help);
        }
        assert!(handle >= 0);
        assert_eq!(hew_metrics_gauge_set(reg, handle, 100.0), 0);
        assert_eq!(hew_metrics_gauge_add(reg, handle, 50.0), 0);
        assert_eq!(hew_metrics_gauge_dec(reg, handle, 25.0), 0);
        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(output.contains("test_gauge 125"), "{output}");
        hew_metrics_close(reg);
    }

    #[test]
    fn histogram_uses_custom_buckets_and_defaults_when_empty() {
        let reg = hew_metrics_new();
        let name = managed("custom_histogram");
        let help = managed("A custom histogram");
        let buckets = managed("0.1,0.5,1.0");
        // SAFETY: all three handles are live managed strings for the call.
        let handle = unsafe { hew_metrics_histogram_with_buckets(reg, name, help, buckets) };
        assert!(handle >= 0);
        assert_eq!(hew_metrics_histogram_observe(reg, handle, 0.42), 0);

        let default_name = managed("default_histogram");
        // SAFETY: both handles are live managed strings for the call.
        let defaulted = unsafe { hew_metrics_histogram_new(reg, default_name, help) };
        assert!(defaulted >= 0);
        assert_eq!(hew_metrics_histogram_observe(reg, defaulted, 0.42), 0);

        let invalid_name = managed("invalid_histogram");
        let invalid_buckets = managed("0.1,not-a-number");
        // SAFETY: all three handles are live managed strings for the call.
        let rejected =
            unsafe { hew_metrics_histogram_with_buckets(reg, invalid_name, help, invalid_buckets) };
        assert_eq!(rejected, -1);

        // SAFETY: this test module owns every handle allocated above.
        unsafe {
            string_release(name);
            string_release(help);
            string_release(buckets);
            string_release(default_name);
            string_release(invalid_name);
            string_release(invalid_buckets);
        }

        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(
            output.contains("custom_histogram_bucket{le=\"0.5\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("default_histogram_bucket{le=\"0.5\"} 1"),
            "{output}"
        );
        assert!(!output.contains("invalid_histogram"), "{output}");
        hew_metrics_close(reg);
    }

    #[test]
    fn labeled_counter_gauge_and_histogram_export_their_series() {
        let reg = hew_metrics_new();
        let labels = managed("method,status");
        let values = managed("GET,200");
        let counter_name = managed("labeled_requests_total");
        let counter_help = managed("Labeled requests");
        let gauge_name = managed("labeled_inflight");
        let gauge_help = managed("Labeled inflight");
        let histogram_name = managed("labeled_latency_seconds");
        let histogram_help = managed("Labeled latency");
        let buckets = managed("0.1,1.0");

        // SAFETY: every handle below is a live managed string for its call.
        unsafe {
            let counter = hew_metrics_counter_vec_new(reg, counter_name, counter_help, labels);
            assert!(counter >= 0);
            assert_eq!(hew_metrics_counter_vec_add(reg, counter, values, 3.0), 0);

            let gauge = hew_metrics_gauge_vec_new(reg, gauge_name, gauge_help, labels);
            assert!(gauge >= 0);
            assert_eq!(hew_metrics_gauge_vec_set(reg, gauge, values, 8.0), 0);
            assert_eq!(hew_metrics_gauge_vec_dec(reg, gauge, values, 2.0), 0);

            let histogram = hew_metrics_histogram_vec_with_buckets(
                reg,
                histogram_name,
                histogram_help,
                labels,
                buckets,
            );
            assert!(histogram >= 0);
            assert_eq!(
                hew_metrics_histogram_vec_observe(reg, histogram, values, 0.2),
                0
            );
        }

        // SAFETY: this test module owns every handle allocated above.
        unsafe {
            string_release(labels);
            string_release(values);
            string_release(counter_name);
            string_release(counter_help);
            string_release(gauge_name);
            string_release(gauge_help);
            string_release(histogram_name);
            string_release(histogram_help);
            string_release(buckets);
        }

        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(
            output.contains("labeled_requests_total{method=\"GET\",status=\"200\"} 3"),
            "{output}"
        );
        assert!(
            output.contains("labeled_inflight{method=\"GET\",status=\"200\"} 6"),
            "{output}"
        );
        assert!(
            output.contains(
                "labeled_latency_seconds_bucket{method=\"GET\",status=\"200\",le=\"1\"} 1"
            ),
            "{output}"
        );
        hew_metrics_close(reg);
    }

    #[test]
    fn labeled_metric_rejects_new_series_at_limit() {
        let reg = hew_metrics_new();
        let name = managed("bounded_requests_total");
        let help = managed("Bounded requests");
        let labels = managed("route");
        // SAFETY: all three handles are live managed strings for the call.
        let metric = unsafe { hew_metrics_counter_vec_new(reg, name, help, labels) };
        assert!(metric >= 0);

        for index in 0..MAX_SERIES_PER_METRIC {
            let value = managed(&index.to_string());
            // SAFETY: `value` is a live managed string for the call.
            assert_eq!(unsafe { hew_metrics_counter_vec_inc(reg, metric, value) }, 0);
            // SAFETY: this test module owns `value`.
            unsafe { string_release(value) };
        }

        let overflow = managed("overflow");
        let existing = managed("0");
        // SAFETY: both handles are live managed strings for their calls.
        unsafe {
            assert_eq!(
                hew_metrics_counter_vec_inc(reg, metric, overflow),
                STATUS_SERIES_LIMIT
            );
            assert_eq!(hew_metrics_counter_vec_inc(reg, metric, existing), 0);
        }
        // SAFETY: this test module owns every handle allocated above.
        unsafe {
            string_release(name);
            string_release(help);
            string_release(labels);
            string_release(overflow);
            string_release(existing);
        }
        hew_metrics_close(reg);
    }

    #[test]
    fn labeled_registration_rejects_an_empty_label_list() {
        let reg = hew_metrics_new();
        let name = managed("unlabeled_vec_total");
        let help = managed("Missing labels");
        // SAFETY: both handles are live; the empty label list is the null string.
        let metric = unsafe { hew_metrics_counter_vec_new(reg, name, help, std::ptr::null()) };
        assert_eq!(metric, -1);
        // SAFETY: this test module owns both handles.
        unsafe {
            string_release(name);
            string_release(help);
        }
        hew_metrics_close(reg);
    }

    #[test]
    fn invalid_metric_handles_are_rejected() {
        let reg = hew_metrics_new();
        assert_eq!(hew_metrics_counter_inc(reg, -1), -1);
        assert_eq!(hew_metrics_counter_inc(reg, 6000), -1);
        assert_eq!(hew_metrics_gauge_set(reg, -1, 1.0), -1);
        assert_eq!(hew_metrics_gauge_add(reg, 6001, 1.0), -1);
        assert_eq!(hew_metrics_histogram_observe(reg, -1, 0.5), -1);
        assert_eq!(hew_metrics_histogram_observe(reg, 6002, 0.5), -1);
        hew_metrics_close(reg);
    }

    #[test]
    fn registration_against_an_absent_registry_is_rejected() {
        assert_eq!(counter(0, "x", "x"), -1);
    }

    #[test]
    fn embedded_nul_metric_name_is_not_truncated() {
        let reg = hew_metrics_new();
        assert_eq!(counter(reg, "valid_name\0invalid", "help"), -1);
        hew_metrics_close(reg);
    }

    #[test]
    fn metric_kinds_reject_another_kind_s_operations() {
        let reg = hew_metrics_new();
        let counter_handle = counter(reg, "kind_counter_total", "Kind tagged metric");
        let name = managed("kind_gauge");
        let help = managed("Kind tagged metric");
        // SAFETY: both handles are live managed strings for the call.
        let gauge = unsafe { hew_metrics_gauge_new(reg, name, help) };
        // SAFETY: this test module owns both handles.
        unsafe {
            string_release(name);
            string_release(help);
        }

        assert_ne!(counter_handle, gauge);
        assert_eq!(hew_metrics_gauge_set(reg, gauge, 5.0), 0);
        assert_eq!(
            hew_metrics_gauge_set(reg, counter_handle, 99.0),
            STATUS_KIND_MISMATCH
        );
        assert_eq!(hew_metrics_counter_inc(reg, gauge), STATUS_KIND_MISMATCH);

        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(output.contains("kind_gauge 5"), "{output}");
        assert!(!output.contains("kind_gauge 99"), "{output}");
        hew_metrics_close(reg);
    }

    #[test]
    fn registries_hold_independent_metrics() {
        let reg = hew_metrics_new();
        let requests = counter(reg, "requests_total", "Total requests");
        assert_eq!(requests, MetricKind::Counter as i64);
        let name = managed("memory_bytes");
        let help = managed("Memory bytes");
        // SAFETY: both handles are live managed strings for the call.
        let memory = unsafe { hew_metrics_gauge_new(reg, name, help) };
        // SAFETY: this test module owns both handles.
        unsafe {
            string_release(name);
            string_release(help);
        }
        assert_eq!(memory, MetricKind::Gauge as i64);

        assert_eq!(hew_metrics_counter_inc(reg, requests), 0);
        assert_eq!(hew_metrics_counter_inc(reg, requests), 0);
        assert_eq!(hew_metrics_gauge_set(reg, memory, 4096.0), 0);

        // SAFETY: the export owner is read and released once.
        let output = unsafe { owned_text(hew_metrics_export(reg)) };
        assert!(output.contains("requests_total 2"), "{output}");
        assert!(output.contains("memory_bytes 4096"), "{output}");
        hew_metrics_close(reg);
    }
}
