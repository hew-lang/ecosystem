//! Native support for `hew.queue.nats`.
//!
//! Handles are validated monotonic IDs. Strings cross the boundary as managed
//! Hew strings: inbound handles are borrowed for the call, and returned
//! handles are freshly allocated owners the caller releases. Registry locks
//! are released before every broker operation and bounded wait.

use hew_cabi::string::{string_as_str, string_from_str, HewString};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    Connection = 1,
    InvalidInput = 2,
    Operation = 3,
    #[expect(
        dead_code,
        reason = "reserved ABI error category for bounded broker operations"
    )]
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

#[derive(Debug, Clone)]
struct NatsMessage {
    subject: String,
    reply: Option<String>,
    data: Vec<u8>,
}

struct NatsConnection {
    inner: nats::Connection,
    subscriptions: Mutex<HashSet<i64>>,
}

impl std::fmt::Debug for NatsConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NatsConnection")
            .finish_non_exhaustive()
    }
}

struct NatsSubscription {
    inner: nats::Subscription,
}

impl std::fmt::Debug for NatsSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NatsSubscription")
            .finish_non_exhaustive()
    }
}

impl Drop for NatsConnection {
    fn drop(&mut self) {
        let ids = self
            .subscriptions
            .get_mut()
            .map(|subscriptions| subscriptions.drain().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Ok(mut registry) = subscriptions().lock() {
            for id in ids {
                registry.remove(&id);
            }
        }
    }
}

static CONNECTIONS: OnceLock<Mutex<HashMap<i64, Arc<NatsConnection>>>> = OnceLock::new();
static SUBSCRIPTIONS: OnceLock<Mutex<HashMap<i64, Arc<NatsSubscription>>>> = OnceLock::new();
static MESSAGES: OnceLock<Mutex<HashMap<i64, NatsMessage>>> = OnceLock::new();
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn connections() -> &'static Mutex<HashMap<i64, Arc<NatsConnection>>> {
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn subscriptions() -> &'static Mutex<HashMap<i64, Arc<NatsSubscription>>> {
    SUBSCRIPTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn messages() -> &'static Mutex<HashMap<i64, NatsMessage>> {
    MESSAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> i64 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        std::process::abort();
    }
    handle
}

fn connection(handle: i64) -> Option<Arc<NatsConnection>> {
    let value = connections()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&handle).cloned());
    if value.is_none() {
        set_error(ErrorKind::Closed, "NATS connection is closed");
    }
    value
}

fn subscription(handle: i64) -> Option<Arc<NatsSubscription>> {
    let value = subscriptions()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&handle).cloned());
    if value.is_none() {
        set_error(ErrorKind::Closed, "NATS subscription is closed");
    }
    value
}

fn register_message(message: NatsMessage) -> i64 {
    let handle = next_handle();
    if let Ok(mut registry) = messages().lock() {
        registry.insert(handle, message);
        handle
    } else {
        set_error(ErrorKind::Operation, "NATS message registry is unavailable");
        0
    }
}

fn message(handle: i64) -> Option<NatsMessage> {
    let value = messages()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&handle).cloned());
    if value.is_none() {
        set_error(ErrorKind::Closed, "NATS message is closed");
    }
    value
}

fn snapshot(message: nats::Message) -> NatsMessage {
    NatsMessage {
        subject: message.subject,
        reply: message.reply,
        data: message.data,
    }
}

#[no_mangle]
pub extern "C" fn hew_nats_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[no_mangle]
pub extern "C" fn hew_nats_last_error() -> *mut HewString {
    LAST_ERROR.with(|state| string_from_str(&state.borrow().message))
}

/// # Safety
/// `url` must be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_nats_connect(url: *const HewString) -> i64 {
    // SAFETY: required by this function's contract.
    let url = unsafe { string_as_str(url) };
    if url.is_empty() {
        set_error(ErrorKind::InvalidInput, "NATS URL must not be empty");
        return 0;
    }
    match nats::connect(url) {
        Ok(inner) => {
            clear_error();
            let handle = next_handle();
            if let Ok(mut registry) = connections().lock() {
                registry.insert(
                    handle,
                    Arc::new(NatsConnection {
                        inner,
                        subscriptions: Mutex::new(HashSet::new()),
                    }),
                );
                handle
            } else {
                set_error(
                    ErrorKind::Connection,
                    "NATS connection registry is unavailable",
                );
                0
            }
        }
        Err(error) => {
            set_error(
                ErrorKind::Connection,
                format!("NATS connection failed: {error}"),
            );
            0
        }
    }
}

