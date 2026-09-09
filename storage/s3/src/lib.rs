//! Native S3-compatible object storage support.
//!
//! Text crosses the ABI as managed Hew strings, payloads use Hew's `bytes`
//! triple, and every opaque value lives in an idempotent handle registry.
//! Registry locks are released before any blocking HTTP operation begins.

use hew_cabi::string::{string_as_str, string_from_str, HewString};
#[cfg(test)]
use hew_cabi::string::string_release;
use rusty_s3::S3Action as _;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read as _;
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
pub extern "C" fn hew_s3_last_error() -> *mut HewString {
    LAST_ERROR.with(|s| string_from_str(&s.borrow().message))
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

/// Decode a `%XX`-percent-encoded key from a LIST response.
///
/// `rusty_s3::ListObjectsV2` always requests `encoding-type=url` but returns
/// the still-encoded key text; every non-ASCII or reserved byte in an object
/// key would otherwise reach Hew code percent-escaped.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "hi and lo are each a single hex digit, so the combined value fits one byte"
                )]
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_owned())
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

/// Connect to an S3-compatible endpoint and register a bucket handle.
///
/// # Safety
///
/// `endpoint`, `region`, `bucket_name`, `access_key`, and `secret_key` must
/// each be a managed Hew string (or null, the canonical empty string).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_connect(
    endpoint: *const HewString,
    region: *const HewString,
    bucket_name: *const HewString,
    access_key: *const HewString,
    secret_key: *const HewString,
) -> i64 {
    // SAFETY: callers supply managed Hew string handles per this function's contract.
    let endpoint = unsafe { string_as_str(endpoint) };
    // SAFETY: see above.
    let region = unsafe { string_as_str(region) };
    // SAFETY: see above.
    let bucket_name = unsafe { string_as_str(bucket_name) };
    // SAFETY: see above.
    let access_key = unsafe { string_as_str(access_key) };
    // SAFETY: see above.
    let secret_key = unsafe { string_as_str(secret_key) };
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
/// `handle` must be a value previously returned by `hew_s3_connect`, or
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
/// `key` and `content_type` must be managed Hew strings; `body` must be null
/// or point to a valid Hew bytes triple.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_put(
    handle: i64,
    key: *const HewString,
    body: *const BytesTriple,
    content_type: *const HewString,
) -> i32 {
    // SAFETY: callers supply managed Hew string handles per this function's contract.
    let key = unsafe { string_as_str(key) };
    // SAFETY: see above.
    let content_type = unsafe { string_as_str(content_type) };
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
/// `key` must be a managed Hew string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_get(handle: i64, key: *const HewString) -> i64 {
    // SAFETY: caller supplies a managed Hew string handle per this function's contract.
    let key = unsafe { string_as_str(key) };
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
/// Free a `GetResult` handle produced by `hew_s3_get`.
///
/// # Safety
///
/// `handle` must be a value returned by `hew_s3_get`, or 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_get_free(handle: i64) {
    free(handle, &GET_RESULTS);
}

/// Delete an object. Deleting an absent key is not an error.
///
/// # Safety
///
/// `key` must be a managed Hew string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_delete(handle: i64, key: *const HewString) -> i32 {
    // SAFETY: caller supplies a managed Hew string handle per this function's contract.
    let key = unsafe { string_as_str(key) };
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
/// `key` must be a managed Hew string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_exists(handle: i64, key: *const HewString) -> i32 {
    // SAFETY: caller supplies a managed Hew string handle per this function's contract.
    let key = unsafe { string_as_str(key) };
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
/// `prefix` must be a managed Hew string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_list(handle: i64, prefix: *const HewString) -> i64 {
    // SAFETY: caller supplies a managed Hew string handle per this function's contract.
    let prefix = unsafe { string_as_str(prefix) };
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
                .map(|v| (percent_decode(&v.key), i64::try_from(v.size).unwrap_or(i64::MAX))),
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
pub extern "C" fn hew_s3_list_key(handle: i64, index: i64) -> *mut HewString {
    let Ok(index) = usize::try_from(index) else {
        set_error(ErrorKind::InvalidInput, "list index is negative");
        return std::ptr::null_mut();
    };
    lookup(handle, &LIST_RESULTS, "list result")
        .and_then(|v| v.entries.get(index).map(|e| string_from_str(&e.0)))
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
/// Free a `ListResult` handle produced by `hew_s3_list`.
///
/// # Safety
///
/// `handle` must be a value returned by `hew_s3_list`, or 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_list_free(handle: i64) {
    free(handle, &LIST_RESULTS);
}

