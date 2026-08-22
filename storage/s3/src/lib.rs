//! Native S3-compatible object storage support.
//!
//! Text crosses the ABI as pointer/length pairs, payloads use Hew's `bytes`
//! triple, and every opaque value lives in an idempotent handle registry.
//! Registry locks are released before any blocking HTTP operation begins.

use rusty_s3::S3Action as _;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read as _;
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BytesTriple {
    ptr: *mut u8,
    offset: u32,
    len: u32,
}

/// Hew's `bytes` value is a 16-byte `BytesTriple`; its data pointer follows
/// this runtime-owned allocation header exactly.
#[repr(C)]
struct BytesHeader {
    refcount: AtomicU32,
    capacity: u32,
}

const BYTES_HEADER_SIZE: usize = std::mem::size_of::<BytesHeader>();

const _: () = {
    assert!(std::mem::size_of::<BytesTriple>() == 16);
    assert!(BYTES_HEADER_SIZE == 8);
};

fn empty_bytes() -> BytesTriple {
    BytesTriple {
        ptr: std::ptr::null_mut(),
        offset: 0,
        len: 0,
    }
}

#[allow(
    clippy::cast_ptr_alignment,
    reason = "malloc's allocation is at least 8-byte aligned on every supported \
              target, well above BytesHeader's 4-byte (u32) alignment"
)]
fn owned_bytes(value: &[u8]) -> BytesTriple {
    if value.is_empty() {
        return empty_bytes();
    }
    let capacity = value.len().max(16);
    let Ok(capacity_u32) = u32::try_from(capacity) else {
        std::process::abort()
    };
    let Ok(len_u32) = u32::try_from(value.len()) else {
        std::process::abort()
    };
    let Some(allocation_len) = capacity.checked_add(BYTES_HEADER_SIZE) else {
        std::process::abort()
    };
    // SAFETY: the allocation is checked before writing its header and payload.
    let base = unsafe { libc::malloc(allocation_len) }.cast::<u8>();
    if base.is_null() {
        std::process::abort();
    }
    // SAFETY: `base` is malloc-aligned and names one header plus `capacity`
    // writable payload bytes. This mirrors hew-runtime's pinned bytes layout.
    unsafe {
        base.cast::<BytesHeader>().write(BytesHeader {
            refcount: AtomicU32::new(1),
            capacity: capacity_u32,
        });
        let data = base.add(BYTES_HEADER_SIZE);
        std::ptr::copy_nonoverlapping(value.as_ptr(), data, value.len());
        BytesTriple {
            ptr: data,
            offset: 0,
            len: len_u32,
        }
    }
}

fn malloc_c_string(value: &str) -> *mut c_char {
    let Some(size) = value.len().checked_add(1) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `size` is non-zero and the result is checked.
    let output = unsafe { libc::malloc(size) }.cast::<u8>();
    if output.is_null() {
        return output.cast();
    }
    // SAFETY: output names `size` writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), output, value.len());
        output.add(value.len()).write(0);
    }
    output.cast()
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    Connection = 1,
    InvalidInput = 2,
    NotFound = 3,
    AccessDenied = 4,
    Throttled = 5,
    ServerError = 6,
    Network = 7,
    HttpStatus = 8,
    Decode = 9,
    Closed = 10,
}

#[derive(Debug)]
struct ErrorState {
    kind: ErrorKind,
    status: i32,
    message: String,
}

thread_local! {
    static LAST_ERROR: RefCell<ErrorState> = const { RefCell::new(ErrorState {
        kind: ErrorKind::None, status: 0, message: String::new(),
    }) };
}

fn clear_error() {
    LAST_ERROR.with(|s| {
        let mut s = s.borrow_mut();
        s.kind = ErrorKind::None;
        s.status = 0;
        s.message.clear();
    });
}
fn set_error(kind: ErrorKind, message: impl Into<String>) {
    LAST_ERROR.with(|s| {
        let mut s = s.borrow_mut();
        s.kind = kind;
        s.status = 0;
        s.message = message.into();
    });
}