/// # Safety
/// `subject` and `data` must each be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_nats_publish(
    handle: i64,
    subject: *const HewString,
    data: *const HewString,
) -> i32 {
    let Some(connection) = connection(handle) else {
        return -1;
    };
    // SAFETY: required by this function's contract.
    let subject = unsafe { string_as_str(subject) };
    // SAFETY: required by this function's contract.
    let data = unsafe { string_as_str(data) };
    match connection.inner.publish(subject, data.as_bytes()) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(
                ErrorKind::Operation,
                format!("NATS publish failed: {error}"),
            );
            -1
        }
    }
}

/// # Safety
/// `subject` must be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_nats_subscribe(
    connection_handle: i64,
    subject: *const HewString,
) -> i64 {
    let Some(connection) = connection(connection_handle) else {
        return 0;
    };
    // SAFETY: required by this function's contract.
    let subject = unsafe { string_as_str(subject) };
    let inner = match connection.inner.subscribe(subject) {
        Ok(inner) => inner,
        Err(error) => {
            set_error(
                ErrorKind::Operation,
                format!("NATS subscribe failed: {error}"),
            );
            return 0;
        }
    };
    let handle = next_handle();
    if let Ok(mut registry) = subscriptions().lock() {
        registry.insert(handle, Arc::new(NatsSubscription { inner }));
    } else {
        set_error(
            ErrorKind::Operation,
            "NATS subscription registry is unavailable",
        );
        return 0;
    }
    if let Ok(mut owned) = connection.subscriptions.lock() {
        owned.insert(handle);
    } else {
        if let Ok(mut registry) = subscriptions().lock() {
            registry.remove(&handle);
        }
        set_error(
            ErrorKind::Operation,
            "NATS connection ownership state is unavailable",
        );
        return 0;
    }
    clear_error();
    handle
}

#[no_mangle]
pub extern "C" fn hew_nats_next_result(handle: i64, timeout_ms: i64) -> i64 {
    let Some(subscription) = subscription(handle) else {
        return 0;
    };
    let Ok(timeout_ms) = u64::try_from(timeout_ms) else {
        set_error(
            ErrorKind::InvalidInput,
            "NATS receive timeout cannot be negative",
        );
        return 0;
    };
    match subscription
        .inner
        .next_timeout(Duration::from_millis(timeout_ms))
    {
        Ok(message) => {
            clear_error();
            register_message(snapshot(message))
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(
                ErrorKind::Operation,
                format!("NATS receive failed: {error}"),
            );
            0
        }
    }
}

/// # Safety
/// `subject` and `data` must each be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_nats_request(
    handle: i64,
    subject: *const HewString,
    data: *const HewString,
    timeout_ms: i64,
) -> i64 {
    let Some(connection) = connection(handle) else {
        return 0;
    };
    // SAFETY: required by this function's contract.
    let subject = unsafe { string_as_str(subject) };
    // SAFETY: required by this function's contract.
    let data = unsafe { string_as_str(data) };
    let Ok(timeout_ms) = u64::try_from(timeout_ms) else {
        set_error(
            ErrorKind::InvalidInput,
            "NATS request timeout cannot be negative",
        );
        return 0;
    };
    match connection.inner.request_timeout(
        subject,
        data.as_bytes(),
        Duration::from_millis(timeout_ms),
    ) {
        Ok(message) => {
            clear_error();
            register_message(snapshot(message))
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(
                ErrorKind::Operation,
                format!("NATS request failed: {error}"),
            );
            0
        }
    }
}

