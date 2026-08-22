#![expect(
    clippy::undocumented_unsafe_blocks,
    reason = "C ABI entry points validate pointer/length and registered-handle contracts before each unsafe call"
)]

//! Native `MongoDB` support for `hew.db.mongodb`.
//!
//! Every string input is a pointer-and-length pair. Operation status, error
//! kind, and value presence are thread-local so the Hew actor can distinguish
//! errors, missing documents, empty values, and cursor exhaustion.

use mongodb::bson::{doc, Document};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

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

static CONNECTIONS: LazyLock<Mutex<HashMap<i64, Arc<MongoConnection>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CURSORS: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static NEXT_CONNECTION_HANDLE: AtomicI64 = AtomicI64::new(1);

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
    // SAFETY: allocation is checked before header or payload writes.
    let base = unsafe { libc::malloc(allocation_len) }.cast::<u8>();
    if base.is_null() {
        std::process::abort();
    }
    // SAFETY: `base` names `8 + capacity` bytes. Byte copies impose no
    // alignment requirement on the two u32 header values.
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

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    Connect = 1,
    InvalidJson = 2,
    Operation = 3,
    Serialize = 4,
    Internal = 5,
}

#[derive(Debug)]
struct ErrorState {
    kind: ErrorKind,
    status: i32,
    message: String,
    value_present: i32,
}

thread_local! {
    static LAST_ERROR: RefCell<ErrorState> = const { RefCell::new(ErrorState {
        kind: ErrorKind::None,
        status: 0,
        message: String::new(),
        value_present: 0,
    }) };
}

fn clear_error(value_present: bool) {
    LAST_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.kind = ErrorKind::None;
        state.status = 0;
        state.message.clear();
        state.value_present = i32::from(value_present);
    });
}

fn set_error(kind: ErrorKind, message: impl Into<String>) {
    LAST_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.kind = kind;
        state.status = -1;
        state.message = message.into();
        state.value_present = 0;
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
    // SAFETY: the caller guarantees `value` addresses at least `len` bytes.
    let bytes = unsafe { slice::from_raw_parts(value.cast::<u8>(), len) };
    std::str::from_utf8(bytes).map_err(|_| "string input was not valid UTF-8")
}

unsafe fn input<'a>(
    value: *const c_char,
    len: i64,
    kind: ErrorKind,
    label: &'static str,
) -> Option<&'a str> {
    match unsafe { utf8_with_len(value, len) } {
        Ok(value) => Some(value),
        Err(error) => {
            set_error(kind, format!("{label}: {error}"));
            None
        }
    }
}

fn json_to_doc(json: &str, label: &str) -> Result<Document, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("invalid {label} JSON: {error}"))?;
    mongodb::bson::to_document(&value)
        .map_err(|error| format!("{label} must be a JSON object: {error}"))
}

fn doc_to_json(document: Document) -> Result<String, String> {
    let value: serde_json::Value = mongodb::bson::from_document(document)
        .map_err(|error| format!("could not convert BSON document to JSON: {error}"))?;
    serde_json::to_string(&value)
        .map_err(|error| format!("could not serialize MongoDB document as JSON: {error}"))
}

#[derive(Debug)]
struct MongoConnection {
    client: mongodb::sync::Client,
    database: String,
}

#[derive(Debug)]
struct MongoCursor {
    items: Vec<String>,
    index: usize,
}

fn register_connection(value: MongoConnection) -> i64 {
    let handle = NEXT_CONNECTION_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        std::process::abort();
    }
    if let Ok(mut connections) = CONNECTIONS.lock() {
        connections.insert(handle, Arc::new(value));
        handle
    } else {
        set_error(
            ErrorKind::Internal,
            "MongoDB connection handle registry is unavailable",
        );
        0
    }
}