fn set_error_with_status(kind: ErrorKind, status: i32, message: impl Into<String>) {
    LAST_ERROR.with(|s| {
        let mut s = s.borrow_mut();
        s.kind = kind;
        s.status = status;
        s.message = message.into();
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_last_error_kind() -> i32 {
    LAST_ERROR.with(|s| s.borrow().kind as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_last_error_status() -> i32 {
    LAST_ERROR.with(|s| s.borrow().status)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_last_error() -> *mut c_char {
    LAST_ERROR.with(|s| malloc_c_string(&s.borrow().message))
}

unsafe fn utf8_with_len<'a>(ptr: *const c_char, len: i64, what: &str) -> Option<&'a str> {
    let Ok(len) = usize::try_from(len) else {
        set_error(
            ErrorKind::InvalidInput,
            format!("{what} length is negative"),
        );
        return None;
    };
    if ptr.is_null() {
        if len == 0 {
            return Some("");
        }
        set_error(ErrorKind::InvalidInput, format!("{what} pointer is null"));
        return None;
    }
    // SAFETY: caller supplies `len` readable bytes; null was handled above.
    let bytes = unsafe { slice::from_raw_parts(ptr.cast::<u8>(), len) };
    match std::str::from_utf8(bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            set_error(
                ErrorKind::InvalidInput,
                format!("{what} is not UTF-8: {error}"),
            );
            None
        }
    }
}

unsafe fn bytes_arg<'a>(value: *const BytesTriple) -> Option<&'a [u8]> {
    // SAFETY: caller supplies a valid Hew bytes triple.
    let Some(value) = (unsafe { value.as_ref() }) else {
        set_error(ErrorKind::InvalidInput, "body bytes pointer is null");
        return None;
    };
    if value.len == 0 {
        return Some(&[]);
    }
    if value.ptr.is_null() {
        set_error(ErrorKind::InvalidInput, "body bytes data is null");
        return None;
    }
    // SAFETY: Hew guarantees ptr+offset has `len` readable bytes.
    Some(unsafe { slice::from_raw_parts(value.ptr.add(value.offset as usize), value.len as usize) })
}

#[derive(Debug)]
struct Bucket {
    client: rusty_s3::Bucket,
    credentials: rusty_s3::Credentials,
    agent: ureq::Agent,
    name: String,
}

fn request_error_message(method: &str, bucket: &Bucket, key: &str, error: &ureq::Error) -> String {
    let status = match error {
        ureq::Error::Status(status, _) => status.to_string(),
        ureq::Error::Transport(_) => "network".to_owned(),
    };
    format!(
        "S3 {method} failed: bucket={:?} key={key:?} status={status}",
        bucket.name
    )
}

fn classify_http_status(status: u16) -> ErrorKind {
    match status {
        401 | 403 => ErrorKind::AccessDenied,
        404 => ErrorKind::NotFound,
        429 | 503 => ErrorKind::Throttled,
        500..=599 => ErrorKind::ServerError,
        _ => ErrorKind::HttpStatus,
    }
}

fn set_request_error(method: &str, bucket: &Bucket, key: &str, error: &ureq::Error) {
    let (kind, status) = match error {
        ureq::Error::Status(status, _) => (classify_http_status(*status), i32::from(*status)),
        ureq::Error::Transport(_) => (ErrorKind::Network, 0),
    };
    set_error_with_status(
        kind,
        status,
        request_error_message(method, bucket, key, error),
    );
}

fn response_error_message(
    method: &str,
    bucket: &Bucket,
    key: &str,
    status: u16,
    detail: &str,
) -> String {
    format!(
        "S3 {method} failed: bucket={:?} key={key:?} status={status}: {detail}",
        bucket.name
    )
}

#[derive(Debug)]
struct GetResult {
    status: i32,
    value: Vec<u8>,
}

#[derive(Debug)]
struct ListResult {
    entries: Vec<(String, i64)>,
}

type Registry<T> = OnceLock<Mutex<HashMap<i64, Arc<T>>>>;
static BUCKETS: Registry<Bucket> = OnceLock::new();
static GET_RESULTS: Registry<GetResult> = OnceLock::new();
static LIST_RESULTS: Registry<ListResult> = OnceLock::new();
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn registry<T>(cell: &'static Registry<T>) -> &'static Mutex<HashMap<i64, Arc<T>>> {
    cell.get_or_init(|| Mutex::new(HashMap::new()))
}
fn register<T>(value: T, cell: &'static Registry<T>) -> i64 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        std::process::abort();
    }
    if let Ok(mut values) = registry(cell).lock() {
        values.insert(handle, Arc::new(value));
        handle
    } else {
        set_error(ErrorKind::Connection, "S3 handle registry is unavailable");
        0
    }
}
fn lookup<T>(handle: i64, cell: &'static Registry<T>, what: &str) -> Option<Arc<T>> {
    let value = registry(cell).lock().ok()?.get(&handle).cloned();
    if value.is_none() {
        set_error(ErrorKind::Closed, format!("S3 {what} handle is closed"));
    }
    value
}
fn free<T>(handle: i64, cell: &'static Registry<T>) {
    if handle != 0 {
        if let Ok(mut values) = registry(cell).lock() {
            values.remove(&handle);
        }
    }
}

macro_rules! text_arg {
    ($ptr:expr, $len:expr, $name:literal, $fallback:expr) => {
        match {
            // SAFETY: callers pass a pointer/length pair originating from a Hew
            // string argument; `utf8_with_len` validates the length and
            // null-pointer cases internally before dereferencing.
            unsafe { utf8_with_len($ptr, $len, $name) }
        } {
            Some(v) => v,
            None => return $fallback,
        }
    };
}

/// Connect to an S3-compatible endpoint and register a bucket handle.
///
/// # Safety
///
/// `endpoint`, `region`, `bucket_name`, `access_key`, and `secret_key` must
/// each be either null (with a length of 0) or point to at least `*_len`
/// readable bytes, matching Hew's string FFI convention.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_connect_len(
    endpoint: *const c_char,
    endpoint_len: i64,
    region: *const c_char,
    region_len: i64,
    bucket_name: *const c_char,
    bucket_len: i64,
    access_key: *const c_char,
    access_len: i64,
    secret_key: *const c_char,
    secret_len: i64,
) -> i64 {
    let endpoint = text_arg!(endpoint, endpoint_len, "endpoint", 0);
    let region = text_arg!(region, region_len, "region", 0);
    let bucket_name = text_arg!(bucket_name, bucket_len, "bucket", 0);
    let access_key = text_arg!(access_key, access_len, "access key", 0);
    let secret_key = text_arg!(secret_key, secret_len, "secret key", 0);
    if bucket_name.is_empty() {
        set_error(ErrorKind::InvalidInput, "bucket must not be empty");
        return 0;
    }
    let endpoint = match endpoint.parse::<url::Url>() {
        Ok(v) => v,
        Err(e) => {
            set_error(ErrorKind::InvalidInput, format!("invalid S3 endpoint: {e}"));
            return 0;
        }
    };
    let bucket = match rusty_s3::Bucket::new(
        endpoint,
        rusty_s3::UrlStyle::Path,
        bucket_name.to_owned(),
        region.to_owned(),
    ) {
        Ok(v) => v,
        Err(e) => {
            set_error(ErrorKind::InvalidInput, format!("invalid S3 bucket: {e}"));
            return 0;
        }
    };
    let handle = register(
        Bucket {
            client: bucket,
            credentials: rusty_s3::Credentials::new(access_key, secret_key),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(30))
                .timeout_write(Duration::from_secs(30))
                .build(),
            name: bucket_name.to_owned(),
        },
        &BUCKETS,
    );
    if handle != 0 {
        clear_error();
    }
    handle
}

/// Release a bucket handle. Safe to call more than once.
///
/// # Safety
///
/// `handle` must be a value previously returned by `hew_s3_connect_len`, or
/// 0, which is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_close(handle: i64) {
    free(handle, &BUCKETS);
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_bucket_count() -> i64 {
    registry(&BUCKETS)
        .lock()
        .ok()
        .and_then(|v| i64::try_from(v.len()).ok())
        .unwrap_or(-1)
}

/// Upload `body` under `key` with the given content type.
///
/// # Safety
///
/// `key` and `content_type` must be valid Hew string pointer/length pairs;
/// `body` must be null or point to a valid Hew bytes triple.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_put_len(
    handle: i64,
    key: *const c_char,
    key_len: i64,
    body: *const BytesTriple,
    content_type: *const c_char,
    content_type_len: i64,
) -> i32 {
    let key = text_arg!(key, key_len, "key", -1);
    let content_type = text_arg!(content_type, content_type_len, "content type", -1);
    // SAFETY: caller supplies a valid Hew bytes value.
    let Some(body) = (unsafe { bytes_arg(body) }) else {
        return -1;
    };
    let Some(conn) = lookup(handle, &BUCKETS, "bucket") else {
        return -1;
    };
    let url = conn
        .client
        .put_object(Some(&conn.credentials), key)
        .sign(Duration::from_mins(5));
    match conn
        .agent
        .put(url.as_str())
        .set("Content-Type", content_type)
        .send_bytes(body)
    {
        Ok(_) => {
            clear_error();
            0
        }
        Err(error) => {
            set_request_error("PUT", &conn, key, &error);
            -1
        }
    }
}

/// Fetch an object's bytes, registering a `GetResult` handle.
///
/// # Safety
///
/// `key` must be a valid Hew string pointer/length pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_get_len(handle: i64, key: *const c_char, key_len: i64) -> i64 {
    let key = text_arg!(key, key_len, "key", 0);
    let Some(conn) = lookup(handle, &BUCKETS, "bucket") else {
        return 0;
    };
    let url = conn
        .client
        .get_object(Some(&conn.credentials), key)
        .sign(Duration::from_mins(5));
    match conn.agent.get(url.as_str()).call() {
        Ok(response) => {
            let status = response.status();
            let mut value = Vec::new();
            if let Err(error) = response.into_reader().read_to_end(&mut value) {
                set_error(
                    ErrorKind::Decode,
                    response_error_message(
                        "GET",
                        &conn,
                        key,
                        status,
                        &format!("response read failed: {error}"),
                    ),
                );
                return 0;
            }
            clear_error();
            register(GetResult { status: 1, value }, &GET_RESULTS)
        }
        Err(ureq::Error::Status(404, _)) => {
            clear_error();
            register(
                GetResult {
                    status: 0,
                    value: Vec::new(),
                },
                &GET_RESULTS,
            )
        }
        Err(error) => {
            set_request_error("GET", &conn, key, &error);
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_get_status(handle: i64) -> i32 {
    lookup(handle, &GET_RESULTS, "get result").map_or(-1, |v| v.status)
}
#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_get_value(handle: i64) -> BytesTriple {
    lookup(handle, &GET_RESULTS, "get result").map_or_else(empty_bytes, |v| owned_bytes(&v.value))
}
/// Free a `GetResult` handle produced by `hew_s3_get_len`.
///
/// # Safety
///
/// `handle` must be a value returned by `hew_s3_get_len`, or 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_get_free(handle: i64) {
    free(handle, &GET_RESULTS);
}

/// Delete an object. Deleting an absent key is not an error.
///
/// # Safety
///
/// `key` must be a valid Hew string pointer/length pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_delete_len(handle: i64, key: *const c_char, key_len: i64) -> i32 {
    let key = text_arg!(key, key_len, "key", -1);
    let Some(conn) = lookup(handle, &BUCKETS, "bucket") else {
        return -1;
    };
    let url = conn
        .client
        .delete_object(Some(&conn.credentials), key)
        .sign(Duration::from_mins(5));
    match conn.agent.delete(url.as_str()).call() {
        Ok(_) => {
            clear_error();
            0
        }
        Err(error) => {
            set_request_error("DELETE", &conn, key, &error);
            -1
        }
    }
}

/// Return whether an object exists: 1 (yes), 0 (no), -1 (error).
///
/// # Safety
///
/// `key` must be a valid Hew string pointer/length pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_exists_len(handle: i64, key: *const c_char, key_len: i64) -> i32 {
    let key = text_arg!(key, key_len, "key", -1);
    let Some(conn) = lookup(handle, &BUCKETS, "bucket") else {
        return -1;
    };
    let url = conn
        .client
        .head_object(Some(&conn.credentials), key)
        .sign(Duration::from_mins(5));
    match conn.agent.head(url.as_str()).call() {
        Ok(_) => {
            clear_error();
            1
        }
        Err(ureq::Error::Status(404, _)) => {
            clear_error();
            0
        }
        Err(error) => {
            set_request_error("HEAD", &conn, key, &error);
            -1
        }
    }
}