/// # Safety
/// `reply_subject` and `data` must each be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_nats_reply(
    handle: i64,
    reply_subject: *const HewString,
    data: *const HewString,
) -> i32 {
    let Some(connection) = connection(handle) else {
        return -1;
    };
    // SAFETY: required by this function's contract.
    let reply_subject = unsafe { string_as_str(reply_subject) };
    // SAFETY: required by this function's contract.
    let data = unsafe { string_as_str(data) };
    if reply_subject.is_empty() {
        set_error(ErrorKind::InvalidInput, "NATS message has no reply subject");
        return -1;
    }
    match connection.inner.publish(reply_subject, data.as_bytes()) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(ErrorKind::Operation, format!("NATS reply failed: {error}"));
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn hew_nats_message_subject(handle: i64) -> *mut HewString {
    message(handle).map_or(std::ptr::null_mut(), |message| {
        clear_error();
        string_from_str(&message.subject)
    })
}

#[no_mangle]
pub extern "C" fn hew_nats_message_data(handle: i64) -> *mut HewString {
    message(handle).map_or(std::ptr::null_mut(), |message| {
        clear_error();
        string_from_str(String::from_utf8_lossy(&message.data).as_ref())
    })
}

#[no_mangle]
pub extern "C" fn hew_nats_message_reply(handle: i64) -> *mut HewString {
    message(handle).map_or(std::ptr::null_mut(), |message| {
        clear_error();
        string_from_str(message.reply.as_deref().unwrap_or(""))
    })
}

#[no_mangle]
pub extern "C" fn hew_nats_message_has_reply(handle: i64) -> i32 {
    message(handle).map_or(-1, |message| i32::from(message.reply.is_some()))
}

#[no_mangle]
pub extern "C" fn hew_nats_message_free(handle: i64) {
    if let Ok(mut registry) = messages().lock() {
        registry.remove(&handle);
    }
}

#[no_mangle]
pub extern "C" fn hew_nats_unsubscribe(handle: i64) {
    let removed = subscriptions()
        .lock()
        .ok()
        .and_then(|mut registry| registry.remove(&handle));
    drop(removed);
}

#[no_mangle]
pub extern "C" fn hew_nats_close(handle: i64) {
    let removed = connections()
        .lock()
        .ok()
        .and_then(|mut registry| registry.remove(&handle));
    drop(removed);
}

#[no_mangle]
pub extern "C" fn hew_nats_connection_count() -> i64 {
    connections()
        .lock()
        .ok()
        .and_then(|registry| i64::try_from(registry.len()).ok())
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn hew_nats_subscription_count() -> i64 {
    subscriptions()
        .lock()
        .ok()
        .and_then(|registry| i64::try_from(registry.len()).ok())
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn hew_nats_message_count() -> i64 {
    messages()
        .lock()
        .ok()
        .and_then(|registry| i64::try_from(registry.len()).ok())
        .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hew_cabi::string::string_release;

    /// Allocate one managed string argument for a boundary call.
    #[cfg(feature = "integration")]
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

    #[test]
    fn message_strings_round_trip_and_lifecycle_is_guarded() {
        let before = hew_nats_message_count();
        let handle = register_message(NatsMessage {
            subject: "events.created".to_owned(),
            reply: None,
            data: b"payload".to_vec(),
        });
        let data = hew_nats_message_data(handle);
        // SAFETY: the accessor just returned this owner.
        assert_eq!(unsafe { owned_text(data) }, "payload");
        assert_eq!(hew_nats_message_has_reply(handle), 0);
        hew_nats_message_free(handle);
        hew_nats_message_free(handle);
        assert_eq!(hew_nats_message_count(), before);
        assert!(hew_nats_message_subject(handle).is_null());
    }

    #[test]
    fn stale_handles_fail_closed() {
        assert_eq!(hew_nats_next_result(i64::MAX, 0), 0);
        assert_eq!(hew_nats_last_error_kind(), ErrorKind::Closed as i32);
        hew_nats_unsubscribe(i64::MAX);
        hew_nats_close(i64::MAX);
    }

    #[cfg(feature = "integration")]
    fn connect() -> i64 {
        let url = managed("nats://127.0.0.1:14222");
        // SAFETY: `url` is a live managed string for the call.
        let handle = unsafe { hew_nats_connect(url) };
        // SAFETY: this test owns the handle.
        unsafe { string_release(url) };
        assert_ne!(handle, 0);
        handle
    }

    #[test]
    #[cfg(feature = "integration")]
    fn publish_subscribe_roundtrip_and_cleanup() {
        let before_connections = hew_nats_connection_count();
        let before_subscriptions = hew_nats_subscription_count();
        let connection = connect();
        let subject = format!("hew.tests.{connection}");
        // A payload over 16 bytes with non-ASCII characters exercises the
        // exact managed-string round trip rather than a short ASCII stand-in.
        let payload = "hello nats — 雪の結晶";
        let subject_arg = managed(&subject);
        // SAFETY: `subject_arg` is a live managed string for the call.
        let subscription = unsafe { hew_nats_subscribe(connection, subject_arg) };
        assert_ne!(subscription, 0);
        let payload_arg = managed(payload);
        // SAFETY: `subject_arg` and `payload_arg` are live managed strings.
        let status = unsafe { hew_nats_publish(connection, subject_arg, payload_arg) };
        assert_eq!(status, 0);
        // SAFETY: this test owns both handles.
        unsafe {
            string_release(subject_arg);
            string_release(payload_arg);
        }
        let message = hew_nats_next_result(subscription, 3000);
        assert_ne!(message, 0);
        // SAFETY: the accessor just returned this owner.
        let received = unsafe { owned_text(hew_nats_message_data(message)) };
        assert_eq!(received, payload);
        hew_nats_message_free(message);
        hew_nats_close(connection);
        assert_eq!(hew_nats_connection_count(), before_connections);
        assert_eq!(hew_nats_subscription_count(), before_subscriptions);
    }
}
