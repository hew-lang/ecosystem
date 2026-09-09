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

//! Native `PostgreSQL` support for `hew.db.postgres`.
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

/// Flatten a driver error into readable text.
///
/// The driver's own `Display` is the bare word "db error"; the server's
/// diagnostic lives in the source chain, so walk it rather than reporting
/// every failure under one opaque phrase.
fn error_text(error: &postgres::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

struct PgConnection {
    inner: postgres::Client,
}

impl std::fmt::Debug for PgConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgConnection")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct PgResult {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
}

static CONNECTIONS: LazyLock<Mutex<HashMap<i64, Arc<Mutex<PgConnection>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RESULTS: LazyLock<Mutex<HashMap<i64, Arc<PgResult>>>> =
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
                format!("PostgreSQL {label} registry is unavailable"),
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
                format!("PostgreSQL {label} registry is unavailable"),
            );
            return None;
        }
    };
    if value.is_none() {
        set_error(ErrorKind::Closed, format!("PostgreSQL {label} is closed"));
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

fn connection(handle: i64) -> Option<Arc<Mutex<PgConnection>>> {
    registered(&CONNECTIONS, handle, "connection")
}

fn result(handle: i64) -> Option<Arc<PgResult>> {
    registered(&RESULTS, handle, "query result")
}

fn split_params(params: &str) -> Vec<&str> {
    if params.is_empty() {
        Vec::new()
    } else {
        params.split('\n').collect()
    }
}

fn cell_to_bytes(row: &postgres::Row, index: usize) -> Result<Option<Vec<u8>>, postgres::Error> {
    use postgres::types::Type;
    match *row.columns()[index].type_() {
        Type::BOOL => row
            .try_get::<_, Option<bool>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::OID => row
            .try_get::<_, Option<u32>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(index)
            .map(|value| value.map(|value| value.to_string().into_bytes())),
        Type::BYTEA => row.try_get::<_, Option<Vec<u8>>>(index),
        _ => row
            .try_get::<_, Option<String>>(index)
            .map(|value| value.map(String::into_bytes)),
    }
}

fn query_result(
    connection: &mut PgConnection,
    sql: &str,
    params: &[&(dyn postgres::types::ToSql + Sync)],
) -> Result<PgResult, postgres::Error> {
    let parameter_types = vec![postgres::types::Type::TEXT; params.len()];
    let statement = connection.inner.prepare_typed(sql, &parameter_types)?;
    let columns = statement
        .columns()
        .iter()
        .map(|column| column.name().to_owned())
        .collect::<Vec<_>>();
    let source_rows = connection.inner.query(&statement, params)?;
    let rows = source_rows
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| cell_to_bytes(row, index))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PgResult { columns, rows })
}

/// Connect using an exact UTF-8 `PostgreSQL` connection string.
///
/// # Safety
/// `connstr` must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_pg_connect(connstr: *const HewString) -> i64 {
    clear_error();
    let connstr = unsafe { string_as_str(connstr) };
    match postgres::Client::connect(connstr, postgres::NoTls) {
        Ok(inner) => register(
            &CONNECTIONS,
            &NEXT_CONNECTION,
            Mutex::new(PgConnection { inner }),
            "connection",
        ),
        Err(error) => {
            set_error(
                ErrorKind::Connect,
                format!("PostgreSQL connection failed: {}", error_text(&error)),
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
        .map(|value| value as &(dyn postgres::types::ToSql + Sync))
        .collect::<Vec<_>>();
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(mut connection) = connection.lock() else {
        set_error(
            ErrorKind::Internal,
            "PostgreSQL connection lock is unavailable",
        );
        return 0;
    };
    let parameter_types = vec![postgres::types::Type::TEXT; parameters.len()];
    let operation = connection
        .inner
        .prepare_typed(sql, &parameter_types)
        .and_then(|statement| connection.inner.execute(&statement, parameters.as_slice()));
    match operation {
        Ok(count) => match i64::try_from(count) {
            Ok(count) => count,
            Err(_) => {
                set_error(
                    ErrorKind::Internal,
                    "PostgreSQL affected-row count exceeds i64",
                );
                0
            }
        },
        Err(error) => {
            set_error(
                ErrorKind::Query,
                format!("PostgreSQL execute failed: {}", error_text(&error)),
            );
            0
        }
    }
}

/// Execute SQL.
///
/// # Safety
/// `sql` must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_pg_execute(handle: i64, sql: *const HewString) -> i64 {
    unsafe { execute_impl(handle, sql, None) }
}

/// Execute parameterized SQL.
///
/// # Safety
/// The SQL string must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_pg_execute_params(
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
        .map(|value| value as &(dyn postgres::types::ToSql + Sync))
        .collect::<Vec<_>>();
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(mut connection) = connection.lock() else {
        set_error(
            ErrorKind::Internal,
            "PostgreSQL connection lock is unavailable",
        );
        return 0;
    };
    match query_result(&mut connection, sql, parameters.as_slice()) {
        Ok(value) => register(&RESULTS, &NEXT_RESULT, value, "query result"),
        Err(error) => {
            set_error(
                ErrorKind::Query,
                format!("PostgreSQL query failed: {}", error_text(&error)),
            );
            0
        }
    }
}

