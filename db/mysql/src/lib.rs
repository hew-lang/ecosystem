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

//! Native `MySQL` support for `hew.db.mysql`.
//!
//! Opaque integer handles own connections and immutable query results. A
//! connection is cloned out of its registry before its mutex is acquired, so
//! registry locks never span network I/O.

#[cfg(test)]
use hew_cabi::string::string_release;
use hew_cabi::string::{string_as_str, string_from_str, HewString};
use std::cell::RefCell;
use std::collections::HashMap;
use std::slice;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use mysql::prelude::Queryable as _;

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

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    Connect = 1,
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

unsafe fn sql_input<'a>(value: *const HewString) -> Option<&'a str> {
    let value = unsafe { string_as_str(value) };
    if value.as_bytes().contains(&0) {
        set_error(ErrorKind::InvalidInput, "SQL contains an embedded NUL byte");
        None
    } else {
        Some(value)
    }
}

struct MysqlConnection {
    inner: mysql::PooledConn,
}

impl std::fmt::Debug for MysqlConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MysqlConnection")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct MysqlResult {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
}

static CONNECTIONS: LazyLock<Mutex<HashMap<i64, Arc<Mutex<MysqlConnection>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RESULTS: LazyLock<Mutex<HashMap<i64, Arc<MysqlResult>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CONNECTION: AtomicI64 = AtomicI64::new(1);
static NEXT_RESULT: AtomicI64 = AtomicI64::new(1);

fn register<T>(
    registry: &Mutex<HashMap<i64, Arc<T>>>,
    counter: &AtomicI64,
    value: T,
    label: &str,
) -> i64 {
    let handle = counter.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        std::process::abort();
    }
    match registry.lock() {
        Ok(mut values) => {
            values.insert(handle, Arc::new(value));
            handle
        }
        Err(_) => {
            set_error(
                ErrorKind::Internal,
                format!("MySQL {label} registry is unavailable"),
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
                format!("MySQL {label} registry is unavailable"),
            );
            return None;
        }
    };
    if value.is_none() {
        set_error(ErrorKind::Closed, format!("MySQL {label} is closed"));
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

fn connection(handle: i64) -> Option<Arc<Mutex<MysqlConnection>>> {
    registered(&CONNECTIONS, handle, "connection")
}

fn result(handle: i64) -> Option<Arc<MysqlResult>> {
    registered(&RESULTS, handle, "query result")
}

fn split_params(params: &str) -> Vec<mysql::Value> {
    if params.is_empty() {
        Vec::new()
    } else {
        params
            .split('\n')
            .map(|value| mysql::Value::Bytes(value.as_bytes().to_vec()))
            .collect()
    }
}

fn value_to_bytes(value: mysql::Value) -> Option<Vec<u8>> {
    match value {
        mysql::Value::NULL => None,
        mysql::Value::Bytes(value) => Some(value),
        mysql::Value::Int(value) => Some(value.to_string().into_bytes()),
        mysql::Value::UInt(value) => Some(value.to_string().into_bytes()),
        mysql::Value::Float(value) => Some(value.to_string().into_bytes()),
        mysql::Value::Double(value) => Some(value.to_string().into_bytes()),
        mysql::Value::Date(year, month, day, hour, minute, second, micros) => {
            let suffix = if micros == 0 {
                String::new()
            } else {
                format!(".{micros:06}")
            };
            Some(
                format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{suffix}")
                    .into_bytes(),
            )
        }
        mysql::Value::Time(negative, days, hour, minute, second, micros) => {
            let sign = if negative { "-" } else { "" };
            let hours = days * 24 + u32::from(hour);
            let suffix = if micros == 0 {
                String::new()
            } else {
                format!(".{micros:06}")
            };
            Some(format!("{sign}{hours:02}:{minute:02}:{second:02}{suffix}").into_bytes())
        }
    }
}

fn query_result_to_value<P: mysql::prelude::Protocol>(
    query: mysql::QueryResult<'_, '_, '_, P>,
) -> Result<MysqlResult, String> {
    let columns = query
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect::<Vec<_>>();
    let rows = query
        .collect::<mysql::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|row| row.unwrap().into_iter().map(value_to_bytes).collect())
        .collect();
    Ok(MysqlResult { columns, rows })
}

/// Connect using an exact UTF-8 `MySQL` URL.
///
/// # Safety
/// `url` must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mysql_connect(url: *const HewString) -> i64 {
    clear_error();
    let url = unsafe { string_as_str(url) };
    let pool = match mysql::Pool::new(url) {
        Ok(pool) => pool,
        Err(error) => {
            set_error(
                ErrorKind::Connect,
                format!("MySQL connection failed: {error}"),
            );
            return 0;
        }
    };
    match pool.get_conn() {
        Ok(inner) => register(
            &CONNECTIONS,
            &NEXT_CONNECTION,
            Mutex::new(MysqlConnection { inner }),
            "connection",
        ),
        Err(error) => {
            set_error(
                ErrorKind::Connect,
                format!("MySQL connection failed: {error}"),
            );
            0
        }
    }
}

