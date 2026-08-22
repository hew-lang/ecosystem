#![expect(
    clippy::undocumented_unsafe_blocks,
    reason = "C ABI entry points validate pointer/length and registered-handle contracts before each unsafe call"
)]
#![expect(
    clippy::option_option,
    reason = "the outer option reports an invalid index while the inner option represents SQL NULL"
)]
#![expect(
    clippy::single_match_else,
    reason = "explicit match arms keep fail-closed registry and numeric conversion paths visible"
)]

//! Native `SQLite` support for `hew.db.sqlite`.
//!
//! Connections and query results are registered under opaque integer handles.
//! Registry guards are released before a connection mutex is acquired, so no
//! registry lock spans database I/O.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

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
    let base = unsafe { libc::malloc(allocation_len) }.cast::<u8>();
    if base.is_null() {
        std::process::abort();
    }
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

unsafe fn bytes_arg<'a>(value: *const BytesTriple, label: &str) -> Option<&'a [u8]> {
    let Some(value) = (unsafe { value.as_ref() }) else {
        set_error(
            ErrorKind::InvalidInput,
            format!("{label} bytes pointer is null"),
        );
        return None;
    };
    if value.len == 0 {
        return Some(&[]);
    }
    if value.ptr.is_null() {
        set_error(
            ErrorKind::InvalidInput,
            format!("{label} bytes data is null"),
        );
        return None;
    }
    unsafe {
        Some(slice::from_raw_parts(
            value.ptr.add(value.offset as usize),
            value.len as usize,
        ))
    }
}

unsafe fn params_input<'a>(value: *const BytesTriple) -> Option<&'a str> {
    let value = unsafe { bytes_arg(value, "parameter") }?;
    match std::str::from_utf8(value) {
        Ok(value) => Some(value),
        Err(error) => {
            set_error(
                ErrorKind::InvalidInput,
                format!("parameters are not UTF-8: {error}"),
            );
            None
        }
    }
}

fn malloc_c_string(value: &str) -> *mut c_char {
    let Some(size) = value.len().checked_add(1) else {
        return std::ptr::null_mut();
    };
    let output = unsafe { libc::malloc(size) }.cast::<u8>();
    if output.is_null() {
        return output.cast();
    }
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
    Open = 1,
    InvalidInput = 2,
    Query = 3,
    Closed = 4,
    Internal = 5,
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

unsafe fn utf8_with_len<'a>(value: *const c_char, len: i64) -> Result<&'a str, &'static str> {
    let len = usize::try_from(len).map_err(|_| "negative or oversized string length")?;
    if value.is_null() {
        return if len == 0 {
            Ok("")
        } else {
            Err("null string pointer with non-zero length")
        };
    }
    let bytes = unsafe { slice::from_raw_parts(value.cast::<u8>(), len) };
    std::str::from_utf8(bytes).map_err(|_| "string input was not valid UTF-8")
}

unsafe fn input<'a>(value: *const c_char, len: i64, label: &str) -> Option<&'a str> {
    match unsafe { utf8_with_len(value, len) } {
        Ok(value) => Some(value),
        Err(error) => {
            set_error(ErrorKind::InvalidInput, format!("invalid {label}: {error}"));
            None
        }
    }
}

unsafe fn sql_input<'a>(value: *const c_char, len: i64) -> Option<&'a str> {
    let value = unsafe { input(value, len, "SQL") }?;
    if value.as_bytes().contains(&0) {
        set_error(ErrorKind::InvalidInput, "SQL contains an embedded NUL byte");
        None
    } else {
        Some(value)
    }
}

#[derive(Debug)]
struct SqliteConnection {
    inner: rusqlite::Connection,
}

#[derive(Debug)]
struct SqliteResult {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
}

static CONNECTIONS: LazyLock<Mutex<HashMap<i64, Arc<Mutex<SqliteConnection>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RESULTS: LazyLock<Mutex<HashMap<i64, Arc<SqliteResult>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CONNECTION: AtomicI64 = AtomicI64::new(1);
static NEXT_RESULT: AtomicI64 = AtomicI64::new(1);

