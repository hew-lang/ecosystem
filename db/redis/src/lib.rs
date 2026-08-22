//! Native Redis support for `hew.db.redis`.
//!
//! Every text argument is received as a pointer plus an explicit byte length.
//! Native handles are registered, validated before dereference, and released
//! idempotently so actor cancellation and explicit close cannot leak or double
//! free them.

use redis::{Commands, ConnectionLike};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Return a string through Hew's foreign-package ABI.
///
/// Package `extern "C" -> string` results are adopted by Hew and released
/// with `libc::free`, so the returned pointer must be the allocation base.
fn malloc_c_string(value: &str) -> *mut c_char {
    let Some(size) = value.len().checked_add(1) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `size` includes the trailing NUL and is non-zero.
    let output = unsafe { libc::malloc(size) }.cast::<u8>();
    if output.is_null() {
        return output.cast();
    }
    // SAFETY: `output` names `size` writable bytes and does not overlap the
    // borrowed string payload.
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), output, value.len());
        output.add(value.len()).write(0);
    }
    output.cast()
}

/// Hew's C-ABI representation for an owned `bytes` value.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BytesTriple {
    ptr: *mut u8,
    offset: u32,
    len: u32,
}

fn empty_bytes() -> BytesTriple {
    BytesTriple {
        ptr: std::ptr::null_mut(),
        offset: 0,
        len: 0,
    }
}

fn owned_bytes(value: &[u8]) -> BytesTriple {
    if value.is_empty() {
        return empty_bytes();
    }
    let capacity = value.len().max(16);
    let Ok(capacity_u32) = u32::try_from(capacity) else {
        std::process::abort();
    };
    let Ok(value_len) = u32::try_from(value.len()) else {
        std::process::abort();
    };
    let Some(allocation_len) = capacity.checked_add(8) else {
        std::process::abort();
    };
    // SAFETY: `malloc` accepts any allocation size and the result is checked
    // before it is written or returned.
    let base = unsafe { libc::malloc(allocation_len) }.cast::<u8>();
    if base.is_null() {
        std::process::abort();
    }
    // SAFETY: `base` names an allocation of `8 + capacity` bytes. Header
    // writes use byte copies, so they do not impose an alignment requirement.
    unsafe {
        std::ptr::copy_nonoverlapping(1_u32.to_ne_bytes().as_ptr(), base, 4);
        std::ptr::copy_nonoverlapping(capacity_u32.to_ne_bytes().as_ptr(), base.add(4), 4);
        let data = base.add(8);
        std::ptr::copy_nonoverlapping(value.as_ptr(), data, value.len());
        BytesTriple {
            ptr: data,
            offset: 0,
            len: value_len,
        }
    }
}

unsafe fn bytes_arg<'a>(value: *const BytesTriple, what: &str) -> Option<&'a [u8]> {
    // SAFETY: the caller promises that a non-null pointer addresses a valid
    // Hew bytes triple for the duration of this call.
    let Some(value) = (unsafe { value.as_ref() }) else {
        set_error(
            ErrorKind::InvalidInput,
            format!("{what} bytes pointer is null"),
        );
        return None;
    };
    if value.len == 0 {
        return Some(&[]);
    }
    if value.ptr.is_null() {
        set_error(
            ErrorKind::InvalidInput,
            format!("{what} bytes data is null"),
        );
        return None;
    }
    // SAFETY: the Hew bytes ABI guarantees `ptr + offset` addresses at least
    // `len` initialized bytes; null pointers were rejected above.
    unsafe {
        Some(slice::from_raw_parts(
            value.ptr.add(value.offset as usize),
            value.len as usize,
        ))
    }
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    Connection = 1,
    InvalidInput = 2,
    Command = 3,
    Timeout = 4,
    Closed = 5,
}

#[derive(Debug)]
struct ErrorState {
    kind: ErrorKind,
    message: String,
}