unsafe fn execute_impl(
    handle: i64,
    sql: *const HewString,
    params: Option<*const BytesTriple>,
) -> i64 {
    clear_error();
    let Some(sql) = (unsafe { sql_input(sql) }) else {
        return 0;
    };
    let parameters = if let Some(params) = params {
        let Some(params) = (unsafe { params_input(params) }) else {
            return 0;
        };
        split_params(params)
    } else {
        Vec::new()
    };
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(mut connection) = connection.lock() else {
        set_error(ErrorKind::Internal, "MySQL connection lock is unavailable");
        return 0;
    };
    let operation = if params.is_some() {
        connection
            .inner
            .exec_drop(sql, mysql::Params::Positional(parameters))
    } else {
        connection.inner.query_drop(sql)
    };
    match operation {
        Ok(()) => match i64::try_from(connection.inner.affected_rows()) {
            Ok(count) => count,
            Err(_) => {
                set_error(ErrorKind::Internal, "MySQL affected-row count exceeds i64");
                0
            }
        },
        Err(error) => {
            set_error(ErrorKind::Query, format!("MySQL execute failed: {error}"));
            0
        }
    }
}

/// Execute SQL.
///
/// # Safety
/// `sql` must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mysql_execute(handle: i64, sql: *const HewString) -> i64 {
    unsafe { execute_impl(handle, sql, None) }
}

/// Execute parameterized SQL.
///
/// # Safety
/// The SQL string must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mysql_execute_params(
    handle: i64,
    sql: *const HewString,
    params: *const BytesTriple,
) -> i64 {
    unsafe { execute_impl(handle, sql, Some(params)) }
}

unsafe fn query_impl(
    handle: i64,
    sql: *const HewString,
    params: Option<*const BytesTriple>,
) -> i64 {
    clear_error();
    let Some(sql) = (unsafe { sql_input(sql) }) else {
        return 0;
    };
    let parameters = if let Some(params) = params {
        let Some(params) = (unsafe { params_input(params) }) else {
            return 0;
        };
        split_params(params)
    } else {
        Vec::new()
    };
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(mut connection) = connection.lock() else {
        set_error(ErrorKind::Internal, "MySQL connection lock is unavailable");
        return 0;
    };
    let value = if params.is_some() {
        connection
            .inner
            .exec_iter(sql, mysql::Params::Positional(parameters))
            .map_err(|error| error.to_string())
            .and_then(query_result_to_value)
    } else {
        connection
            .inner
            .query_iter(sql)
            .map_err(|error| error.to_string())
            .and_then(query_result_to_value)
    };
    match value {
        Ok(value) => register(&RESULTS, &NEXT_RESULT, value, "query result"),
        Err(error) => {
            set_error(ErrorKind::Query, format!("MySQL query failed: {error}"));
            0
        }
    }
}

/// Query SQL.
///
/// # Safety
/// `sql` must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mysql_query(handle: i64, sql: *const HewString) -> i64 {
    unsafe { query_impl(handle, sql, None) }
}

/// Query parameterized SQL.
///
/// # Safety
/// The SQL string must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mysql_query_params(
    handle: i64,
    sql: *const HewString,
    params: *const BytesTriple,
) -> i64 {
    unsafe { query_impl(handle, sql, Some(params)) }
}

#[no_mangle]
pub extern "C" fn hew_mysql_result_rows(handle: i64) -> i64 {
    clear_error();
    result(handle)
        .and_then(|value| i64::try_from(value.rows.len()).ok())
        .unwrap_or_else(|| {
            if LAST_ERROR.with(|state| state.borrow().kind) == ErrorKind::None {
                set_error(ErrorKind::Internal, "MySQL row count exceeds i64");
            }
            -1
        })
}

#[no_mangle]
pub extern "C" fn hew_mysql_result_cols(handle: i64) -> i64 {
    clear_error();
    result(handle)
        .and_then(|value| i64::try_from(value.columns.len()).ok())
        .unwrap_or_else(|| {
            if LAST_ERROR.with(|state| state.borrow().kind) == ErrorKind::None {
                set_error(ErrorKind::Internal, "MySQL column count exceeds i64");
            }
            -1
        })
}

#[no_mangle]
pub extern "C" fn hew_mysql_result_column(handle: i64, index: i64) -> BytesTriple {
    clear_error();
    let Ok(index) = usize::try_from(index) else {
        set_error(
            ErrorKind::InvalidInput,
            "MySQL column index is negative or oversized",
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
                "MySQL column index is out of bounds",
            );
            empty_bytes()
        }
    }
}

fn result_cell(handle: i64, row: i64, column: i64) -> Option<Option<Vec<u8>>> {
    let Ok(row) = usize::try_from(row) else {
        set_error(
            ErrorKind::InvalidInput,
            "MySQL row index is negative or oversized",
        );
        return None;
    };
    let Ok(column) = usize::try_from(column) else {
        set_error(
            ErrorKind::InvalidInput,
            "MySQL column index is negative or oversized",
        );
        return None;
    };
    let result = result(handle)?;
    match result.rows.get(row).and_then(|values| values.get(column)) {
        Some(value) => Some(value.clone()),
        None => {
            set_error(ErrorKind::InvalidInput, "MySQL cell index is out of bounds");
            None
        }
    }
}