fn next_handle(counter: &AtomicI64) -> Option<i64> {
    let handle = counter.fetch_add(1, Ordering::Relaxed);
    (handle > 0).then_some(handle)
}

fn register<T>(
    registry: &Mutex<HashMap<i64, Arc<T>>>,
    counter: &AtomicI64,
    value: T,
    label: &str,
) -> i64 {
    let Some(handle) = next_handle(counter) else {
        std::process::abort();
    };
    match registry.lock() {
        Ok(mut values) => {
            values.insert(handle, Arc::new(value));
            handle
        }
        Err(_) => {
            set_error(
                ErrorKind::Internal,
                format!("SQLite {label} registry is unavailable"),
            );
            0
        }
    }
}

fn registered<T>(
    registry: &Mutex<HashMap<i64, Arc<T>>>,
    handle: i64,
    label: &str,
) -> Option<Arc<T>> {
    let value = match registry.lock() {
        Ok(values) => values.get(&handle).cloned(),
        Err(_) => {
            set_error(
                ErrorKind::Internal,
                format!("SQLite {label} registry is unavailable"),
            );
            return None;
        }
    };
    if value.is_none() {
        set_error(ErrorKind::Closed, format!("SQLite {label} is closed"));
    }
    value
}

fn unregister<T>(registry: &Mutex<HashMap<i64, Arc<T>>>, handle: i64) {
    match registry.lock() {
        Ok(mut values) => {
            values.remove(&handle);
        }
        Err(_) => std::process::abort(),
    }
}

fn connection(handle: i64) -> Option<Arc<Mutex<SqliteConnection>>> {
    registered(&CONNECTIONS, handle, "connection")
}

fn result(handle: i64) -> Option<Arc<SqliteResult>> {
    registered(&RESULTS, handle, "query result")
}

fn split_params(params: &str) -> Vec<&str> {
    if params.is_empty() {
        Vec::new()
    } else {
        params.split('\n').collect()
    }
}

fn query_result(
    connection: &mut SqliteConnection,
    sql: &str,
    params: &[&dyn rusqlite::types::ToSql],
) -> Result<SqliteResult, rusqlite::Error> {
    let mut statement = connection.inner.prepare(sql)?;
    let columns = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let column_count = columns.len();
    let mapped = statement.query_map(params, |row| {
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            let value = match row.get_ref(index)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(value) => Some(value.to_string().into_bytes()),
                rusqlite::types::ValueRef::Real(value) => Some(value.to_string().into_bytes()),
                rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                    Some(value.to_vec())
                }
            };
            values.push(value);
        }
        Ok(values)
    })?;
    let rows = mapped.collect::<Result<Vec<_>, _>>()?;
    Ok(SqliteResult { columns, rows })
}

/// Open a `SQLite` database from an exact UTF-8 path.
///
/// # Safety
/// `path` must address exactly `path_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn hew_sqlite_open_len(path: *const c_char, path_len: i64) -> i64 {
    clear_error();
    let Some(path) = (unsafe { input(path, path_len, "database path") }) else {
        return 0;
    };
    match rusqlite::Connection::open(path) {
        Ok(inner) => register(
            &CONNECTIONS,
            &NEXT_CONNECTION,
            Mutex::new(SqliteConnection { inner }),
            "connection",
        ),
        Err(error) => {
            set_error(
                ErrorKind::Open,
                format!("could not open SQLite database: {error}"),
            );
            0
        }
    }
}

/// Execute SQL against a registered connection.
///
/// # Safety
/// `sql` must address exactly `sql_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn hew_sqlite_execute_len(
    handle: i64,
    sql: *const c_char,
    sql_len: i64,
) -> i64 {
    clear_error();
    let Some(sql) = (unsafe { sql_input(sql, sql_len) }) else {
        return 0;
    };
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(connection) = connection.lock() else {
        set_error(ErrorKind::Internal, "SQLite connection lock is unavailable");
        return 0;
    };
    match connection.inner.execute_batch(sql) {
        Ok(()) => match i64::try_from(connection.inner.changes()) {
            Ok(count) => count,
            Err(_) => {
                set_error(ErrorKind::Internal, "SQLite affected-row count exceeds i64");
                0
            }
        },
        Err(error) => {
            set_error(ErrorKind::Query, format!("SQLite execute failed: {error}"));
            0
        }
    }
}