thread_local! {
    static LAST_ERROR: RefCell<ErrorState> = const { RefCell::new(ErrorState {
        kind: ErrorKind::None,
        message: String::new(),
    }) };
}

fn clear_error() {
    LAST_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.kind = ErrorKind::None;
        state.message.clear();
    });
}

fn set_error(kind: ErrorKind, message: impl Into<String>) {
    LAST_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.kind = kind;
        state.message = message.into();
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_redis_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_redis_last_error() -> *mut c_char {
    LAST_ERROR.with(|state| malloc_c_string(&state.borrow().message))
}

unsafe fn utf8_with_len<'a>(value: *const c_char, len: i64, what: &str) -> Option<&'a str> {
    let Ok(len) = usize::try_from(len) else {
        set_error(
            ErrorKind::InvalidInput,
            format!("{what} length is negative"),
        );
        return None;
    };
    if value.is_null() {
        if len == 0 {
            return Some("");
        }
        set_error(ErrorKind::InvalidInput, format!("{what} pointer is null"));
        return None;
    }
    // SAFETY: the FFI caller provides `len` readable bytes at `value`; null is
    // handled above, including the valid empty-string representation.
    let bytes = unsafe { slice::from_raw_parts(value.cast::<u8>(), len) };
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) => {
            set_error(
                ErrorKind::InvalidInput,
                format!("{what} is not UTF-8: {error}"),
            );
            None
        }
    }
}

fn command_error(operation: &str, error: &redis::RedisError) {
    set_error(
        ErrorKind::Command,
        format!("Redis {operation} failed: {error}"),
    );
}

struct RedisConnection {
    inner: redis::Connection,
    url: String,
}

impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisConnection")
            .finish_non_exhaustive()
    }
}

type SharedRegistry<T> = OnceLock<Mutex<HashMap<i64, Arc<Mutex<T>>>>>;

static CONNECTIONS: SharedRegistry<RedisConnection> = OnceLock::new();
static PIPELINES: SharedRegistry<Pipeline> = OnceLock::new();
static STRING_RESULTS: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
static HASH_RESULTS: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
static SET_RESULTS: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
static NEXT_SHARED_HANDLE: AtomicI64 = AtomicI64::new(1);

fn shared_registry<T>(
    cell: &'static SharedRegistry<T>,
) -> &'static Mutex<HashMap<i64, Arc<Mutex<T>>>> {
    cell.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_shared<T>(value: T, cell: &'static SharedRegistry<T>) -> i64 {
    let handle = NEXT_SHARED_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        std::process::abort();
    }
    if let Ok(mut handles) = shared_registry(cell).lock() {
        handles.insert(handle, Arc::new(Mutex::new(value)));
        handle
    } else {
        set_error(
            ErrorKind::Connection,
            "Redis handle registry is unavailable",
        );
        0
    }
}

fn registered_shared<T>(
    handle: i64,
    cell: &'static SharedRegistry<T>,
    what: &str,
) -> Option<Arc<Mutex<T>>> {
    let handles = shared_registry(cell).lock().ok()?;
    let value = handles.get(&handle).cloned();
    drop(handles);
    if value.is_none() {
        set_error(ErrorKind::Closed, format!("{what} is closed"));
    }
    value
}

fn free_shared<T>(handle: i64, cell: &'static SharedRegistry<T>) {
    if handle == 0 {
        return;
    }
    if let Ok(mut handles) = shared_registry(cell).lock() {
        handles.remove(&handle);
    }
}