/// Generate a signed URL for GET, PUT, DELETE, or HEAD.
///
/// # Safety
///
/// `key` and `method` must be managed Hew strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_s3_presign(
    handle: i64,
    key: *const HewString,
    method: *const HewString,
    expires_seconds: i64,
) -> *mut HewString {
    if expires_seconds <= 0 {
        set_error(ErrorKind::InvalidInput, "expiry must be positive");
        return std::ptr::null_mut();
    }
    // SAFETY: callers supply managed Hew string handles per this function's contract.
    let key = unsafe { string_as_str(key) };
    // SAFETY: see above.
    let method = unsafe { string_as_str(method) };
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
    string_from_str(url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    unsafe fn connect(endpoint: &str) -> i64 {
        let endpoint = string_from_str(endpoint);
        let region = string_from_str("us-east-1");
        let bucket = string_from_str("hew-test");
        let access_key = string_from_str("minioadmin");
        let secret_key = string_from_str("minioadmin");
        // SAFETY: every argument is a live managed Hew string, satisfying
        // hew_s3_connect's contract.
        let handle =
            unsafe { hew_s3_connect(endpoint, region, bucket, access_key, secret_key) };
        // SAFETY: each handle was produced by string_from_str above and is
        // still owned by this function; the callee never releases inbound
        // handles.
        unsafe {
            string_release(endpoint);
            string_release(region);
            string_release(bucket);
            string_release(access_key);
            string_release(secret_key);
        }
        handle
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
        let key = string_from_str("x");
        assert_eq!(
            // SAFETY: `key` is a live managed Hew string; `handle` is stale,
            // which hew_s3_exists's contract handles by returning a typed
            // error rather than dereferencing it.
            unsafe { hew_s3_exists(handle, key) },
            -1
        );
        // SAFETY: `key` is owned by this function and not yet released.
        unsafe { string_release(key) };
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
    fn percent_decode_reverses_the_list_response_url_encoding() {
        assert_eq!(percent_decode("plain-key.txt"), "plain-key.txt");
        assert_eq!(
            percent_decode("na%C3%AFve-%E6%97%A5%E6%9C%AC%E8%AA%9E.txt"),
            "naïve-日本語.txt"
        );
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("trailing%"), "trailing%");
        assert_eq!(percent_decode("bad%zzhex"), "bad%zzhex");
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
        reason = "test payload literal is a handful of bytes, far below u32::MAX"
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
        let key_s = string_from_str(key);
        let content_type_s = string_from_str("image/png");
        let body_data = [1_u8, 2, 3];
        let body = BytesTriple {
            ptr: body_data.as_ptr().cast_mut(),
            offset: 0,
            len: body_data.len() as u32,
        };

        assert_eq!(
            // SAFETY: `key_s`, `body`, and `content_type_s` are live managed
            // values satisfying hew_s3_put's contract.
            unsafe { hew_s3_put(handle, key_s, &raw const body, content_type_s) },
            -1
        );
        assert_last_request_error_is_redacted("PUT", key);

        assert_eq!(
            // SAFETY: `key_s` is a live managed Hew string.
            unsafe { hew_s3_get(handle, key_s) },
            0
        );
        assert_last_request_error_is_redacted("GET", key);

        assert_eq!(
            // SAFETY: `key_s` is a live managed Hew string.
            unsafe { hew_s3_delete(handle, key_s) },
            -1
        );
        assert_last_request_error_is_redacted("DELETE", key);

        assert_eq!(
            // SAFETY: `key_s` is a live managed Hew string.
            unsafe { hew_s3_exists(handle, key_s) },
            -1
        );
        assert_last_request_error_is_redacted("HEAD", key);

        assert_eq!(
            // SAFETY: `key_s` is a live managed Hew string.
            unsafe { hew_s3_list(handle, key_s) },
            0
        );
        assert_last_request_error_is_redacted("LIST", key);
        // SAFETY: `key_s`, `content_type_s` and `handle` are the live values
        // obtained above; the caller retained ownership throughout.
        unsafe {
            string_release(key_s);
            string_release(content_type_s);
            hew_s3_close(handle);
        }
    }

    #[test]
    fn presign_validates_method_and_returns_managed_string() {
        let _serial = TEST_BUCKETS.lock().unwrap();
        // SAFETY: connect()'s contract is satisfied by the fixed test literal.
        let handle = unsafe { connect("http://127.0.0.1:9000") };
        let key = string_from_str("a b");
        let get_method = string_from_str("GET");
        // SAFETY: `key` and `get_method` are live managed Hew strings.
        let ptr = unsafe { hew_s3_presign(handle, key, get_method, 60) };
        assert!(!ptr.is_null());
        // SAFETY: `ptr` is the live managed string `hew_s3_presign` just
        // returned.
        let value = unsafe { string_as_str(ptr) };
        assert!(value.contains("X-Amz-Signature="));
        // SAFETY: `ptr` was allocated by `string_from_str` and not released yet.
        unsafe { string_release(ptr) };
        let post_method = string_from_str("POST");
        assert!(
            // SAFETY: `key` and `post_method` are live managed Hew strings.
            unsafe { hew_s3_presign(handle, key, post_method, 60) }.is_null()
        );
        assert_eq!(hew_s3_last_error_kind(), ErrorKind::InvalidInput as i32);
        // SAFETY: `key`, `get_method`, `post_method` and `handle` are the
        // live values obtained above.
        unsafe {
            string_release(key);
            string_release(get_method);
            string_release(post_method);
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
        // SAFETY: `key` is the live managed string `hew_s3_list_key` just
        // returned.
        assert_eq!(unsafe { string_as_str(key) }, "a");
        // SAFETY: `key` was allocated by `hew_s3_list_key` and not released
        // yet; `handle` came from `register` above and freeing it twice is
        // deliberately exercising idempotence.
        unsafe {
            string_release(key);
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
        clippy::cast_possible_truncation,
        reason = "test payload literal is a handful of bytes, far below u32::MAX"
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
        let key = string_from_str("native/value");
        let content_type = string_from_str("application/octet-stream");
        assert_eq!(
            // SAFETY: `key`, `body`, and `content_type` are live managed
            // values satisfying hew_s3_put's contract.
            unsafe { hew_s3_put(handle, key, &raw const body, content_type) },
            0
        );
        // SAFETY: `content_type` is owned by this function and not needed again.
        unsafe { string_release(content_type) };
        // SAFETY: `key` is a live managed Hew string.
        let get = unsafe { hew_s3_get(handle, key) };
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
            // SAFETY: `key` is a live managed Hew string.
            unsafe { hew_s3_delete(handle, key) },
            0
        );
        // SAFETY: `key` is a live managed Hew string.
        let missing = unsafe { hew_s3_get(handle, key) };
        assert_eq!(hew_s3_get_status(missing), 0);
        // SAFETY: `missing`, `key` and `handle` are the live handles
        // obtained above.
        unsafe {
            hew_s3_get_free(missing);
            string_release(key);
            hew_s3_close(handle);
        }
    }
}
