//! Hew runtime: Prometheus-compatible metrics.
//!
//! Wraps the `prometheus` crate to provide counters, gauges, and histograms
//! for compiled Hew programs. Registry handles are opaque process-local `i64`
//! ids. A return of -1 indicates failure.
//!
//! Returned strings are allocation-base, NUL-terminated `libc::malloc`
//! buffers. Hew takes ownership of them and releases the allocation base.

use std::{
    collections::{HashMap, HashSet},
    os::raw::c_char,
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

fn str_to_malloc(value: &str) -> *mut c_char {
    if value.as_bytes().contains(&0) {
        return std::ptr::null_mut();
    }
    // SAFETY: `value.len() + 1` is a nonzero size; `malloc` returns either a
    // suitably-sized allocation or null, and the null case is checked below.
    let output = unsafe { libc::malloc(value.len() + 1) }.cast::<u8>();
    if output.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `output` was just malloc'd with room for `value.len() + 1` bytes
    // and checked non-null above; `value.as_ptr()` is valid for `value.len()`
    // bytes and does not overlap the fresh `output` allocation.
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), output, value.len());
        output.add(value.len()).write(0);
    }
    output.cast::<c_char>()
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

unsafe fn c_string(ptr: *const c_char, len: i64) -> Option<String> {
    let len = usize::try_from(len).ok()?;
    if ptr.is_null() {
        return (len == 0).then(String::new);
    }
    // SAFETY: the caller of this function (see its `# Safety` doc) guarantees
    // `ptr` addresses `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    std::str::from_utf8(bytes).ok().map(ToOwned::to_owned)
}