fn register_box<T>(value: T, registry: &Mutex<HashSet<usize>>, label: &str) -> Option<i64> {
    let pointer = Box::into_raw(Box::new(value));
    let address = pointer.addr();
    let Ok(handle) = i64::try_from(address) else {
        // SAFETY: conversion failed before the pointer escaped this function.
        drop(unsafe { Box::from_raw(pointer) });
        set_error(ErrorKind::Internal, format!("{label} address is too large"));
        return None;
    };
    if let Ok(mut handles) = registry.lock() {
        handles.insert(address);
        Some(handle)
    } else {
        // SAFETY: registration failed before the pointer escaped this function.
        drop(unsafe { Box::from_raw(pointer) });
        set_error(
            ErrorKind::Internal,
            format!("{label} handle registry is unavailable"),
        );
        None
    }
}

struct RegisteredMut<'a, T> {
    _registry: MutexGuard<'a, HashSet<usize>>,
    pointer: *mut T,
}

impl<T> Deref for RegisteredMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: registry membership owns this allocation and the held guard
        // prevents concurrent removal for the reference lifetime.
        unsafe { &*self.pointer }
    }
}

impl<T> DerefMut for RegisteredMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the exclusive registry guard serializes all registered
        // access, so no other mutable reference can coexist.
        unsafe { &mut *self.pointer }
    }
}

fn registered_mut<T>(
    handle: i64,
    registry: &'static Mutex<HashSet<usize>>,
    label: &str,
) -> Option<RegisteredMut<'static, T>> {
    let Ok(address) = usize::try_from(handle) else {
        set_error(ErrorKind::Internal, format!("invalid {label} handle"));
        return None;
    };
    let Ok(handles) = registry.lock() else {
        set_error(
            ErrorKind::Internal,
            format!("{label} handle registry is unavailable"),
        );
        return None;
    };
    if !handles.contains(&address) {
        set_error(ErrorKind::Internal, format!("stale {label} handle"));
        return None;
    }
    Some(RegisteredMut {
        _registry: handles,
        pointer: address as *mut T,
    })
}

fn connection(handle: i64) -> Option<Arc<MongoConnection>> {
    let Ok(connections) = CONNECTIONS.lock() else {
        set_error(
            ErrorKind::Internal,
            "MongoDB connection handle registry is unavailable",
        );
        return None;
    };
    let connection = connections.get(&handle).cloned();
    drop(connections);
    if connection.is_none() {
        set_error(ErrorKind::Internal, "stale MongoDB connection handle");
    }
    connection
}

fn cursor(handle: i64) -> Option<RegisteredMut<'static, MongoCursor>> {
    registered_mut(handle, &CURSORS, "MongoDB cursor")
}

/// Connect and ping a `MongoDB` server, returning an owned native handle.
///
/// # Safety
/// `uri` and `database` must point to their respective byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_connect(
    uri: *const c_char,
    uri_len: i64,
    database: *const c_char,
    database_len: i64,
) -> i64 {
    let Some(uri) = (unsafe { input(uri, uri_len, ErrorKind::Connect, "invalid MongoDB URI") })
    else {
        return 0;
    };
    let Some(database) = (unsafe {
        input(
            database,
            database_len,
            ErrorKind::Connect,
            "invalid database name",
        )
    }) else {
        return 0;
    };
    if database.is_empty() {
        set_error(
            ErrorKind::Connect,
            "MongoDB database name must not be empty",
        );
        return 0;
    }
    let client = match mongodb::sync::Client::with_uri_str(uri) {
        Ok(client) => client,
        Err(error) => {
            set_error(
                ErrorKind::Connect,
                format!("could not parse MongoDB URI: {error}"),
            );
            return 0;
        }
    };
    if let Err(error) = client
        .database("admin")
        .run_command(doc! { "ping": 1 })
        .run()
    {
        set_error(
            ErrorKind::Connect,
            format!("could not connect to MongoDB: {error}"),
        );
        return 0;
    }
    clear_error(true);
    register_connection(MongoConnection {
        client,
        database: database.to_owned(),
    })
}