/// Execute parameterized SQL against a registered connection.
///
/// # Safety
/// Both strings must address exactly their supplied byte lengths.
#[no_mangle]
pub unsafe extern "C" fn hew_sqlite_execute_params_len(
    handle: i64,
    sql: *const c_char,
    sql_len: i64,
    params: *const BytesTriple,
) -> i64 {
    clear_error();
    let Some(sql) = (unsafe { sql_input(sql, sql_len) }) else {
        return 0;
    };
    let Some(params) = (unsafe { params_input(params) }) else {
        return 0;
    };
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(connection) = connection.lock() else {
        set_error(ErrorKind::Internal, "SQLite connection lock is unavailable");
        return 0;
    };
    let values = split_params(params);
    let params = values
        .iter()
        .map(|value| value as &dyn rusqlite::types::ToSql)
        .collect::<Vec<_>>();
    match connection.inner.execute(sql, params.as_slice()) {
        Ok(count) => match i64::try_from(count) {
            Ok(count) => count,
            Err(_) => {
                set_error(ErrorKind::Internal, "SQLite affected-row count exceeds i64");
                0
            }
        },
        Err(error) => {
            set_error(ErrorKind::Query, format!("SQLite execute failed: {error}"));
            0
        }
    }
}

unsafe fn query_impl(
    handle: i64,
    sql: *const c_char,
    sql_len: i64,
    params: Option<*const BytesTriple>,
) -> i64 {
    clear_error();
    let Some(sql) = (unsafe { sql_input(sql, sql_len) }) else {
        return 0;
    };
    let parameter_values = if let Some(params) = params {
        let Some(params) = (unsafe { params_input(params) }) else {
            return 0;
        };
        split_params(params)
    } else {
        Vec::new()
    };
    let parameters = parameter_values
        .iter()
        .map(|value| value as &dyn rusqlite::types::ToSql)
        .collect::<Vec<_>>();
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(mut connection) = connection.lock() else {
        set_error(ErrorKind::Internal, "SQLite connection lock is unavailable");
        return 0;
    };
    match query_result(&mut connection, sql, parameters.as_slice()) {
        Ok(value) => register(&RESULTS, &NEXT_RESULT, value, "query result"),
        Err(error) => {
            set_error(ErrorKind::Query, format!("SQLite query failed: {error}"));
            0
        }
    }
}

/// Query SQL and return an owned result handle.
///
/// # Safety
/// `sql` must address exactly `sql_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn hew_sqlite_query_len(
    handle: i64,
    sql: *const c_char,
    sql_len: i64,
) -> i64 {
    unsafe { query_impl(handle, sql, sql_len, None) }
}

/// Query parameterized SQL and return an owned result handle.
///
/// # Safety
/// Both strings must address exactly their supplied byte lengths.
#[no_mangle]
pub unsafe extern "C" fn hew_sqlite_query_params_len(
    handle: i64,
    sql: *const c_char,
    sql_len: i64,
    params: *const BytesTriple,
) -> i64 {
    unsafe { query_impl(handle, sql, sql_len, Some(params)) }
}

#[no_mangle]
pub extern "C" fn hew_sqlite_result_rows(handle: i64) -> i64 {
    clear_error();
    result(handle)
        .and_then(|value| i64::try_from(value.rows.len()).ok())
        .unwrap_or_else(|| {
            if LAST_ERROR.with(|state| state.borrow().kind) == ErrorKind::None {
                set_error(ErrorKind::Internal, "SQLite row count exceeds i64");
            }
            -1
        })
}

#[no_mangle]
pub extern "C" fn hew_sqlite_result_cols(handle: i64) -> i64 {
    clear_error();
    result(handle)
        .and_then(|value| i64::try_from(value.columns.len()).ok())
        .unwrap_or_else(|| {
            if LAST_ERROR.with(|state| state.borrow().kind) == ErrorKind::None {
                set_error(ErrorKind::Internal, "SQLite column count exceeds i64");
            }
            -1
        })
}