/// List every object under `prefix`, following continuation tokens and
/// registering a `ListResult` handle.
///
/// # Safety
///
/// `prefix` must be a valid Hew string pointer/length pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_list_len_ffi(
    handle: i64,
    prefix: *const c_char,
    prefix_len: i64,
) -> i64 {
    let prefix = text_arg!(prefix, prefix_len, "prefix", 0);
    let Some(conn) = lookup(handle, &BUCKETS, "bucket") else {
        return 0;
    };
    let mut entries = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let mut action = conn.client.list_objects_v2(Some(&conn.credentials));
        action.with_prefix(prefix);
        if let Some(token) = continuation.as_deref() {
            action.with_continuation_token(token);
        }
        let url = action.sign(Duration::from_mins(5));
        let response = match conn.agent.get(url.as_str()).call() {
            Ok(response) => response,
            Err(error) => {
                set_request_error("LIST", &conn, prefix, &error);
                return 0;
            }
        };
        let status = response.status();
        let xml = match response.into_string() {
            Ok(xml) => xml,
            Err(error) => {
                set_error(
                    ErrorKind::Decode,
                    response_error_message(
                        "LIST",
                        &conn,
                        prefix,
                        status,
                        &format!("response read failed: {error}"),
                    ),
                );
                return 0;
            }
        };
        let response = match rusty_s3::actions::ListObjectsV2::parse_response(&xml) {
            Ok(v) => v,
            Err(error) => {
                set_error(
                    ErrorKind::Decode,
                    format!("S3 LIST response was invalid: {error}"),
                );
                return 0;
            }
        };
        entries.extend(
            response
                .contents
                .into_iter()
                .map(|v| (v.key, i64::try_from(v.size).unwrap_or(i64::MAX))),
        );
        match response.next_continuation_token {
            Some(token) if !token.is_empty() => continuation = Some(token),
            _ => break,
        }
    }
    clear_error();
    register(ListResult { entries }, &LIST_RESULTS)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_list_count(handle: i64) -> i64 {
    lookup(handle, &LIST_RESULTS, "list result")
        .and_then(|v| i64::try_from(v.entries.len()).ok())
        .unwrap_or(-1)
}
#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_list_key(handle: i64, index: i64) -> *mut c_char {
    let Ok(index) = usize::try_from(index) else {
        set_error(ErrorKind::InvalidInput, "list index is negative");
        return std::ptr::null_mut();
    };
    lookup(handle, &LIST_RESULTS, "list result")
        .and_then(|v| v.entries.get(index).map(|e| malloc_c_string(&e.0)))
        .unwrap_or_else(|| {
            set_error(ErrorKind::InvalidInput, "list index is out of bounds");
            std::ptr::null_mut()
        })
}
#[unsafe(no_mangle)]
pub extern "C" fn hew_s3_list_size(handle: i64, index: i64) -> i64 {
    let Ok(index) = usize::try_from(index) else {
        set_error(ErrorKind::InvalidInput, "list index is negative");
        return -1;
    };
    lookup(handle, &LIST_RESULTS, "list result")
        .and_then(|v| v.entries.get(index).map(|e| e.1))
        .unwrap_or_else(|| {
            set_error(ErrorKind::InvalidInput, "list index is out of bounds");
            -1
        })
}
/// Free a `ListResult` handle produced by `hew_s3_list_len_ffi`.
///
/// # Safety
///
/// `handle` must be a value returned by `hew_s3_list_len_ffi`, or 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_list_free(handle: i64) {
    free(handle, &LIST_RESULTS);
}