/// Insert one JSON document.
///
/// # Safety
/// `conn` must be live and both string pointers must address their byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_insert_one(
    conn: i64,
    collection: *const c_char,
    collection_len: i64,
    document: *const c_char,
    document_len: i64,
) -> BytesTriple {
    let Some(conn) = connection(conn) else {
        return empty_bytes();
    };
    let Some(collection) = (unsafe {
        input(
            collection,
            collection_len,
            ErrorKind::Operation,
            "invalid collection",
        )
    }) else {
        return empty_bytes();
    };
    let Some(document) = (unsafe {
        input(
            document,
            document_len,
            ErrorKind::InvalidJson,
            "invalid document",
        )
    }) else {
        return empty_bytes();
    };
    let document = match json_to_doc(document, "document") {
        Ok(document) => document,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return empty_bytes();
        }
    };
    match conn
        .client
        .database(&conn.database)
        .collection::<Document>(collection)
        .insert_one(document)
        .run()
    {
        Ok(result) => {
            clear_error(true);
            owned_bytes(result.inserted_id.to_string().as_bytes())
        }
        Err(error) => {
            set_error(ErrorKind::Operation, format!("insert_one failed: {error}"));
            empty_bytes()
        }
    }
}

/// Find the first matching document.
///
/// # Safety
/// `conn` must be live and both string pointers must address their byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_find_one(
    conn: i64,
    collection: *const c_char,
    collection_len: i64,
    filter: *const c_char,
    filter_len: i64,
) -> BytesTriple {
    let Some(conn) = connection(conn) else {
        return empty_bytes();
    };
    let Some(collection) = (unsafe {
        input(
            collection,
            collection_len,
            ErrorKind::Operation,
            "invalid collection",
        )
    }) else {
        return empty_bytes();
    };
    let Some(filter) =
        (unsafe { input(filter, filter_len, ErrorKind::InvalidJson, "invalid filter") })
    else {
        return empty_bytes();
    };
    let filter = match json_to_doc(filter, "filter") {
        Ok(filter) => filter,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return empty_bytes();
        }
    };
    match conn
        .client
        .database(&conn.database)
        .collection::<Document>(collection)
        .find_one(filter)
        .run()
    {
        Ok(Some(document)) => match doc_to_json(document) {
            Ok(json) => {
                clear_error(true);
                owned_bytes(json.as_bytes())
            }
            Err(error) => {
                set_error(ErrorKind::Serialize, error);
                empty_bytes()
            }
        },
        Ok(None) => {
            clear_error(false);
            empty_bytes()
        }
        Err(error) => {
            set_error(ErrorKind::Operation, format!("find_one failed: {error}"));
            empty_bytes()
        }
    }
}

/// Start a query and prefetch its documents into an owned cursor.
///
/// # Safety
/// `conn` must be live and both string pointers must address their byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_find(
    conn: i64,
    collection: *const c_char,
    collection_len: i64,
    filter: *const c_char,
    filter_len: i64,
) -> i64 {
    let Some(conn) = connection(conn) else {
        return 0;
    };
    let Some(collection) = (unsafe {
        input(
            collection,
            collection_len,
            ErrorKind::Operation,
            "invalid collection",
        )
    }) else {
        return 0;
    };
    let Some(filter) =
        (unsafe { input(filter, filter_len, ErrorKind::InvalidJson, "invalid filter") })
    else {
        return 0;
    };
    let filter = match json_to_doc(filter, "filter") {
        Ok(filter) => filter,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return 0;
        }
    };
    let query = conn
        .client
        .database(&conn.database)
        .collection::<Document>(collection)
        .find(filter)
        .run();
    let cursor = match query {
        Ok(cursor) => cursor,
        Err(error) => {
            set_error(ErrorKind::Operation, format!("find failed: {error}"));
            return 0;
        }
    };
    let mut items = Vec::new();
    for row in cursor {
        let document = match row {
            Ok(document) => document,
            Err(error) => {
                set_error(ErrorKind::Operation, format!("cursor read failed: {error}"));
                return 0;
            }
        };
        match doc_to_json(document) {
            Ok(json) => items.push(json),
            Err(error) => {
                set_error(ErrorKind::Serialize, error);
                return 0;
            }
        }
    }
    clear_error(true);
    register_box(MongoCursor { items, index: 0 }, &CURSORS, "MongoDB cursor").unwrap_or(0)
}