unsafe fn parse_string_list(ptr: *const c_char, len: i64) -> Option<Vec<String>> {
    // SAFETY: forwards this function's own safety precondition on `ptr`/`len`
    // unchanged to `c_string`.
    let s = unsafe { c_string(ptr, len) }?;
    Some(
        s.split([',', '\n', '\r', '\t', ' '])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

unsafe fn parse_buckets(ptr: *const c_char, len: i64) -> Option<Vec<f64>> {
    // SAFETY: forwards this function's own safety precondition on `ptr`/`len`
    // unchanged to `parse_string_list`.
    let values = unsafe { parse_string_list(ptr, len) }?;
    let mut buckets = Vec::with_capacity(values.len());
    for value in values {
        let Ok(bucket) = value.parse::<f64>() else {
            return None;
        };
        if !bucket.is_finite() {
            return None;
        }
        buckets.push(bucket);
    }
    if buckets.is_empty() {
        return None;
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
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_new(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name`/`name_len` satisfy this function's documented invariant.
    let Some(name_str) = (unsafe { c_string(name, name_len) }) else {
        return -1;
    };
    // SAFETY: `help`/`help_len` satisfy this function's documented invariant.
    let Some(help_str) = (unsafe { c_string(help, help_len) }) else {
        return -1;
    };
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
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_vec_new(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
    label_names: *const c_char,
    label_names_len: i64,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name`/`name_len` satisfy this function's documented invariant.
    let Some(name_str) = (unsafe { c_string(name, name_len) }) else {
        return -1;
    };
    // SAFETY: `help`/`help_len` satisfy this function's documented invariant.
    let Some(help_str) = (unsafe { c_string(help, help_len) }) else {
        return -1;
    };
    // SAFETY: `label_names`/`label_names_len` satisfy this function's documented
    // invariant.
    let Some(labels) = (unsafe { parse_string_list(label_names, label_names_len) }) else {
        return -1;
    };
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
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_new(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name`/`name_len` satisfy this function's documented invariant.
    let Some(name_str) = (unsafe { c_string(name, name_len) }) else {
        return -1;
    };
    // SAFETY: `help`/`help_len` satisfy this function's documented invariant.
    let Some(help_str) = (unsafe { c_string(help, help_len) }) else {
        return -1;
    };
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
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_new(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
    label_names: *const c_char,
    label_names_len: i64,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name`/`name_len` satisfy this function's documented invariant.
    let Some(name_str) = (unsafe { c_string(name, name_len) }) else {
        return -1;
    };
    // SAFETY: `help`/`help_len` satisfy this function's documented invariant.
    let Some(help_str) = (unsafe { c_string(help, help_len) }) else {
        return -1;
    };
    // SAFETY: `label_names`/`label_names_len` satisfy this function's documented
    // invariant.
    let Some(labels) = (unsafe { parse_string_list(label_names, label_names_len) }) else {
        return -1;
    };
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
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_new(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
) -> i64 {
    // SAFETY: forwards this function's own safety precondition unchanged to
    // `hew_metrics_histogram_with_buckets`.
    unsafe {
        hew_metrics_histogram_with_buckets(
            reg_handle,
            name,
            name_len,
            help,
            help_len,
            std::ptr::null(),
            0,
        )
    }
}

/// Register a histogram with custom buckets, or defaults when `buckets` is null.
///
/// `buckets` is a comma/newline/whitespace-delimited list of finite f64 values.
///
/// # Safety
///
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8. `buckets` may be null only when `buckets_len` is
/// zero; other pointers may be null only with a zero paired length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_with_buckets(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
    buckets: *const c_char,
    buckets_len: i64,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name`/`name_len` satisfy this function's documented invariant.
    let Some(name_str) = (unsafe { c_string(name, name_len) }) else {
        return -1;
    };
    // SAFETY: `help`/`help_len` satisfy this function's documented invariant.
    let Some(help_str) = (unsafe { c_string(help, help_len) }) else {
        return -1;
    };
    let bucket_values = if buckets.is_null() {
        prometheus::DEFAULT_BUCKETS.to_vec()
    } else {
        // SAFETY: `buckets` is non-null here (checked above) and `buckets_len`
        // pairs with it per this function's documented invariant.
        let Some(parsed) = (unsafe { parse_buckets(buckets, buckets_len) }) else {
            return -1;
        };
        parsed
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
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_vec_new(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
    label_names: *const c_char,
    label_names_len: i64,
) -> i64 {
    // SAFETY: forwards this function's own safety precondition unchanged to
    // `hew_metrics_histogram_vec_with_buckets`.
    unsafe {
        hew_metrics_histogram_vec_with_buckets(
            reg_handle,
            name,
            name_len,
            help,
            help_len,
            label_names,
            label_names_len,
            std::ptr::null(),
            0,
        )
    }
}

/// Register a labeled histogram vector with custom buckets.
///
/// # Safety
///
/// Each string pointer must address its paired length in readable bytes
/// containing valid UTF-8. `buckets` may be null only when `buckets_len` is
/// zero; other pointers may be null only with a zero paired length.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_vec_with_buckets(
    reg_handle: i64,
    name: *const c_char,
    name_len: i64,
    help: *const c_char,
    help_len: i64,
    label_names: *const c_char,
    label_names_len: i64,
    buckets: *const c_char,
    buckets_len: i64,
) -> i64 {
    let Some(registry) = registry_for(reg_handle) else {
        return -1;
    };
    // SAFETY: `name`/`name_len` satisfy this function's documented invariant.
    let Some(name_str) = (unsafe { c_string(name, name_len) }) else {
        return -1;
    };
    // SAFETY: `help`/`help_len` satisfy this function's documented invariant.
    let Some(help_str) = (unsafe { c_string(help, help_len) }) else {
        return -1;
    };
    // SAFETY: `label_names`/`label_names_len` satisfy this function's documented
    // invariant.
    let Some(labels) = (unsafe { parse_string_list(label_names, label_names_len) }) else {
        return -1;
    };
    if labels.is_empty() {
        return -1;
    }
    let bucket_values = if buckets.is_null() {
        prometheus::DEFAULT_BUCKETS.to_vec()
    } else {
        // SAFETY: `buckets` is non-null here (checked above) and `buckets_len`
        // pairs with it per this function's documented invariant.
        let Some(parsed) = (unsafe { parse_buckets(buckets, buckets_len) }) else {
            return -1;
        };
        parsed
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
/// `label_values` must address `label_values_len` readable bytes containing
/// valid UTF-8; null is allowed only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_vec_inc(
    reg_handle: i64,
    metric: i64,
    label_values: *const c_char,
    label_values_len: i64,
) -> i32 {
    // SAFETY: forwards this function's own safety precondition unchanged to
    // `hew_metrics_counter_vec_add`.
    unsafe { hew_metrics_counter_vec_add(reg_handle, metric, label_values, label_values_len, 1.0) }
}

/// Add a non-negative, finite value to a labeled counter.
///
/// # Safety
///
/// `label_values` must address `label_values_len` readable bytes containing
/// valid UTF-8; null is allowed only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_counter_vec_add(
    reg_handle: i64,
    metric: i64,
    label_values: *const c_char,
    label_values_len: i64,
    value: f64,
) -> i32 {
    if !value.is_finite() || value < 0.0 {
        return -1;
    }
    // SAFETY: `label_values`/`label_values_len` satisfy this function's
    // documented invariant.
    let Some(labels) = (unsafe { parse_string_list(label_values, label_values_len) }) else {
        return -1;
    };
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
/// `label_values` must address `label_values_len` readable bytes containing
/// valid UTF-8; null is allowed only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_set(
    reg_handle: i64,
    metric: i64,
    label_values: *const c_char,
    label_values_len: i64,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values`/`label_values_len` satisfy this function's
    // documented invariant.
    let Some(labels) = (unsafe { parse_string_list(label_values, label_values_len) }) else {
        return -1;
    };
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
/// `label_values` must address `label_values_len` readable bytes containing
/// valid UTF-8; null is allowed only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_add(
    reg_handle: i64,
    metric: i64,
    label_values: *const c_char,
    label_values_len: i64,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values`/`label_values_len` satisfy this function's
    // documented invariant.
    let Some(labels) = (unsafe { parse_string_list(label_values, label_values_len) }) else {
        return -1;
    };
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
/// `label_values` must address `label_values_len` readable bytes containing
/// valid UTF-8; null is allowed only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_gauge_vec_dec(
    reg_handle: i64,
    metric: i64,
    label_values: *const c_char,
    label_values_len: i64,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values`/`label_values_len` satisfy this function's
    // documented invariant.
    let Some(labels) = (unsafe { parse_string_list(label_values, label_values_len) }) else {
        return -1;
    };
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
/// `label_values` must address `label_values_len` readable bytes containing
/// valid UTF-8; null is allowed only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_metrics_histogram_vec_observe(
    reg_handle: i64,
    metric: i64,
    label_values: *const c_char,
    label_values_len: i64,
    value: f64,
) -> i32 {
    if !value.is_finite() {
        return -1;
    }
    // SAFETY: `label_values`/`label_values_len` satisfy this function's
    // documented invariant.
    let Some(labels) = (unsafe { parse_string_list(label_values, label_values_len) }) else {
        return -1;
    };
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
/// Returns an allocation-base, NUL-terminated `libc::malloc` buffer, or null
/// on error. Hew takes ownership and releases that allocation base.
#[no_mangle]
pub extern "C" fn hew_metrics_export(reg_handle: i64) -> *mut c_char {
    let Some(registry) = registry_for(reg_handle) else {
        return std::ptr::null_mut();
    };
    let reg = lock_or_recover(&registry.inner);
    let encoder = prometheus::TextEncoder::new();
    let metric_families = reg.registry.gather();
    let mut output = String::new();
    if encoder.encode_utf8(&metric_families, &mut output).is_err() {
        return std::ptr::null_mut();
    }
    str_to_malloc(&output)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn cstr(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        // SAFETY: `ptr` is non-null (asserted above) and is a NUL-terminated
        // `libc::malloc` buffer produced by this crate's own FFI functions.
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `ptr` is the same allocation-base pointer validated and read via
        // `CStr::from_ptr` immediately above; freed exactly once here.
        unsafe { libc::free(ptr.cast()) };
        s
    }

    #[test]
    fn test_new_and_close() {
        let reg = hew_metrics_new();
        assert!(reg > 0);
        hew_metrics_close(reg);
        assert!(hew_metrics_export(reg).is_null());
    }

    #[test]
    fn test_close_null_and_repeated_are_noop() {
        hew_metrics_close(0);
        hew_metrics_close(-1);
        let reg = hew_metrics_new();
        hew_metrics_close(reg);
        hew_metrics_close(reg);
        assert!(hew_metrics_export(reg).is_null());
    }

    #[test]
    fn test_counter_basic() {
        // SAFETY: all pointers below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();
            let name = std::ffi::CString::new("test_counter").unwrap();
            let help = std::ffi::CString::new("A test counter").unwrap();
            let handle = hew_metrics_counter_new(
                reg,
                name.as_ptr(),
                i64::try_from(name.as_bytes().len()).unwrap(),
                help.as_ptr(),
                i64::try_from(help.as_bytes().len()).unwrap(),
            );
            assert!(handle >= 0);

            assert_eq!(hew_metrics_counter_inc(reg, handle), 0);

            let s = cstr(hew_metrics_export(reg));
            assert!(s.contains("test_counter"));
            hew_metrics_close(reg);
        }
    }

    #[test]
    fn test_counter_add_rejects_negative_and_nan() {
        // SAFETY: all pointers below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();
            let name = std::ffi::CString::new("safe_add_counter").unwrap();
            let help = std::ffi::CString::new("Counter with checked add").unwrap();
            let handle = hew_metrics_counter_new(
                reg,
                name.as_ptr(),
                i64::try_from(name.as_bytes().len()).unwrap(),
                help.as_ptr(),
                i64::try_from(help.as_bytes().len()).unwrap(),
            );
            assert!(handle >= 0);

            assert_eq!(hew_metrics_counter_add(reg, handle, 5.0), 0);
            assert_eq!(hew_metrics_counter_add(reg, handle, -1.0), -1);
            assert_eq!(hew_metrics_counter_add(reg, handle, f64::NAN), -1);

            let s = cstr(hew_metrics_export(reg));
            assert!(s.contains("safe_add_counter") && s.contains('5'));
            assert!(!s.contains("-1"));
            hew_metrics_close(reg);
        }
    }

    #[test]
    fn test_gauge_set_add_and_dec() {
        // SAFETY: all pointers below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();
            let name = std::ffi::CString::new("test_gauge").unwrap();
            let help = std::ffi::CString::new("A test gauge").unwrap();
            let handle = hew_metrics_gauge_new(
                reg,
                name.as_ptr(),
                i64::try_from(name.as_bytes().len()).unwrap(),
                help.as_ptr(),
                i64::try_from(help.as_bytes().len()).unwrap(),
            );
            assert!(handle >= 0);

            assert_eq!(hew_metrics_gauge_set(reg, handle, 100.0), 0);
            assert_eq!(hew_metrics_gauge_add(reg, handle, 50.0), 0);
            assert_eq!(hew_metrics_gauge_dec(reg, handle, 25.0), 0);

            let s = cstr(hew_metrics_export(reg));
            assert!(s.contains("test_gauge"));
            assert!(s.contains("125"));
            hew_metrics_close(reg);
        }
    }

    #[test]
    fn test_histogram_custom_buckets() {
        // SAFETY: all pointers below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();
            let name = std::ffi::CString::new("custom_histogram").unwrap();
            let help = std::ffi::CString::new("A custom histogram").unwrap();
            let buckets = std::ffi::CString::new("0.1,0.5,1.0").unwrap();
            let handle = hew_metrics_histogram_with_buckets(
                reg,
                name.as_ptr(),
                i64::try_from(name.as_bytes().len()).unwrap(),
                help.as_ptr(),
                i64::try_from(help.as_bytes().len()).unwrap(),
                buckets.as_ptr(),
                i64::try_from(buckets.as_bytes().len()).unwrap(),
            );
            assert!(handle >= 0);

            assert_eq!(hew_metrics_histogram_observe(reg, handle, 0.42), 0);

            let s = cstr(hew_metrics_export(reg));
            assert!(s.contains("custom_histogram_bucket{le=\"0.5\"} 1"));
            hew_metrics_close(reg);
        }
    }

    #[test]
    fn test_labeled_counter_gauge_and_histogram() {
        // SAFETY: all pointers below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();
            let labels = std::ffi::CString::new("method,status").unwrap();
            let values = std::ffi::CString::new("GET,200").unwrap();

            let cn = std::ffi::CString::new("labeled_requests_total").unwrap();
            let ch = std::ffi::CString::new("Labeled requests").unwrap();
            let c = hew_metrics_counter_vec_new(
                reg,
                cn.as_ptr(),
                i64::try_from(cn.as_bytes().len()).unwrap(),
                ch.as_ptr(),
                i64::try_from(ch.as_bytes().len()).unwrap(),
                labels.as_ptr(),
                i64::try_from(labels.as_bytes().len()).unwrap(),
            );
            assert!(c >= 0);
            assert_eq!(
                hew_metrics_counter_vec_add(
                    reg,
                    c,
                    values.as_ptr(),
                    i64::try_from(values.as_bytes().len()).unwrap(),
                    3.0
                ),
                0
            );

            let gn = std::ffi::CString::new("labeled_inflight").unwrap();
            let gh = std::ffi::CString::new("Labeled inflight").unwrap();
            let g = hew_metrics_gauge_vec_new(
                reg,
                gn.as_ptr(),
                i64::try_from(gn.as_bytes().len()).unwrap(),
                gh.as_ptr(),
                i64::try_from(gh.as_bytes().len()).unwrap(),
                labels.as_ptr(),
                i64::try_from(labels.as_bytes().len()).unwrap(),
            );
            assert!(g >= 0);
            assert_eq!(
                hew_metrics_gauge_vec_set(
                    reg,
                    g,
                    values.as_ptr(),
                    i64::try_from(values.as_bytes().len()).unwrap(),
                    8.0
                ),
                0
            );
            assert_eq!(
                hew_metrics_gauge_vec_dec(
                    reg,
                    g,
                    values.as_ptr(),
                    i64::try_from(values.as_bytes().len()).unwrap(),
                    2.0
                ),
                0
            );

            let hn = std::ffi::CString::new("labeled_latency_seconds").unwrap();
            let hh = std::ffi::CString::new("Labeled latency").unwrap();
            let buckets = std::ffi::CString::new("0.1,1.0").unwrap();
            let h = hew_metrics_histogram_vec_with_buckets(
                reg,
                hn.as_ptr(),
                i64::try_from(hn.as_bytes().len()).unwrap(),
                hh.as_ptr(),
                i64::try_from(hh.as_bytes().len()).unwrap(),
                labels.as_ptr(),
                i64::try_from(labels.as_bytes().len()).unwrap(),
                buckets.as_ptr(),
                i64::try_from(buckets.as_bytes().len()).unwrap(),
            );
            assert!(h >= 0);
            assert_eq!(
                hew_metrics_histogram_vec_observe(
                    reg,
                    h,
                    values.as_ptr(),
                    i64::try_from(values.as_bytes().len()).unwrap(),
                    0.2
                ),
                0
            );

            let s = cstr(hew_metrics_export(reg));
            assert!(s.contains("labeled_requests_total{method=\"GET\",status=\"200\"} 3"));
            assert!(s.contains("labeled_inflight{method=\"GET\",status=\"200\"} 6"));
            assert!(s.contains(
                "labeled_latency_seconds_bucket{method=\"GET\",status=\"200\",le=\"1\"} 1"
            ));
            hew_metrics_close(reg);
        }
    }

    #[test]
    fn labeled_metric_rejects_new_series_at_limit() {
        // SAFETY: all pointers below are backed by live byte-slice locals (`b"..."`)
        // paired with their own length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();
            let name = b"bounded_requests_total";
            let help = b"Bounded requests";
            let labels = b"route";
            let metric = hew_metrics_counter_vec_new(
                reg,
                name.as_ptr().cast(),
                i64::try_from(name.len()).unwrap(),
                help.as_ptr().cast(),
                i64::try_from(help.len()).unwrap(),
                labels.as_ptr().cast(),
                i64::try_from(labels.len()).unwrap(),
            );
            assert!(metric >= 0);

            for index in 0..MAX_SERIES_PER_METRIC {
                let value = index.to_string();
                assert_eq!(
                    hew_metrics_counter_vec_inc(
                        reg,
                        metric,
                        value.as_ptr().cast(),
                        i64::try_from(value.len()).unwrap(),
                    ),
                    0
                );
            }

            let overflow = b"overflow";
            assert_eq!(
                hew_metrics_counter_vec_inc(
                    reg,
                    metric,
                    overflow.as_ptr().cast(),
                    i64::try_from(overflow.len()).unwrap(),
                ),
                STATUS_SERIES_LIMIT
            );
            let existing = b"0";
            assert_eq!(
                hew_metrics_counter_vec_inc(
                    reg,
                    metric,
                    existing.as_ptr().cast(),
                    i64::try_from(existing.len()).unwrap(),
                ),
                0
            );
            hew_metrics_close(reg);
        }
    }

    #[test]
    fn test_invalid_handle_is_noop() {
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
    fn test_null_reg_counter_returns_minus_one() {
        // SAFETY: `name`/`help` below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariant.
        unsafe {
            let name = std::ffi::CString::new("x").unwrap();
            let help = std::ffi::CString::new("x").unwrap();
            let h = hew_metrics_counter_new(
                0,
                name.as_ptr(),
                i64::try_from(name.as_bytes().len()).unwrap(),
                help.as_ptr(),
                i64::try_from(help.as_bytes().len()).unwrap(),
            );
            assert_eq!(h, -1);
        }
    }

    #[test]
    fn embedded_nul_metric_name_is_not_truncated() {
        let reg = hew_metrics_new();
        let name = b"valid_name\0invalid";
        let help = b"help";
        // SAFETY: `name`/`help` below are backed by live byte-slice locals paired
        // with their own length, satisfying the callees' documented invariant.
        let handle = unsafe {
            hew_metrics_counter_new(
                reg,
                name.as_ptr().cast(),
                i64::try_from(name.len()).unwrap(),
                help.as_ptr().cast(),
                i64::try_from(help.len()).unwrap(),
            )
        };
        assert_eq!(handle, -1);
        hew_metrics_close(reg);
    }

    #[test]
    fn test_export_null_returns_null() {
        let ptr = hew_metrics_export(0);
        assert!(ptr.is_null());
    }

    #[test]
    fn test_multiple_metrics_independent() {
        // SAFETY: all pointers below are backed by live `CString` locals paired
        // with their own byte length, satisfying the callees' documented
        // invariants.
        unsafe {
            let reg = hew_metrics_new();

            let n1 = std::ffi::CString::new("requests_total").unwrap();
            let h1 = std::ffi::CString::new("Total requests").unwrap();
            let c_handle = hew_metrics_counter_new(
                reg,
                n1.as_ptr(),
                i64::try_from(n1.as_bytes().len()).unwrap(),
                h1.as_ptr(),
                i64::try_from(h1.as_bytes().len()).unwrap(),
            );
            assert_eq!(c_handle, MetricKind::Counter as i64);

            let n2 = std::ffi::CString::new("memory_bytes").unwrap();
            let h2 = std::ffi::CString::new("Memory bytes").unwrap();
            let g_handle = hew_metrics_gauge_new(
                reg,
                n2.as_ptr(),
                i64::try_from(n2.as_bytes().len()).unwrap(),
                h2.as_ptr(),
                i64::try_from(h2.as_bytes().len()).unwrap(),
            );
            assert_eq!(g_handle, MetricKind::Gauge as i64);

            assert_eq!(hew_metrics_counter_inc(reg, c_handle), 0);
            assert_eq!(hew_metrics_counter_inc(reg, c_handle), 0);
            assert_eq!(hew_metrics_gauge_set(reg, g_handle, 4096.0), 0);

            let s = cstr(hew_metrics_export(reg));
            assert!(s.contains("requests_total"));
            assert!(s.contains("memory_bytes"));
            assert!(s.contains('2'));
            assert!(s.contains("4096"));

            hew_metrics_close(reg);
        }
    }

    #[test]
    fn handles_reject_operations_for_another_metric_kind() {
        // SAFETY: all pointers below are backed by live byte-slice locals paired
        // with their own length, satisfying the callees' documented invariants.
        unsafe {
            let reg = hew_metrics_new();
            let counter_name = b"kind_counter_total";
            let gauge_name = b"kind_gauge";
            let help = b"Kind tagged metric";
            let counter = hew_metrics_counter_new(
                reg,
                counter_name.as_ptr().cast(),
                i64::try_from(counter_name.len()).unwrap(),
                help.as_ptr().cast(),
                i64::try_from(help.len()).unwrap(),
            );
            let gauge = hew_metrics_gauge_new(
                reg,
                gauge_name.as_ptr().cast(),
                i64::try_from(gauge_name.len()).unwrap(),
                help.as_ptr().cast(),
                i64::try_from(help.len()).unwrap(),
            );

            assert_ne!(counter, gauge);
            assert_eq!(hew_metrics_gauge_set(reg, gauge, 5.0), 0);
            assert_eq!(
                hew_metrics_gauge_set(reg, counter, 99.0),
                STATUS_KIND_MISMATCH
            );
            assert_eq!(hew_metrics_counter_inc(reg, gauge), STATUS_KIND_MISMATCH);

            let output = cstr(hew_metrics_export(reg));
            assert!(output.contains("kind_gauge 5"));
            assert!(!output.contains("kind_gauge 99"));
            hew_metrics_close(reg);
        }
    }
}