/// Generate a signed URL for GET, PUT, DELETE, or HEAD.
///
/// # Safety
///
/// `key` and `method` must be valid Hew string pointer/length pairs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_presign_len(
    handle: i64,
    key: *const c_char,
    key_len: i64,
    method: *const c_char,
    method_len: i64,
    expires_seconds: i64,
) -> *mut c_char {
    if expires_seconds <= 0 {
        set_error(ErrorKind::InvalidInput, "expiry must be positive");
        return std::ptr::null_mut();
    }
    let key = text_arg!(key, key_len, "key", std::ptr::null_mut());
    let method = text_arg!(method, method_len, "method", std::ptr::null_mut());
    let Some(conn) = lookup(handle, &BUCKETS, "bucket") else {
        return std::ptr::null_mut();
    };
    let expires = Duration::from_secs(u64::try_from(expires_seconds).unwrap_or(u64::MAX));
    let url = match method {
        "GET" => conn
            .client
            .get_object(Some(&conn.credentials), key)
            .sign(expires),
        "PUT" => conn
            .client
            .put_object(Some(&conn.credentials), key)
            .sign(expires),
        "DELETE" => conn
            .client
            .delete_object(Some(&conn.credentials), key)
            .sign(expires),
        "HEAD" => conn
            .client
            .head_object(Some(&conn.credentials), key)
            .sign(expires),
        _ => {
            set_error(
                ErrorKind::InvalidInput,
                "method must be GET, PUT, DELETE, or HEAD",
            );
            return std::ptr::null_mut();
        }
    };
    clear_error();
    malloc_c_string(url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    static TEST_BUCKETS: Mutex<()> = Mutex::new(());

    #[allow(
        clippy::cast_ptr_alignment,
        reason = "malloc's allocation is at least 8-byte aligned on every supported \
                  target, well above BytesHeader's 4-byte (u32) alignment"
    )]
    unsafe fn release_bytes_like_hew(value: BytesTriple) {
        if value.ptr.is_null() {
            return;
        }
        // SAFETY: test callers pass a live value returned by `owned_bytes`.
        let header = unsafe { value.ptr.sub(BYTES_HEADER_SIZE).cast::<BytesHeader>() };
        // SAFETY: `header` points to the initialized Hew bytes header.
        if unsafe { (*header).refcount.fetch_sub(1, Ordering::Release) } == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            // SAFETY: the final owner releases the malloc allocation base.
            unsafe { libc::free(header.cast()) };
        }
    }

    #[allow(
        clippy::cast_possible_wrap,
        reason = "test endpoint literals are far below i64::MAX in length"
    )]
    unsafe fn connect(endpoint: &str) -> i64 {
        // SAFETY: every argument is a valid, live Rust string slice whose
        // length matches its byte length, satisfying hew_s3_connect_len's
        // contract.
        unsafe {
            hew_s3_connect_len(
                endpoint.as_ptr().cast(),
                endpoint.len() as i64,
                "us-east-1".as_ptr().cast(),
                9,
                "hew-test".as_ptr().cast(),
                8,
                "minioadmin".as_ptr().cast(),
                10,
                "minioadmin".as_ptr().cast(),
                10,
            )
        }
    }

    #[test]
    fn invalid_endpoint_returns_typed_error() {
        // SAFETY: connect()'s contract is satisfied by the fixed test literal.
        let handle = unsafe { connect("not a URL") };
        assert_eq!(handle, 0);
        assert_eq!(hew_s3_last_error_kind(), ErrorKind::InvalidInput as i32);
    }

    #[test]
    fn close_is_idempotent_and_registry_count_is_relative() {
        let _serial = TEST_BUCKETS.lock().unwrap();
        let before = hew_s3_bucket_count();
        // SAFETY: connect()'s contract is satisfied by the fixed test literal.
        let handle = unsafe { connect("http://127.0.0.1:9000") };
        assert_ne!(handle, 0);
        assert_eq!(hew_s3_bucket_count(), before + 1);
        // SAFETY: `handle` came from connect() above; hew_s3_close is
        // idempotent by contract, so calling it twice is deliberate here.
        unsafe {
            hew_s3_close(handle);
            hew_s3_close(handle);
        }
        assert_eq!(hew_s3_bucket_count(), before);
        assert_eq!(
            // SAFETY: "x" is a valid 1-byte string literal; `handle` is
            // stale, which hew_s3_exists_len's contract handles by
            // returning a typed error rather than dereferencing it.
            unsafe { hew_s3_exists_len(handle, "x".as_ptr().cast(), 1) },
            -1
        );
    }

    fn assert_last_request_error_is_redacted(method: &str, key: &str) {
        let message = LAST_ERROR.with(|state| state.borrow().message.clone());
        assert_eq!(hew_s3_last_error_kind(), ErrorKind::Network as i32);
        assert_eq!(hew_s3_last_error_status(), 0);
        assert!(message.contains(&format!("S3 {method} failed")));
        assert!(message.contains("bucket=\"hew-test\""));
        assert!(message.contains(&format!("key={key:?}")));
        assert!(message.contains("status=network"));
        assert!(!message.contains("X-Amz"), "signed query leaked: {message}");
        assert!(
            !message.contains("minioadmin"),
            "credential leaked: {message}"
        );
        assert!(
            !message.contains("http://"),
            "request URL leaked: {message}"
        );
        assert!(
            !message.contains("https://"),
            "request URL leaked: {message}"
        );
    }

    #[test]
    fn http_statuses_map_to_public_error_categories() {
        assert_eq!(classify_http_status(401), ErrorKind::AccessDenied);
        assert_eq!(classify_http_status(403), ErrorKind::AccessDenied);
        assert_eq!(classify_http_status(404), ErrorKind::NotFound);
        assert_eq!(classify_http_status(429), ErrorKind::Throttled);
        assert_eq!(classify_http_status(503), ErrorKind::Throttled);
        assert_eq!(classify_http_status(500), ErrorKind::ServerError);
        assert_eq!(classify_http_status(599), ErrorKind::ServerError);
        assert_eq!(classify_http_status(409), ErrorKind::HttpStatus);
    }

    #[test]
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        reason = "test payload and key literals are a handful of bytes, far \
                  below u32::MAX / i64::MAX"
    )]
    fn signed_request_errors_never_expose_the_url() {
        let _serial = TEST_BUCKETS.lock().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let endpoint = format!("http://127.0.0.1:{port}");
        // SAFETY: connect()'s contract is satisfied by the fixed test literal.
        let handle = unsafe { connect(&endpoint) };
        assert_ne!(handle, 0);
        let key = "private/object";
        let body_data = [1_u8, 2, 3];
        let body = BytesTriple {
            ptr: body_data.as_ptr().cast_mut(),
            offset: 0,
            len: body_data.len() as u32,
        };

        assert_eq!(
            // SAFETY: `key`, `body`, and the content type are valid
            // pointer/length pairs to live test data.
            unsafe {
                hew_s3_put_len(
                    handle,
                    key.as_ptr().cast(),
                    key.len() as i64,
                    &raw const body,
                    "image/png".as_ptr().cast(),
                    9,
                )
            },
            -1
        );
        assert_last_request_error_is_redacted("PUT", key);

        assert_eq!(
            // SAFETY: `key` is a valid pointer/length pair to a live &str.
            unsafe { hew_s3_get_len(handle, key.as_ptr().cast(), key.len() as i64) },
            0
        );
        assert_last_request_error_is_redacted("GET", key);

        assert_eq!(
            // SAFETY: `key` is a valid pointer/length pair to a live &str.
            unsafe { hew_s3_delete_len(handle, key.as_ptr().cast(), key.len() as i64) },
            -1
        );
        assert_last_request_error_is_redacted("DELETE", key);

        assert_eq!(
            // SAFETY: `key` is a valid pointer/length pair to a live &str.
            unsafe { hew_s3_exists_len(handle, key.as_ptr().cast(), key.len() as i64) },
            -1
        );
        assert_last_request_error_is_redacted("HEAD", key);

        assert_eq!(
            // SAFETY: `key` is a valid pointer/length pair to a live &str.
            unsafe { hew_s3_list_len_ffi(handle, key.as_ptr().cast(), key.len() as i64) },
            0
        );
        assert_last_request_error_is_redacted("LIST", key);
        // SAFETY: `handle` came from connect() above.
        unsafe { hew_s3_close(handle) };
    }

    #[test]
    fn presign_validates_method_and_returns_allocation_base() {
        let _serial = TEST_BUCKETS.lock().unwrap();
        // SAFETY: connect()'s contract is satisfied by the fixed test literal.
        let handle = unsafe { connect("http://127.0.0.1:9000") };
        // SAFETY: "a b" and "GET" are valid pointer/length pairs to live
        // string literals.
        let ptr = unsafe {
            hew_s3_presign_len(
                handle,
                "a b".as_ptr().cast(),
                3,
                "GET".as_ptr().cast(),
                3,
                60,
            )
        };
        assert!(!ptr.is_null());
        // SAFETY: `ptr` is the non-null, NUL-terminated allocation
        // `hew_s3_presign_len` just returned.
        let value = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        assert!(value.contains("X-Amz-Signature="));
        // SAFETY: `ptr` was allocated by `malloc_c_string` and not freed yet.
        unsafe {
            libc::free(ptr.cast());
        }
        assert!(
            // SAFETY: "x" and "POST" are valid pointer/length pairs to live
            // string literals.
            unsafe {
                hew_s3_presign_len(
                    handle,
                    "x".as_ptr().cast(),
                    1,
                    "POST".as_ptr().cast(),
                    4,
                    60,
                )
            }
            .is_null()
        );
        assert_eq!(hew_s3_last_error_kind(), ErrorKind::InvalidInput as i32);
        // SAFETY: `handle` came from connect() above.
        unsafe {
            hew_s3_close(handle);
        }
    }

    #[test]
    fn list_resources_reject_stale_and_out_of_range_handles() {
        let handle = register(
            ListResult {
                entries: vec![("a".into(), 3)],
            },
            &LIST_RESULTS,
        );
        assert_eq!(hew_s3_list_count(handle), 1);
        let key = hew_s3_list_key(handle, 0);
        // SAFETY: `key` is the non-null, NUL-terminated allocation
        // `hew_s3_list_key` just returned.
        assert_eq!(unsafe { CStr::from_ptr(key) }.to_str().unwrap(), "a");
        // SAFETY: `key` was allocated by `malloc_c_string` and not freed
        // yet; `handle` came from `register` above and freeing it twice is
        // deliberately exercising idempotence.
        unsafe {
            libc::free(key.cast());
            hew_s3_list_free(handle);
            hew_s3_list_free(handle);
        }
        assert_eq!(hew_s3_list_count(handle), -1);
    }

    #[test]
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "malloc's allocation is at least 8-byte aligned on every supported \
                  target, well above BytesHeader's 4-byte (u32) alignment"
    )]
    fn non_empty_get_value_survives_hew_release_oracle() {
        let payload = b"non-empty S3 payload\0with binary data";
        for _ in 0..256 {
            let handle = register(
                GetResult {
                    status: 1,
                    value: payload.to_vec(),
                },
                &GET_RESULTS,
            );
            let value = hew_s3_get_value(handle);
            assert_eq!(std::mem::size_of_val(&value), 16);
            assert_eq!(value.offset, 0);
            assert_eq!(value.len as usize, payload.len());
            assert_eq!(
                // SAFETY: `value.ptr` names `value.len` readable bytes, per
                // `owned_bytes`'s contract.
                unsafe { slice::from_raw_parts(value.ptr, value.len as usize) },
                payload
            );
            // SAFETY: `value.ptr` points past a live Hew bytes header
            // written by `owned_bytes`.
            let header = unsafe { &*value.ptr.sub(BYTES_HEADER_SIZE).cast::<BytesHeader>() };
            assert_eq!(header.refcount.load(Ordering::Acquire), 1);
            assert!(header.capacity >= value.len);
            // SAFETY: `value` is the live handle produced above, released
            // exactly once through the same header/refcount path as
            // hew-runtime.
            unsafe {
                release_bytes_like_hew(value);
                hew_s3_get_free(handle);
            }
        }
    }

    #[cfg(feature = "integration")]
    #[test]
    #[allow(
        clippy::cast_possible_wrap,
        reason = "test key/content-type literals are a handful of bytes"
    )]
    fn minio_round_trip_preserves_binary_and_missing() {
        let _serial = TEST_BUCKETS.lock().unwrap();
        // SAFETY: connect()'s contract is satisfied by the fixed test literal.
        let handle = unsafe { connect("http://127.0.0.1:9000") };
        let body_data = [65_u8, 0, 66];
        let body = BytesTriple {
            ptr: body_data.as_ptr().cast_mut(),
            offset: 0,
            len: 3,
        };
        assert_eq!(
            // SAFETY: `key`, `body`, and the content type are valid
            // pointer/length pairs to live test data.
            unsafe {
                hew_s3_put_len(
                    handle,
                    "native/value".as_ptr().cast(),
                    12,
                    &raw const body,
                    "application/octet-stream".as_ptr().cast(),
                    24,
                )
            },
            0
        );
        // SAFETY: "native/value" is a valid pointer/length pair to a live
        // string literal.
        let get = unsafe { hew_s3_get_len(handle, "native/value".as_ptr().cast(), 12) };
        assert_eq!(hew_s3_get_status(get), 1);
        let value = hew_s3_get_value(get);
        assert_eq!(
            // SAFETY: `value.ptr` names `value.len` readable bytes, per
            // `owned_bytes`'s contract.
            unsafe { slice::from_raw_parts(value.ptr, value.len as usize) },
            body_data
        );
        // SAFETY: `value` is a live handle from `hew_s3_get_value`,
        // released through the same header/refcount path as hew-runtime.
        unsafe {
            release_bytes_like_hew(value);
            hew_s3_get_free(get);
        }
        assert_eq!(
            // SAFETY: "native/value" is a valid pointer/length pair to a
            // live string literal.
            unsafe { hew_s3_delete_len(handle, "native/value".as_ptr().cast(), 12) },
            0
        );
        // SAFETY: "native/value" is a valid pointer/length pair to a live
        // string literal.
        let missing = unsafe { hew_s3_get_len(handle, "native/value".as_ptr().cast(), 12) };
        assert_eq!(hew_s3_get_status(missing), 0);
        // SAFETY: `missing` and `handle` are the live handles obtained
        // above.
        unsafe {
            hew_s3_get_free(missing);
            hew_s3_close(handle);
        }
    }
}