/// Query SQL.
///
/// # Safety
/// `sql` must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_pg_query(handle: i64, sql: *const HewString) -> i64 {
    unsafe { query_impl(handle, sql, None) }
}

/// Query parameterized SQL.
///
/// # Safety
/// The SQL string must be a managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_pg_query_params(
    handle: i64,
    sql: *const HewString,
    params: *const BytesTriple,
) -> i64 {
    unsafe { query_impl(handle, sql, Some(params)) }
}

#[no_mangle]
pub extern "C" fn hew_pg_result_rows(handle: i64) -> i64 {
    clear_error();
    result(handle)
        .and_then(|value| i64::try_from(value.rows.len()).ok())
        .unwrap_or_else(|| {
            if LAST_ERROR.with(|state| state.borrow().kind) == ErrorKind::None {
                set_error(ErrorKind::Internal, "PostgreSQL row count exceeds i64");
            }
            -1
        })
}

#[no_mangle]
pub extern "C" fn hew_pg_result_cols(handle: i64) -> i64 {
    clear_error();
    result(handle)
        .and_then(|value| i64::try_from(value.columns.len()).ok())
        .unwrap_or_else(|| {
            if LAST_ERROR.with(|state| state.borrow().kind) == ErrorKind::None {
                set_error(ErrorKind::Internal, "PostgreSQL column count exceeds i64");
            }
            -1
        })
}

#[no_mangle]
pub extern "C" fn hew_pg_result_column(handle: i64, index: i64) -> BytesTriple {
    clear_error();
    let Ok(index) = usize::try_from(index) else {
        set_error(
            ErrorKind::InvalidInput,
            "PostgreSQL column index is negative or oversized",
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
                "PostgreSQL column index is out of bounds",
            );
            empty_bytes()
        }
    }
}

fn result_cell(handle: i64, row: i64, column: i64) -> Option<Option<Vec<u8>>> {
    let Ok(row) = usize::try_from(row) else {
        set_error(
            ErrorKind::InvalidInput,
            "PostgreSQL row index is negative or oversized",
        );
        return None;
    };
    let Ok(column) = usize::try_from(column) else {
        set_error(
            ErrorKind::InvalidInput,
            "PostgreSQL column index is negative or oversized",
        );
        return None;
    };
    let result = result(handle)?;
    match result.rows.get(row).and_then(|values| values.get(column)) {
        Some(value) => Some(value.clone()),
        None => {
            set_error(
                ErrorKind::InvalidInput,
                "PostgreSQL cell index is out of bounds",
            );
            None
        }
    }
}