/// Return one cursor item, distinguishing exhaustion through value presence.
///
/// # Safety
/// `cursor_handle` must be a live cursor handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_cursor_next(cursor_handle: i64) -> BytesTriple {
    let Some(mut cursor) = cursor(cursor_handle) else {
        return empty_bytes();
    };
    if cursor.index < cursor.items.len() {
        let index = cursor.index;
        cursor.index += 1;
        clear_error(true);
        owned_bytes(cursor.items[index].as_bytes())
    } else {
        clear_error(false);
        empty_bytes()
    }
}

fn parse_operation_inputs<'a>(
    collection: &'a str,
    filter: &'a str,
) -> Result<(&'a str, Document), String> {
    json_to_doc(filter, "filter").map(|filter| (collection, filter))
}

/// Update the first matching document.
///
/// # Safety
/// `conn` must be live and all string pointers must address their byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_update_one(
    conn: i64,
    collection: *const c_char,
    collection_len: i64,
    filter: *const c_char,
    filter_len: i64,
    update: *const c_char,
    update_len: i64,
) -> i64 {
    let Some(conn) = connection(conn) else {
        return 0;
    };
    let Some(collection) = (unsafe {
        input(
            collection,
            collection_len,
            ErrorKind::Operation,
            "invalid collection",
        )
    }) else {
        return 0;
    };
    let Some(filter) =
        (unsafe { input(filter, filter_len, ErrorKind::InvalidJson, "invalid filter") })
    else {
        return 0;
    };
    let Some(update) =
        (unsafe { input(update, update_len, ErrorKind::InvalidJson, "invalid update") })
    else {
        return 0;
    };
    let (collection, filter) = match parse_operation_inputs(collection, filter) {
        Ok(values) => values,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return 0;
        }
    };
    let update = match json_to_doc(update, "update") {
        Ok(update) => update,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return 0;
        }
    };
    match conn
        .client
        .database(&conn.database)
        .collection::<Document>(collection)
        .update_one(filter, update)
        .run()
    {
        Ok(result) => {
            if let Ok(count) = i64::try_from(result.modified_count) {
                clear_error(true);
                count
            } else {
                set_error(ErrorKind::Operation, "modified count exceeded i64::MAX");
                0
            }
        }
        Err(error) => {
            set_error(ErrorKind::Operation, format!("update_one failed: {error}"));
            0
        }
    }
}

/// Delete the first matching document.
///
/// # Safety
/// `conn` must be live and both string pointers must address their byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_delete_one(
    conn: i64,
    collection: *const c_char,
    collection_len: i64,
    filter: *const c_char,
    filter_len: i64,
) -> i64 {
    let Some(conn) = connection(conn) else {
        return 0;
    };
    let Some(collection) = (unsafe {
        input(
            collection,
            collection_len,
            ErrorKind::Operation,
            "invalid collection",
        )
    }) else {
        return 0;
    };
    let Some(filter) =
        (unsafe { input(filter, filter_len, ErrorKind::InvalidJson, "invalid filter") })
    else {
        return 0;
    };
    let (collection, filter) = match parse_operation_inputs(collection, filter) {
        Ok(values) => values,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return 0;
        }
    };
    match conn
        .client
        .database(&conn.database)
        .collection::<Document>(collection)
        .delete_one(filter)
        .run()
    {
        Ok(result) => {
            if let Ok(count) = i64::try_from(result.deleted_count) {
                clear_error(true);
                count
            } else {
                set_error(ErrorKind::Operation, "deleted count exceeded i64::MAX");
                0
            }
        }
        Err(error) => {
            set_error(ErrorKind::Operation, format!("delete_one failed: {error}"));
            0
        }
    }
}

/// Count matching documents.
///
/// # Safety
/// `conn` must be live and both string pointers must address their byte lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_count(
    conn: i64,
    collection: *const c_char,
    collection_len: i64,
    filter: *const c_char,
    filter_len: i64,
) -> i64 {
    let Some(conn) = connection(conn) else {
        return 0;
    };
    let Some(collection) = (unsafe {
        input(
            collection,
            collection_len,
            ErrorKind::Operation,
            "invalid collection",
        )
    }) else {
        return 0;
    };
    let Some(filter) =
        (unsafe { input(filter, filter_len, ErrorKind::InvalidJson, "invalid filter") })
    else {
        return 0;
    };
    let (collection, filter) = match parse_operation_inputs(collection, filter) {
        Ok(values) => values,
        Err(error) => {
            set_error(ErrorKind::InvalidJson, error);
            return 0;
        }
    };
    match conn
        .client
        .database(&conn.database)
        .collection::<Document>(collection)
        .count_documents(filter)
        .run()
    {
        Ok(count) => {
            if let Ok(count) = i64::try_from(count) {
                clear_error(true);
                count
            } else {
                set_error(ErrorKind::Operation, "document count exceeded i64::MAX");
                0
            }
        }
        Err(error) => {
            set_error(ErrorKind::Operation, format!("count failed: {error}"));
            0
        }
    }
}