#[no_mangle]
pub extern "C" fn hew_sqlite_result_column(handle: i64, index: i64) -> BytesTriple {
    clear_error();
    let Ok(index) = usize::try_from(index) else {
        set_error(
            ErrorKind::InvalidInput,
            "SQLite column index is negative or oversized",
        );
        return empty_bytes();
    };
    let Some(result) = result(handle) else {
        return empty_bytes();
    };
    match result.columns.get(index) {
        Some(value) => owned_bytes(value.as_bytes()),
        None => {
            set_error(
                ErrorKind::InvalidInput,
                "SQLite column index is out of bounds",
            );
            empty_bytes()
        }
    }
}

fn result_cell(handle: i64, row: i64, column: i64) -> Option<Option<Vec<u8>>> {
    let Ok(row) = usize::try_from(row) else {
        set_error(
            ErrorKind::InvalidInput,
            "SQLite row index is negative or oversized",
        );
        return None;
    };
    let Ok(column) = usize::try_from(column) else {
        set_error(
            ErrorKind::InvalidInput,
            "SQLite column index is negative or oversized",
        );
        return None;
    };
    let result = result(handle)?;
    match result.rows.get(row).and_then(|values| values.get(column)) {
        Some(value) => Some(value.clone()),
        None => {
            set_error(
                ErrorKind::InvalidInput,
                "SQLite cell index is out of bounds",
            );
            None
        }
    }
}