#[no_mangle]
pub extern "C" fn hew_mysql_result_cell_kind(handle: i64, row: i64, column: i64) -> i32 {
    clear_error();
    match result_cell(handle, row, column) {
        Some(None) => 0,
        Some(Some(_)) => 1,
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn hew_mysql_result_cell(handle: i64, row: i64, column: i64) -> BytesTriple {
    clear_error();
    match result_cell(handle, row, column) {
        Some(Some(value)) => owned_bytes(&value),
        Some(None) => {
            set_error(ErrorKind::InvalidInput, "MySQL cell is NULL");
            empty_bytes()
        }
        None => empty_bytes(),
    }
}

#[no_mangle]
pub extern "C" fn hew_mysql_result_free(handle: i64) {
    unregister(&RESULTS, handle);
}

#[no_mangle]
pub extern "C" fn hew_mysql_close(handle: i64) {
    unregister(&CONNECTIONS, handle);
}

#[no_mangle]
pub extern "C" fn hew_mysql_connection_count() -> i64 {
    let Ok(values) = CONNECTIONS.lock() else {
        std::process::abort();
    };
    let Ok(count) = i64::try_from(values.len()) else {
        std::process::abort();
    };
    count
}

#[no_mangle]
pub extern "C" fn hew_mysql_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[no_mangle]
pub extern "C" fn hew_mysql_last_error() -> *mut HewString {
    LAST_ERROR.with(|state| string_from_str(&state.borrow().message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_error_message() -> String {
        let message = hew_mysql_last_error();
        let text = unsafe { string_as_str(message) }.to_owned();
        unsafe { string_release(message) };
        text
    }

    #[test]
    fn embedded_nul_sql_is_rejected_before_driver_dispatch() {
        let value = string_from_str("SELECT 1\0 trailing");
        assert!(unsafe { sql_input(value) }.is_none());
        unsafe { string_release(value) };
        assert_eq!(hew_mysql_last_error_kind(), ErrorKind::InvalidInput as i32);
    }

    #[test]
    fn invalid_handles_fail_closed_with_typed_error() {
        let sql = string_from_str("SELECT 1");
        assert_eq!(unsafe { hew_mysql_execute(987_654, sql) }, 0);
        unsafe { string_release(sql) };
        assert_eq!(hew_mysql_last_error_kind(), ErrorKind::Closed as i32);
        assert!(last_error_message().contains("is closed"));
        assert_eq!(hew_mysql_result_rows(987_654), -1);
        assert_eq!(hew_mysql_last_error_kind(), ErrorKind::Closed as i32);
    }

    #[test]
    fn managed_error_messages_carry_non_ascii_text() {
        set_error(ErrorKind::Query, "Répertoire des requêtes traitées — 雪");
        assert_eq!(
            last_error_message(),
            "Répertoire des requêtes traitées — 雪"
        );
        clear_error();
        assert_eq!(last_error_message(), "");
    }

    #[test]
    fn binary_values_are_preserved_exactly() {
        assert_eq!(
            value_to_bytes(mysql::Value::Bytes(vec![0xff])),
            Some(vec![0xff])
        );
    }

    #[cfg(feature = "integration")]
    #[test]
    fn service_round_trip_preserves_null_and_exact_values() {
        let url = std::env::var("HEW_MYSQL_URL")
            .unwrap_or_else(|_| "mysql://hew:hew@127.0.0.1:3306/hew_test".to_owned());
        let url = string_from_str(&url);
        let handle = unsafe { hew_mysql_connect(url) };
        unsafe { string_release(url) };
        assert!(handle > 0, "{}", last_error_message());
        let sql =
            string_from_str("SELECT '' AS empty_value, NULL AS missing_value, 41 AS count_value");
        let result = unsafe { hew_mysql_query(handle, sql) };
        unsafe { string_release(sql) };
        assert!(result > 0, "{}", last_error_message());
        assert_eq!(hew_mysql_result_rows(result), 1);
        assert_eq!(hew_mysql_result_cols(result), 3);
        assert_eq!(hew_mysql_result_cell_kind(result, 0, 0), 1);
        assert_eq!(hew_mysql_result_cell_kind(result, 0, 1), 0);
        assert_eq!(hew_mysql_result_cell_kind(result, 0, 2), 1);
        hew_mysql_result_free(result);

        let sql = string_from_str("SELECT * FROM répertoire_des_requêtes_雪");
        assert_eq!(unsafe { hew_mysql_query(handle, sql) }, 0);
        unsafe { string_release(sql) };
        assert_eq!(hew_mysql_last_error_kind(), ErrorKind::Query as i32);
        assert!(last_error_message().contains("répertoire_des_requêtes_雪"));
        hew_mysql_close(handle);
    }
}