/// Free a cursor. Zero is accepted as a no-op.
///
/// # Safety
/// Non-zero handles must be live and freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_cursor_free(cursor: i64) {
    if cursor == 0 {
        return;
    }
    let Ok(address) = usize::try_from(cursor) else {
        set_error(ErrorKind::Internal, "invalid MongoDB cursor handle");
        return;
    };
    let removed = if let Ok(mut handles) = CURSORS.lock() {
        handles.remove(&address)
    } else {
        set_error(
            ErrorKind::Internal,
            "MongoDB cursor handle registry is unavailable",
        );
        return;
    };
    if !removed {
        set_error(ErrorKind::Internal, "stale MongoDB cursor handle");
        return;
    }
    // SAFETY: the handle was allocated by hew_mongodb_find and is uniquely
    // owned by the Hew NativeCursor resource.
    drop(unsafe { Box::from_raw(address as *mut MongoCursor) });
}

/// Close a connection. Zero is accepted as a no-op.
///
/// # Safety
/// Non-zero handles must be live and closed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_mongodb_close(conn: i64) {
    if conn == 0 {
        return;
    }
    let removed = if let Ok(mut connections) = CONNECTIONS.lock() {
        connections.remove(&conn).is_some()
    } else {
        set_error(
            ErrorKind::Internal,
            "MongoDB connection handle registry is unavailable",
        );
        return;
    };
    if !removed {
        set_error(ErrorKind::Internal, "stale MongoDB connection handle");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_mongodb_last_error() -> *mut c_char {
    LAST_ERROR.with(|state| malloc_c_string(&state.borrow().message))
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_mongodb_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_mongodb_last_status() -> i32 {
    LAST_ERROR.with(|state| state.borrow().status)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_mongodb_last_value_present() -> i32 {
    LAST_ERROR.with(|state| state.borrow().value_present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    unsafe fn owned_string(pointer: *mut c_char) -> String {
        assert!(!pointer.is_null());
        // SAFETY: native string returns are malloc-owned NUL-terminated strings.
        let value = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .expect("native result should be UTF-8")
            .to_owned();
        // SAFETY: package string results are allocated with libc::malloc.
        unsafe { libc::free(pointer.cast()) };
        value
    }

    unsafe fn owned_byte_vec(value: BytesTriple) -> Vec<u8> {
        if value.len == 0 {
            assert!(value.ptr.is_null());
            return Vec::new();
        }
        assert!(!value.ptr.is_null());
        // SAFETY: the triple came from `owned_bytes`, so its active range is
        // initialized and its allocation starts eight bytes before `ptr`.
        let bytes = unsafe {
            slice::from_raw_parts(value.ptr.add(value.offset as usize), value.len as usize).to_vec()
        };
        // SAFETY: this is the unique test-side release of the refcount-1
        // allocation produced by `owned_bytes`.
        unsafe { libc::free(value.ptr.sub(8).cast()) };
        bytes
    }

    #[test]
    fn valid_json_object_round_trips_exact_value() {
        let original = r#"{"name":"Alice","age":30}"#;
        let document = json_to_doc(original, "document").expect("valid object");
        let encoded = doc_to_json(document).expect("serializable document");
        let expected: serde_json::Value = serde_json::from_str(original).unwrap();
        let actual: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn non_object_json_returns_typed_validation_message() {
        let error = json_to_doc("[1,2,3]", "filter").expect_err("array is not a document");
        assert!(error.contains("filter must be a JSON object"));
    }

    #[test]
    fn pointer_length_input_preserves_embedded_nul() {
        let bytes = b"a\0b";
        // SAFETY: bytes is live for the requested three-byte slice.
        let value = unsafe { utf8_with_len(bytes.as_ptr().cast(), 3) }.unwrap();
        assert_eq!(value.as_bytes(), bytes);
    }

    #[test]
    fn owned_bytes_output_preserves_embedded_nul() {
        let original = b"a\0b";
        // SAFETY: this test uniquely consumes the returned bytes allocation.
        let returned = unsafe { owned_byte_vec(owned_bytes(original)) };
        assert_eq!(returned, original);
    }

    #[test]
    fn package_string_return_uses_bare_malloc_ownership() {
        set_error(ErrorKind::Operation, "allocator probe");
        let pointer = hew_mongodb_last_error();
        assert!(!pointer.is_null());
        // SAFETY: the export returns a NUL-terminated foreign-package string.
        assert_eq!(
            unsafe { CStr::from_ptr(pointer) }.to_str().unwrap(),
            "allocator probe"
        );
        // SAFETY: Hew releases package string returns with libc::free.
        unsafe { libc::free(pointer.cast()) };
    }

    #[test]
    fn serialized_json_bytes_escape_embedded_nul_without_truncation() {
        let document = doc! { "value": "a\0b" };
        let json = doc_to_json(document).expect("BSON string should serialize");
        assert_eq!(json, r#"{"value":"a\u0000b"}"#);
        // SAFETY: this test uniquely consumes the returned bytes allocation.
        let returned = unsafe { owned_byte_vec(owned_bytes(json.as_bytes())) };
        assert_eq!(returned, br#"{"value":"a\u0000b"}"#);
        assert!(!returned.contains(&0));
    }

    #[test]
    fn raw_nul_in_json_input_returns_validation_error() {
        let error =
            json_to_doc("{\"value\":\"a\0b\"}", "document").expect_err("raw NUL is invalid JSON");
        assert!(error.contains("invalid document JSON"));
    }

    #[test]
    fn null_pointer_with_length_returns_internal_error() {
        // SAFETY: null is deliberate and rejected before dereference.
        let result = unsafe { hew_mongodb_insert_one(0, std::ptr::null(), 1, std::ptr::null(), 1) };
        // SAFETY: result is an owned native bytes value.
        assert_eq!(unsafe { owned_byte_vec(result) }, b"");
        assert_eq!(hew_mongodb_last_status(), -1);
        assert_eq!(hew_mongodb_last_error_kind(), ErrorKind::Internal as i32);
        // SAFETY: error pointer is an owned native string.
        let message = unsafe { owned_string(hew_mongodb_last_error()) };
        assert_eq!(message, "stale MongoDB connection handle");
    }

    #[test]
    fn cursor_items_and_exhaustion_are_distinct() {
        let handle = register_box(
            MongoCursor {
                items: vec![String::new(), r#"{"n":2}"#.to_owned()],
                index: 0,
            },
            &CURSORS,
            "MongoDB cursor",
        )
        .unwrap();
        // SAFETY: handle owns a live cursor.
        let first = unsafe { owned_byte_vec(hew_mongodb_cursor_next(handle)) };
        assert_eq!(first, b"");
        assert_eq!(hew_mongodb_last_status(), 0);
        assert_eq!(hew_mongodb_last_value_present(), 1);
        // SAFETY: handle remains live.
        let second = unsafe { owned_byte_vec(hew_mongodb_cursor_next(handle)) };
        assert_eq!(second, br#"{"n":2}"#);
        assert_eq!(hew_mongodb_last_value_present(), 1);
        // SAFETY: handle remains live.
        let exhausted = unsafe { owned_byte_vec(hew_mongodb_cursor_next(handle)) };
        assert_eq!(exhausted, b"");
        assert_eq!(hew_mongodb_last_status(), 0);
        assert_eq!(hew_mongodb_last_value_present(), 0);
        // SAFETY: this is the cursor's unique close.
        unsafe { hew_mongodb_cursor_free(handle) };
    }

    #[test]
    fn invalid_cursor_returns_error_not_exhaustion() {
        // SAFETY: zero is deliberately invalid for next.
        let value = unsafe { owned_byte_vec(hew_mongodb_cursor_next(0)) };
        assert_eq!(value, b"");
        assert_eq!(hew_mongodb_last_status(), -1);
        assert_eq!(hew_mongodb_last_error_kind(), ErrorKind::Internal as i32);
        assert_eq!(hew_mongodb_last_value_present(), 0);
    }

    #[test]
    fn closed_cursor_handle_is_rejected_without_double_free() {
        let handle = register_box(
            MongoCursor {
                items: Vec::new(),
                index: 0,
            },
            &CURSORS,
            "MongoDB cursor",
        )
        .unwrap();
        // SAFETY: first call uniquely frees the registered cursor.
        unsafe { hew_mongodb_cursor_free(handle) };
        // SAFETY: second call deliberately exercises stale-handle rejection.
        unsafe { hew_mongodb_cursor_free(handle) };
        assert_eq!(hew_mongodb_last_status(), -1);
        // SAFETY: error pointer is an owned native string.
        assert_eq!(
            unsafe { owned_string(hew_mongodb_last_error()) },
            "stale MongoDB cursor handle"
        );
    }

    #[test]
    fn cursor_close_waits_for_registered_use_guard() {
        use std::sync::mpsc;
        use std::time::Duration;

        let handle = register_box(
            MongoCursor {
                items: vec!["held".to_owned()],
                index: 0,
            },
            &CURSORS,
            "MongoDB cursor",
        )
        .unwrap();
        let guard = cursor(handle).expect("cursor should be registered");
        let (finished_tx, finished_rx) = mpsc::channel();
        let closer = std::thread::spawn(move || {
            // SAFETY: close competes with a registered guard and must wait.
            unsafe { hew_mongodb_cursor_free(handle) };
            finished_tx.send(()).unwrap();
        });
        assert!(finished_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(guard.items[0], "held");
        drop(guard);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("close should finish after the use guard drops");
        closer.join().unwrap();
        assert!(cursor(handle).is_none());
    }

    #[test]
    fn invalid_uri_returns_connect_error() {
        let uri = b"not a uri";
        let database = b"test";
        // SAFETY: pointers address the supplied lengths.
        let handle = unsafe {
            hew_mongodb_connect(
                uri.as_ptr().cast(),
                i64::try_from(uri.len()).unwrap(),
                database.as_ptr().cast(),
                i64::try_from(database.len()).unwrap(),
            )
        };
        assert_eq!(handle, 0);
        assert_eq!(hew_mongodb_last_status(), -1);
        assert_eq!(hew_mongodb_last_error_kind(), ErrorKind::Connect as i32);
    }

    #[test]
    fn empty_database_returns_connect_error() {
        let uri = b"mongodb://127.0.0.1:27017";
        // SAFETY: URI pointer addresses its length; null with zero length is valid.
        let handle = unsafe {
            hew_mongodb_connect(
                uri.as_ptr().cast(),
                i64::try_from(uri.len()).unwrap(),
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(handle, 0);
        assert_eq!(hew_mongodb_last_error_kind(), ErrorKind::Connect as i32);
        // SAFETY: error pointer is an owned native string.
        assert_eq!(
            unsafe { owned_string(hew_mongodb_last_error()) },
            "MongoDB database name must not be empty"
        );
    }

    #[test]
    fn refused_server_returns_connect_error() {
        let uri = b"mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=10&connectTimeoutMS=10";
        let database = b"test";
        // SAFETY: both pointers address their supplied lengths.
        let handle = unsafe {
            hew_mongodb_connect(
                uri.as_ptr().cast(),
                i64::try_from(uri.len()).unwrap(),
                database.as_ptr().cast(),
                i64::try_from(database.len()).unwrap(),
            )
        };
        assert_eq!(handle, 0);
        assert_eq!(hew_mongodb_last_status(), -1);
        assert_eq!(hew_mongodb_last_error_kind(), ErrorKind::Connect as i32);
        // SAFETY: error pointer is an owned native string.
        let message = unsafe { owned_string(hew_mongodb_last_error()) };
        assert!(message.starts_with("could not connect to MongoDB:"));
    }
}