#[no_mangle]
pub extern "C" fn hew_sqlite_result_cell_kind(handle: i64, row: i64, column: i64) -> i32 {
    clear_error();
    match result_cell(handle, row, column) {
        Some(None) => 0,
        Some(Some(_)) => 1,
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn hew_sqlite_result_cell(handle: i64, row: i64, column: i64) -> BytesTriple {
    clear_error();
    match result_cell(handle, row, column) {
        Some(Some(value)) => owned_bytes(&value),
        Some(None) => {
            set_error(ErrorKind::InvalidInput, "SQLite cell is NULL");
            empty_bytes()
        }
        None => empty_bytes(),
    }
}

#[no_mangle]
pub extern "C" fn hew_sqlite_result_free(handle: i64) {
    unregister(&RESULTS, handle);
}

#[no_mangle]
pub extern "C" fn hew_sqlite_close(handle: i64) {
    unregister(&CONNECTIONS, handle);
}

#[no_mangle]
pub extern "C" fn hew_sqlite_connection_count() -> i64 {
    let Ok(values) = CONNECTIONS.lock() else {
        std::process::abort();
    };
    let Ok(count) = i64::try_from(values.len()) else {
        std::process::abort();
    };
    count
}

#[no_mangle]
pub extern "C" fn hew_sqlite_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[no_mangle]
pub extern "C" fn hew_sqlite_last_error() -> *mut c_char {
    LAST_ERROR.with(|state| malloc_c_string(&state.borrow().message))
}

#[cfg(test)]
mod tests {
    use super::*;

    static CONNECTION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn open_memory() -> i64 {
        let path = ":memory:";
        let handle = unsafe {
            hew_sqlite_open_len(path.as_ptr().cast(), i64::try_from(path.len()).unwrap())
        };
        assert!(handle > 0);
        handle
    }

    fn execute(handle: i64, sql: &str) -> i64 {
        unsafe {
            hew_sqlite_execute_len(
                handle,
                sql.as_ptr().cast(),
                i64::try_from(sql.len()).expect("test input length fits in i64"),
            )
        }
    }

    fn query(handle: i64, sql: &str) -> i64 {
        unsafe {
            hew_sqlite_query_len(
                handle,
                sql.as_ptr().cast(),
                i64::try_from(sql.len()).expect("test input length fits in i64"),
            )
        }
    }

    unsafe fn bytes_value(value: BytesTriple) -> Vec<u8> {
        if value.ptr.is_null() {
            return Vec::new();
        }
        let bytes = unsafe { slice::from_raw_parts(value.ptr, value.len as usize) }.to_vec();
        unsafe { libc::free(value.ptr.sub(8).cast()) };
        bytes
    }

    #[test]
    fn pointer_length_input_preserves_embedded_nul() {
        let value = b"left\0right";
        let parsed = unsafe {
            utf8_with_len(
                value.as_ptr().cast(),
                i64::try_from(value.len()).expect("test input length fits in i64"),
            )
        }
        .unwrap();
        assert_eq!(parsed.as_bytes(), value);
    }

    #[test]
    fn embedded_nul_sql_is_rejected_before_driver_dispatch() {
        let value = b"SELECT 1\0 trailing";
        assert!(unsafe {
            sql_input(
                value.as_ptr().cast(),
                i64::try_from(value.len()).expect("test input length fits in i64"),
            )
        }
        .is_none());
        assert_eq!(hew_sqlite_last_error_kind(), ErrorKind::InvalidInput as i32);
    }

    #[test]
    fn query_preserves_empty_nul_and_null_cells() {
        let _test_guard = CONNECTION_TEST_LOCK.lock().unwrap();
        let handle = open_memory();
        assert_eq!(execute(handle, "CREATE TABLE t (value TEXT)"), 0);
        assert_eq!(
            execute(
                handle,
                "INSERT INTO t VALUES (''), (char(65, 0, 66)), (NULL)"
            ),
            3
        );
        let result = query(handle, "SELECT value FROM t ORDER BY rowid");
        assert!(result > 0);
        assert_eq!(hew_sqlite_result_rows(result), 3);
        assert_eq!(hew_sqlite_result_cols(result), 1);
        assert_eq!(hew_sqlite_result_cell_kind(result, 0, 0), 1);
        assert_eq!(
            unsafe { bytes_value(hew_sqlite_result_cell(result, 0, 0)) },
            b""
        );
        assert_eq!(
            unsafe { bytes_value(hew_sqlite_result_cell(result, 1, 0)) },
            b"A\0B"
        );
        assert_eq!(hew_sqlite_result_cell_kind(result, 2, 0), 0);
        hew_sqlite_result_free(result);
        hew_sqlite_close(handle);
    }

    #[test]
    fn invalid_sql_sets_typed_query_error() {
        let _test_guard = CONNECTION_TEST_LOCK.lock().unwrap();
        let handle = open_memory();
        assert_eq!(execute(handle, "NOT SQL"), 0);
        assert_eq!(hew_sqlite_last_error_kind(), ErrorKind::Query as i32);
        hew_sqlite_close(handle);
    }

    #[test]
    fn close_and_result_free_are_idempotent_and_observable() {
        let _test_guard = CONNECTION_TEST_LOCK.lock().unwrap();
        let before = hew_sqlite_connection_count();
        let handle = open_memory();
        assert_eq!(hew_sqlite_connection_count(), before + 1);
        let result = query(handle, "SELECT 1 AS value");
        assert_eq!(hew_sqlite_result_rows(result), 1);
        hew_sqlite_result_free(result);
        hew_sqlite_result_free(result);
        assert_eq!(hew_sqlite_result_rows(result), -1);
        assert_eq!(hew_sqlite_last_error_kind(), ErrorKind::Closed as i32);
        hew_sqlite_close(handle);
        hew_sqlite_close(handle);
        assert_eq!(hew_sqlite_connection_count(), before);
        assert_eq!(execute(handle, "SELECT 1"), 0);
        assert_eq!(hew_sqlite_last_error_kind(), ErrorKind::Closed as i32);
    }

    #[test]
    fn out_of_bounds_cell_sets_invalid_input() {
        let _test_guard = CONNECTION_TEST_LOCK.lock().unwrap();
        let handle = open_memory();
        let result = query(handle, "SELECT 1");
        assert_eq!(hew_sqlite_result_cell_kind(result, 2, 0), -1);
        assert_eq!(hew_sqlite_last_error_kind(), ErrorKind::InvalidInput as i32);
        hew_sqlite_result_free(result);
        hew_sqlite_close(handle);
    }
}