fn registry(cell: &'static OnceLock<Mutex<HashSet<i64>>>) -> &'static Mutex<HashSet<i64>> {
    cell.get_or_init(|| Mutex::new(HashSet::new()))
}

fn register<T>(value: T, cell: &'static OnceLock<Mutex<HashSet<i64>>>) -> i64 {
    let handle = Box::into_raw(Box::new(value)) as i64;
    if let Ok(mut handles) = registry(cell).lock() {
        handles.insert(handle);
        handle
    } else {
        // SAFETY: `handle` came directly from `Box::into_raw` above and was
        // not inserted into the registry, so this is its sole reclamation.
        unsafe { drop(Box::from_raw(handle as *mut T)) };
        set_error(
            ErrorKind::Connection,
            "Redis handle registry is unavailable",
        );
        0
    }
}

struct RegisteredMut<'a, T> {
    _registry: MutexGuard<'a, HashSet<i64>>,
    ptr: *mut T,
}

impl<T> Deref for RegisteredMut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: registration owns the allocation and `_registry` prevents
        // concurrent removal for the lifetime of this guard.
        unsafe { &*self.ptr }
    }
}

impl<T> DerefMut for RegisteredMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the exclusive registry guard serializes all access to this
        // registered allocation, so no other mutable reference can coexist.
        unsafe { &mut *self.ptr }
    }
}

fn registered_mut<T>(
    handle: i64,
    cell: &'static OnceLock<Mutex<HashSet<i64>>>,
    what: &str,
) -> Option<RegisteredMut<'static, T>> {
    let handles = registry(cell).lock().ok()?;
    if handle == 0 || !handles.contains(&handle) {
        set_error(ErrorKind::Closed, format!("{what} is closed"));
        return None;
    }
    Some(RegisteredMut {
        _registry: handles,
        ptr: handle as *mut T,
    })
}

fn free_registered<T>(handle: i64, cell: &'static OnceLock<Mutex<HashSet<i64>>>) {
    if handle == 0 {
        return;
    }
    let removed = registry(cell)
        .lock()
        .ok()
        .is_some_and(|mut handles| handles.remove(&handle));
    if removed {
        // SAFETY: successful removal proves this registered handle owns one
        // live `Box<T>` and prevents any concurrent guard from existing.
        unsafe { drop(Box::from_raw(handle as *mut T)) };
    }
}