#[no_mangle]
pub extern "C" fn hew_pg_result_cell_kind(handle: i64, row: i64, column: i64) -> i32 {
    clear_error();
    match result_cell(handle, row, column) {
        Some(None) => 0,
        Some(Some(_)) => 1,
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn hew_pg_result_cell(handle: i64, row: i64, column: i64) -> BytesTriple {
    clear_error();
    match result_cell(handle, row, column) {
        Some(Some(value)) => owned_bytes(&value),
        Some(None) => {
            set_error(ErrorKind::InvalidInput, "PostgreSQL cell is NULL");
            empty_bytes()
        }
        None => empty_bytes(),
    }
}

#[no_mangle]
pub extern "C" fn hew_pg_result_free(handle: i64) {
    unregister(&RESULTS, handle);
}

#[no_mangle]
pub extern "C" fn hew_pg_close(handle: i64) {
    unregister(&CONNECTIONS, handle);
}

#[no_mangle]
pub extern "C" fn hew_pg_connection_count() -> i64 {
    let Ok(values) = CONNECTIONS.lock() else {
        std::process::abort();
    };
    let Ok(count) = i64::try_from(values.len()) else {
        std::process::abort();
    };
    count
}

#[no_mangle]
pub extern "C" fn hew_pg_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[no_mangle]
pub extern "C" fn hew_pg_last_error() -> *mut HewString {
    LAST_ERROR.with(|state| string_from_str(&state.borrow().message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_error_message() -> String {
        let message = hew_pg_last_error();
        let text = unsafe { string_as_str(message) }.to_owned();
        unsafe { string_release(message) };
        text
    }

    #[test]
    fn embedded_nul_sql_is_rejected_before_driver_dispatch() {
        let value = string_from_str("SELECT 1\0 trailing");
        assert!(unsafe { sql_input(value) }.is_none());
        unsafe { string_release(value) };
        assert_eq!(hew_pg_last_error_kind(), ErrorKind::InvalidInput as i32);
    }

    #[test]
    fn invalid_handles_fail_closed_with_typed_error() {
        let sql = string_from_str("SELECT 1");
        assert_eq!(unsafe { hew_pg_execute(987_654, sql) }, 0);
        unsafe { string_release(sql) };
        assert_eq!(hew_pg_last_error_kind(), ErrorKind::Closed as i32);
        assert!(last_error_message().contains("is closed"));
        assert_eq!(hew_pg_result_rows(987_654), -1);
        assert_eq!(hew_pg_last_error_kind(), ErrorKind::Closed as i32);
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

    #[cfg(feature = "integration")]
    #[test]
    fn service_round_trip_preserves_null_and_exact_values() {
        let connstr = std::env::var("HEW_POSTGRES_URL").unwrap_or_else(|_| {
            "host=127.0.0.1 port=5432 user=hew password=hew dbname=hew_test".to_owned()
        });
        let connstr = string_from_str(&connstr);
        let handle = unsafe { hew_pg_connect(connstr) };
        unsafe { string_release(connstr) };
        assert!(handle > 0, "{}", last_error_message());
        let sql =
            string_from_str("SELECT ''::text AS empty, NULL::text AS missing, 41::bigint AS count");
        let result = unsafe { hew_pg_query(handle, sql) };
        unsafe { string_release(sql) };
        assert!(result > 0, "{}", last_error_message());
        assert_eq!(hew_pg_result_rows(result), 1);
        assert_eq!(hew_pg_result_cols(result), 3);
        assert_eq!(hew_pg_result_cell_kind(result, 0, 0), 1);
        assert_eq!(hew_pg_result_cell_kind(result, 0, 1), 0);
        assert_eq!(hew_pg_result_cell_kind(result, 0, 2), 1);
        hew_pg_result_free(result);

        let sql = string_from_str("SELECT 'Répertoire des requêtes traitées — 雪'::bigint");
        assert_eq!(unsafe { hew_pg_query(handle, sql) }, 0);
        unsafe { string_release(sql) };
        assert_eq!(hew_pg_last_error_kind(), ErrorKind::Query as i32);
        assert!(last_error_message().contains("Répertoire des requêtes traitées — 雪"));
        hew_pg_close(handle);
    }
}