fn connection(handle: i64) -> Option<Arc<Mutex<RedisConnection>>> {
    registered_shared(handle, &CONNECTIONS, "Redis connection")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_connect_len(url: *const c_char, url_len: i64) -> i64 {
    // SAFETY: the function contract requires `url` to expose `url_len`
    // readable bytes for this call.
    let Some(url) = (unsafe { utf8_with_len(url, url_len, "Redis URL") }) else {
        return 0;
    };
    let client = match redis::Client::open(url) {
        Ok(client) => client,
        Err(error) => {
            set_error(
                ErrorKind::InvalidInput,
                format!("invalid Redis URL: {error}"),
            );
            return 0;
        }
    };
    match client.get_connection() {
        Ok(inner) => {
            clear_error();
            register_shared(
                RedisConnection {
                    inner,
                    url: url.to_owned(),
                },
                &CONNECTIONS,
            )
        }
        Err(error) => {
            set_error(
                ErrorKind::Connection,
                format!("Redis connection failed: {error}"),
            );
            0
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_close(handle: i64) {
    free_shared(handle, &CONNECTIONS);
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_redis_connection_count() -> i64 {
    shared_registry(&CONNECTIONS)
        .lock()
        .ok()
        .and_then(|connections| i64::try_from(connections.len()).ok())
        .unwrap_or(-1)
}

macro_rules! text_arg {
    ($ptr:expr, $len:expr, $name:literal) => {
        // SAFETY: each enclosing FFI entry point requires the pointer/length
        // pair to describe readable memory for the duration of the call.
        match unsafe { utf8_with_len($ptr, $len, $name) } {
            Some(value) => value,
            None => return -1,
        }
    };
}

macro_rules! binary_arg {
    ($value:expr, $name:literal) => {
        // SAFETY: each enclosing FFI entry point requires the pointer to name
        // a valid Hew bytes triple for the duration of the call.
        match unsafe { bytes_arg($value, $name) } {
            Some(value) => value,
            None => return -1,
        }
    };
}

macro_rules! lock_connection {
    ($handle:expr, $shared:ident, $guard:ident, $error:expr) => {
        let Some($shared) = connection($handle) else {
            return $error;
        };
        let Ok(mut $guard) = $shared.lock() else {
            set_error(ErrorKind::Connection, "Redis connection is unavailable");
            return $error;
        };
    };
}

fn int_reply(result: redis::RedisResult<i64>, operation: &str) -> i64 {
    match result {
        Ok(value) => {
            clear_error();
            value
        }
        Err(error) => {
            command_error(operation, &error);
            -1
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_set_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    v: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let v = binary_arg!(v, "value");
    match c.inner.req_command(redis::cmd("SET").arg(k).arg(v)) {
        Ok(_) => {
            clear_error();
            0
        }
        Err(error) => {
            command_error("SET", &error);
            -1
        }
    }
}

#[derive(Debug)]
struct StringResult {
    status: i32,
    value: Option<Vec<u8>>,
}

fn string_reply(result: redis::RedisResult<Option<Vec<u8>>>, operation: &str) -> i64 {
    let reply = match result {
        Ok(Some(value)) => {
            clear_error();
            StringResult {
                status: 1,
                value: Some(value),
            }
        }
        Ok(None) => {
            clear_error();
            StringResult {
                status: 0,
                value: None,
            }
        }
        Err(error) => {
            command_error(operation, &error);
            StringResult {
                status: -1,
                value: None,
            }
        }
    };
    register(reply, &STRING_RESULTS)
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_get_len(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(
        c,
        shared_c,
        c,
        register(
            StringResult {
                status: -1,
                value: None,
            },
            &STRING_RESULTS,
        )
    );
    let k = text_arg!(k, kl, "key");
    string_reply(c.inner.get(k), "GET")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_del_len(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    int_reply(c.inner.del(k), "DEL")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_set_ex_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    v: *const BytesTriple,
    ttl: i64,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let v = binary_arg!(v, "value");
    if ttl <= 0 {
        set_error(ErrorKind::InvalidInput, "TTL must be positive");
        return -1;
    }
    match c
        .inner
        .req_command(redis::cmd("SETEX").arg(k).arg(ttl).arg(v))
    {
        Ok(_) => {
            clear_error();
            0
        }
        Err(e) => {
            command_error("SETEX", &e);
            -1
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_expire_len(c: i64, k: *const c_char, kl: i64, ttl: i64) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    if ttl <= 0 {
        set_error(ErrorKind::InvalidInput, "TTL must be positive");
        return -1;
    }
    int_reply(
        redis::cmd("EXPIRE").arg(k).arg(ttl).query(&mut c.inner),
        "EXPIRE",
    )
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_ttl_len(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(c, shared_c, c, -3);
    let k = text_arg!(k, kl, "key");
    match redis::cmd("TTL").arg(k).query(&mut c.inner) {
        Ok(v) => {
            clear_error();
            v
        }
        Err(e) => {
            command_error("TTL", &e);
            -3
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_incr_len(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(c, shared_c, c, 0);
    let k = text_arg!(k, kl, "key");
    match c.inner.incr(k, 1) {
        Ok(v) => {
            clear_error();
            v
        }
        Err(e) => {
            command_error("INCR", &e);
            0
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_lpush_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    v: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let v = binary_arg!(v, "value");
    int_reply(c.inner.lpush(k, v), "LPUSH")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_rpop_len(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(
        c,
        shared_c,
        c,
        register(
            StringResult {
                status: -1,
                value: None,
            },
            &STRING_RESULTS,
        )
    );
    let k = text_arg!(k, kl, "key");
    string_reply(c.inner.rpop(k, None), "RPOP")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hset_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    f: *const c_char,
    fl: i64,
    v: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let f = text_arg!(f, fl, "field");
    let v = binary_arg!(v, "value");
    int_reply(
        redis::cmd("HSET").arg(k).arg(f).arg(v).query(&mut c.inner),
        "HSET",
    )
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hget_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    f: *const c_char,
    fl: i64,
) -> i64 {
    lock_connection!(
        c,
        shared_c,
        c,
        register(
            StringResult {
                status: -1,
                value: None,
            },
            &STRING_RESULTS,
        )
    );
    let k = text_arg!(k, kl, "key");
    let f = text_arg!(f, fl, "field");
    string_reply(redis::cmd("HGET").arg(k).arg(f).query(&mut c.inner), "HGET")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hdel_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    f: *const c_char,
    fl: i64,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let f = text_arg!(f, fl, "field");
    int_reply(redis::cmd("HDEL").arg(k).arg(f).query(&mut c.inner), "HDEL")
}

#[derive(Debug)]
struct HashResult {
    entries: Vec<(String, Vec<u8>)>,
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hgetall_len_ffi(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(c, shared_c, c, 0);
    let k = text_arg!(k, kl, "key");
    match redis::cmd("HGETALL")
        .arg(k)
        .query::<Vec<(String, Vec<u8>)>>(&mut c.inner)
    {
        Ok(entries) => {
            clear_error();
            register(HashResult { entries }, &HASH_RESULTS)
        }
        Err(e) => {
            command_error("HGETALL", &e);
            0
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hgetall_count(h: i64) -> i64 {
    registered_mut::<HashResult>(h, &HASH_RESULTS, "hash result").map_or(-1, |result| {
        i64::try_from(result.entries.len()).unwrap_or(-1)
    })
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hgetall_key(h: i64, i: i64) -> *mut c_char {
    let Some(result) = registered_mut::<HashResult>(h, &HASH_RESULTS, "hash result") else {
        return malloc_c_string("");
    };
    usize::try_from(i)
        .ok()
        .and_then(|index| result.entries.get(index))
        .map_or_else(|| malloc_c_string(""), |entry| malloc_c_string(&entry.0))
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hgetall_value(h: i64, i: i64) -> BytesTriple {
    let Some(result) = registered_mut::<HashResult>(h, &HASH_RESULTS, "hash result") else {
        return empty_bytes();
    };
    usize::try_from(i)
        .ok()
        .and_then(|index| result.entries.get(index))
        .map_or_else(empty_bytes, |entry| owned_bytes(&entry.1))
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_hgetall_free(h: i64) {
    free_registered::<HashResult>(h, &HASH_RESULTS);
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_sadd_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    m: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let m = binary_arg!(m, "member");
    int_reply(redis::cmd("SADD").arg(k).arg(m).query(&mut c.inner), "SADD")
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_srem_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    m: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let m = binary_arg!(m, "member");
    int_reply(redis::cmd("SREM").arg(k).arg(m).query(&mut c.inner), "SREM")
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_sismember_len(
    c: i64,
    k: *const c_char,
    kl: i64,
    m: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let k = text_arg!(k, kl, "key");
    let m = binary_arg!(m, "member");
    int_reply(
        redis::cmd("SISMEMBER").arg(k).arg(m).query(&mut c.inner),
        "SISMEMBER",
    )
}

#[derive(Debug)]
struct SetResult {
    members: Vec<Vec<u8>>,
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_smembers_len_ffi(c: i64, k: *const c_char, kl: i64) -> i64 {
    lock_connection!(c, shared_c, c, 0);
    let k = text_arg!(k, kl, "key");
    match redis::cmd("SMEMBERS").arg(k).query(&mut c.inner) {
        Ok(members) => {
            clear_error();
            register(SetResult { members }, &SET_RESULTS)
        }
        Err(e) => {
            command_error("SMEMBERS", &e);
            0
        }
    }
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_smembers_count(h: i64) -> i64 {
    registered_mut::<SetResult>(h, &SET_RESULTS, "set result").map_or(-1, |result| {
        i64::try_from(result.members.len()).unwrap_or(-1)
    })
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_smembers_get(h: i64, i: i64) -> BytesTriple {
    let Some(result) = registered_mut::<SetResult>(h, &SET_RESULTS, "set result") else {
        return empty_bytes();
    };
    usize::try_from(i)
        .ok()
        .and_then(|index| result.members.get(index))
        .map_or_else(empty_bytes, |value| owned_bytes(value))
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_smembers_free(h: i64) {
    free_registered::<SetResult>(h, &SET_RESULTS);
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_publish_len(
    c: i64,
    ch: *const c_char,
    chl: i64,
    m: *const BytesTriple,
) -> i64 {
    lock_connection!(c, shared_c, c, -1);
    let ch = text_arg!(ch, chl, "channel");
    let m = binary_arg!(m, "message");
    int_reply(c.inner.publish(ch, m), "PUBLISH")
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_subscribe_once_len(
    c: i64,
    ch: *const c_char,
    chl: i64,
    timeout_ms: i64,
) -> i64 {
    let Some(c) = connection(c) else {
        return register(
            StringResult {
                status: -1,
                value: None,
            },
            &STRING_RESULTS,
        );
    };
    let ch = text_arg!(ch, chl, "channel");
    if timeout_ms <= 0 {
        set_error(ErrorKind::InvalidInput, "timeout must be positive");
        return register(
            StringResult {
                status: -1,
                value: None,
            },
            &STRING_RESULTS,
        );
    }
    let url = {
        let Ok(c) = c.lock() else {
            set_error(ErrorKind::Connection, "Redis connection is unavailable");
            return register(
                StringResult {
                    status: -1,
                    value: None,
                },
                &STRING_RESULTS,
            );
        };
        c.url.clone()
    };
    let client = match redis::Client::open(url.as_str()) {
        Ok(v) => v,
        Err(e) => {
            command_error("SUBSCRIBE", &e);
            return register(
                StringResult {
                    status: -1,
                    value: None,
                },
                &STRING_RESULTS,
            );
        }
    };
    let mut connection = match client.get_connection() {
        Ok(v) => v,
        Err(e) => {
            command_error("SUBSCRIBE", &e);
            return register(
                StringResult {
                    status: -1,
                    value: None,
                },
                &STRING_RESULTS,
            );
        }
    };
    let mut pubsub = connection.as_pubsub();
    let timeout = Duration::from_millis(timeout_ms.cast_unsigned());
    if let Err(e) = pubsub
        .set_read_timeout(Some(timeout))
        .and_then(|()| pubsub.subscribe(ch))
    {
        command_error("SUBSCRIBE", &e);
        return register(
            StringResult {
                status: -1,
                value: None,
            },
            &STRING_RESULTS,
        );
    }
    match pubsub.get_message() {
        Ok(message) => string_reply(message.get_payload::<Vec<u8>>().map(Some), "SUBSCRIBE"),
        Err(error) if error.is_timeout() => {
            set_error(
                ErrorKind::Timeout,
                format!("Redis subscription timed out after {timeout_ms} ms"),
            );
            register(
                StringResult {
                    status: 0,
                    value: None,
                },
                &STRING_RESULTS,
            )
        }
        Err(error) => {
            command_error("SUBSCRIBE", &error);
            register(
                StringResult {
                    status: -1,
                    value: None,
                },
                &STRING_RESULTS,
            )
        }
    }
}

/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_string_status(h: i64) -> i32 {
    registered_mut::<StringResult>(h, &STRING_RESULTS, "string result").map_or(-1, |r| r.status)
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_string_value(h: i64) -> BytesTriple {
    let Some(result) = registered_mut::<StringResult>(h, &STRING_RESULTS, "string result") else {
        return empty_bytes();
    };
    result
        .value
        .as_ref()
        .map_or_else(empty_bytes, |value| owned_bytes(value))
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_string_free(h: i64) {
    free_registered::<StringResult>(h, &STRING_RESULTS);
}

#[derive(Debug)]
struct Pipeline {
    inner: redis::Pipeline,
}
#[unsafe(no_mangle)]
pub extern "C" fn hew_redis_pipeline_new() -> i64 {
    clear_error();
    register_shared(
        Pipeline {
            inner: redis::pipe(),
        },
        &PIPELINES,
    )
}

fn pipeline_add(h: i64, command: &str, args: &[&str]) -> i64 {
    let Some(p) = registered_shared(h, &PIPELINES, "Redis pipeline") else {
        return -1;
    };
    let Ok(mut p) = p.lock() else {
        set_error(ErrorKind::Connection, "Redis pipeline is unavailable");
        return -1;
    };
    let mut cmd = redis::cmd(command);
    for arg in args {
        cmd.arg(arg);
    }
    p.inner.add_command(cmd);
    clear_error();
    0
}
fn pipeline_add_bytes(h: i64, command: &str, args: &[&[u8]]) -> i64 {
    let Some(p) = registered_shared(h, &PIPELINES, "Redis pipeline") else {
        return -1;
    };
    let Ok(mut p) = p.lock() else {
        set_error(ErrorKind::Connection, "Redis pipeline is unavailable");
        return -1;
    };
    let mut cmd = redis::cmd(command);
    for arg in args {
        cmd.arg(*arg);
    }
    p.inner.add_command(cmd);
    clear_error();
    0
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_pipeline_add0_len(h: i64, c: *const c_char, cl: i64) -> i64 {
    let c = text_arg!(c, cl, "command");
    pipeline_add(h, c, &[])
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_pipeline_add1_len(
    h: i64,
    c: *const c_char,
    cl: i64,
    a: *const BytesTriple,
) -> i64 {
    let c = text_arg!(c, cl, "command");
    let a = binary_arg!(a, "argument");
    pipeline_add_bytes(h, c, &[a])
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_pipeline_add2_len(
    h: i64,
    c: *const c_char,
    cl: i64,
    a: *const BytesTriple,
    b: *const BytesTriple,
) -> i64 {
    let c = text_arg!(c, cl, "command");
    let a = binary_arg!(a, "argument 1");
    let b = binary_arg!(b, "argument 2");
    pipeline_add_bytes(h, c, &[a, b])
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_pipeline_add3_len(
    handle: i64,
    command: *const c_char,
    command_len: i64,
    argument_a: *const BytesTriple,
    argument_b: *const BytesTriple,
    argument_c: *const BytesTriple,
) -> i64 {
    let command = text_arg!(command, command_len, "command");
    let argument_a = binary_arg!(argument_a, "argument 1");
    let argument_b = binary_arg!(argument_b, "argument 2");
    let argument_c = binary_arg!(argument_c, "argument 3");
    pipeline_add_bytes(handle, command, &[argument_a, argument_b, argument_c])
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_pipeline_exec(c: i64, p: i64) -> i64 {
    let Some(c) = connection(c) else {
        return -1;
    };
    let Some(p) = registered_shared(p, &PIPELINES, "Redis pipeline") else {
        return -1;
    };
    let Ok(mut c) = c.lock() else {
        set_error(ErrorKind::Connection, "Redis connection is unavailable");
        return -1;
    };
    let Ok(p) = p.lock() else {
        set_error(ErrorKind::Connection, "Redis pipeline is unavailable");
        return -1;
    };
    match p.inner.query::<redis::Value>(&mut c.inner) {
        Ok(_) => {
            clear_error();
            0
        }
        Err(e) => {
            command_error("pipeline", &e);
            -1
        }
    }
}
/// # Safety
/// Any pointer argument must name readable Hew ABI memory for its declared
/// length. Handle arguments must originate from this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_redis_pipeline_free(h: i64) {
    free_shared(h, &PIPELINES);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn length_boundary_preserves_embedded_nul() {
        let bytes = b"a\0b";
        // SAFETY: `bytes` remains alive and exposes exactly the supplied
        // number of initialized bytes for this call.
        let value = unsafe {
            utf8_with_len(
                bytes.as_ptr().cast(),
                i64::try_from(bytes.len()).unwrap(),
                "value",
            )
        };
        assert_eq!(value, Some("a\0b"));
    }

    #[test]
    fn bytes_abi_round_trips_embedded_nul() {
        let expected = b"left\0right";
        let value = owned_bytes(expected);
        assert_eq!(
            // SAFETY: `value` is a live bytes triple produced by `owned_bytes`.
            unsafe { bytes_arg(&raw const value, "value") },
            Some(expected.as_slice())
        );
        // SAFETY: `owned_bytes` allocated this block with `malloc`; subtracting
        // its fixed header yields the original allocation address.
        unsafe { libc::free(value.ptr.sub(8).cast()) };
    }

    #[test]
    fn invalid_url_reports_typed_error() {
        let url = "not-a-redis-url";
        // SAFETY: `url` is alive and exposes exactly the supplied byte count.
        let handle = unsafe {
            hew_redis_connect_len(url.as_ptr().cast(), i64::try_from(url.len()).unwrap())
        };
        assert_eq!(handle, 0);
        assert_eq!(hew_redis_last_error_kind(), ErrorKind::InvalidInput as i32);
    }

    #[test]
    fn package_string_return_uses_bare_malloc_ownership() {
        set_error(ErrorKind::Command, "allocator probe");
        let pointer = hew_redis_last_error();
        assert!(!pointer.is_null());
        // SAFETY: the export returns a NUL-terminated foreign-package string.
        let message = unsafe { CStr::from_ptr(pointer) }.to_str().unwrap();
        assert_eq!(message, "allocator probe");
        // SAFETY: Hew releases package string returns with libc::free.
        unsafe { libc::free(pointer.cast()) };
    }

    #[test]
    fn resources_close_idempotently() {
        let pipeline = hew_redis_pipeline_new();
        assert_ne!(pipeline, 0);
        // SAFETY: `pipeline` originated from this module; close is explicitly
        // idempotent and validates the registry before reclamation.
        unsafe {
            hew_redis_pipeline_free(pipeline);
            hew_redis_pipeline_free(pipeline);
        }
        assert!(!shared_registry(&PIPELINES)
            .lock()
            .unwrap()
            .contains_key(&pipeline));
    }

    #[test]
    fn closed_handle_fails_without_dereference() {
        let key = "key";
        // SAFETY: `key` remains alive for its supplied byte count. The invalid
        // handle is an intentional registry-validation regression case.
        let result = unsafe {
            hew_redis_del_len(
                12345,
                key.as_ptr().cast(),
                i64::try_from(key.len()).unwrap(),
            )
        };
        assert_eq!(result, -1);
        assert_eq!(hew_redis_last_error_kind(), ErrorKind::Closed as i32);
    }

    #[test]
    fn concurrent_pipeline_use_and_close_never_alias_or_dereference_freed_memory() {
        use std::sync::{Arc, Barrier};
        let pipeline = hew_redis_pipeline_new();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            let command = "PING";
            // SAFETY: `command` remains alive for its supplied byte count and
            // `pipeline` originated from this module.
            unsafe {
                hew_redis_pipeline_add0_len(
                    pipeline,
                    command.as_ptr().cast(),
                    i64::try_from(command.len()).unwrap(),
                )
            }
        });
        barrier.wait();
        // SAFETY: `pipeline` originated from this module; removing the shared
        // handle cannot reclaim it while the worker retains an `Arc` clone.
        unsafe { hew_redis_pipeline_free(pipeline) };
        let status = worker.join().unwrap();
        assert!(status == 0 || status == -1);
        // SAFETY: repeated close is an intentional idempotence assertion.
        unsafe { hew_redis_pipeline_free(pipeline) };
    }
}
