//! Native support for `hew.queue.mqtt`.
//!
//! Public handles are monotonic registry IDs rather than pointers. Registry
//! locks protect only metadata: they are released before broker operations or
//! bounded waits. Strings cross the boundary as managed Hew strings: inbound
//! handles are borrowed for the call, and returned handles are freshly
//! allocated owners the caller releases.

use hew_cabi::string::{string_as_str, string_from_str, HewString};
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
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

fn qos(value: i32) -> Option<QoS> {
    match value {
        0 => Some(QoS::AtMostOnce),
        1 => Some(QoS::AtLeastOnce),
        2 => Some(QoS::ExactlyOnce),
        _ => {
            set_error(
                ErrorKind::InvalidInput,
                format!("MQTT QoS must be 0, 1, or 2; received {value}"),
            );
            None
        }
    }
}

#[derive(Debug, Clone)]
struct MqttMessage {
    topic: String,
    payload: Vec<u8>,
}

struct MqttConnection {
    client: Client,
    receiver: Mutex<mpsc::Receiver<MqttMessage>>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for MqttConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MqttConnection")
            .finish_non_exhaustive()
    }
}

impl Drop for MqttConnection {
    fn drop(&mut self) {
        let _ = self.client.disconnect();
        if let Ok(reader) = self.reader.get_mut() {
            if let Some(reader) = reader.take() {
                let _ = reader.join();
            }
        }
    }
}

type ConnectionRegistry = OnceLock<Mutex<HashMap<i64, Arc<MqttConnection>>>>;
static CONNECTIONS: ConnectionRegistry = OnceLock::new();
static MESSAGES: OnceLock<Mutex<HashMap<i64, MqttMessage>>> = OnceLock::new();
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn connections() -> &'static Mutex<HashMap<i64, Arc<MqttConnection>>> {
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn messages() -> &'static Mutex<HashMap<i64, MqttMessage>> {
    MESSAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> i64 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        std::process::abort();
    }
    handle
}

fn register_connection(connection: MqttConnection) -> i64 {
    let handle = next_handle();
    if let Ok(mut registry) = connections().lock() {
        registry.insert(handle, Arc::new(connection));
        handle
    } else {
        set_error(
            ErrorKind::Connection,
            "MQTT connection registry is unavailable",
        );
        0
    }
}

fn connection(handle: i64) -> Option<Arc<MqttConnection>> {
    let value = connections()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&handle).cloned());
    if value.is_none() {
        set_error(ErrorKind::Closed, "MQTT connection is closed");
    }
    value
}

fn register_message(message: MqttMessage) -> i64 {
    let handle = next_handle();
    if let Ok(mut registry) = messages().lock() {
        registry.insert(handle, message);
        handle
    } else {
        set_error(ErrorKind::Operation, "MQTT message registry is unavailable");
        0
    }
}

fn message(handle: i64) -> Option<MqttMessage> {
    let value = messages()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&handle).cloned());
    if value.is_none() {
        set_error(ErrorKind::Closed, "MQTT message is closed");
    }
    value
}

#[no_mangle]
pub extern "C" fn hew_mqtt_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[no_mangle]
pub extern "C" fn hew_mqtt_last_error() -> *mut HewString {
    LAST_ERROR.with(|state| string_from_str(&state.borrow().message))
}

/// # Safety
/// `host` and `client_id` must each be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mqtt_connect(
    host: *const HewString,
    port: i64,
    client_id: *const HewString,
    keepalive_secs: i64,
) -> i64 {
    // SAFETY: required by this function's contract.
    let host = unsafe { string_as_str(host) };
    // SAFETY: required by this function's contract.
    let client_id = unsafe { string_as_str(client_id) };
    let Ok(port) = u16::try_from(port) else {
        set_error(
            ErrorKind::InvalidInput,
            "MQTT port must be between 0 and 65535",
        );
        return 0;
    };
    if host.is_empty() || client_id.is_empty() || keepalive_secs <= 0 {
        set_error(
            ErrorKind::InvalidInput,
            "MQTT host and client ID must be non-empty and keepalive must be positive",
        );
        return 0;
    }

    let mut options = MqttOptions::new(client_id.to_owned(), host.to_owned(), port);
    let Ok(keepalive_secs) = u64::try_from(keepalive_secs) else {
        set_error(ErrorKind::InvalidInput, "MQTT keepalive is out of range");
        return 0;
    };
    options.set_keep_alive(Duration::from_secs(keepalive_secs));
    let (client, mut eventloop) = Client::new(options, 10);
    match eventloop.recv() {
        Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {}
        Ok(Ok(_)) => {
            set_error(
                ErrorKind::Connection,
                "MQTT broker did not acknowledge the connection",
            );
            return 0;
        }
        Ok(Err(error)) => {
            set_error(
                ErrorKind::Connection,
                format!("MQTT connection failed: {error}"),
            );
            return 0;
        }
        Err(error) => {
            set_error(
                ErrorKind::Connection,
                format!("MQTT connection ended: {error:?}"),
            );
            return 0;
        }
    }

    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for event in eventloop.iter() {
            match event {
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    if sender
                        .send(MqttMessage {
                            topic: publish.topic,
                            payload: publish.payload.to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    clear_error();
    register_connection(MqttConnection {
        client,
        receiver: Mutex::new(receiver),
        reader: Mutex::new(Some(reader)),
    })
}

/// # Safety
/// `topic` and `payload` must each be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mqtt_publish(
    handle: i64,
    topic: *const HewString,
    payload: *const HewString,
    qos_value: i32,
    retain: i32,
) -> i32 {
    let Some(connection) = connection(handle) else {
        return -1;
    };
    // SAFETY: required by this function's contract.
    let topic = unsafe { string_as_str(topic) };
    // SAFETY: required by this function's contract.
    let payload = unsafe { string_as_str(payload) };
    let Some(qos) = qos(qos_value) else {
        return -1;
    };
    match connection
        .client
        .publish(topic, qos, retain != 0, payload.as_bytes())
    {
        Ok(()) => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(
                ErrorKind::Operation,
                format!("MQTT publish failed: {error}"),
            );
            -1
        }
    }
}

/// # Safety
/// `topic` must be null or a live managed Hew string.
#[no_mangle]
pub unsafe extern "C" fn hew_mqtt_subscribe(
    handle: i64,
    topic: *const HewString,
    qos_value: i32,
) -> i32 {
    let Some(connection) = connection(handle) else {
        return -1;
    };
    // SAFETY: required by this function's contract.
    let topic = unsafe { string_as_str(topic) };
    let Some(qos) = qos(qos_value) else {
        return -1;
    };
    match connection.client.subscribe(topic, qos) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(
                ErrorKind::Operation,
                format!("MQTT subscribe failed: {error}"),
            );
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn hew_mqtt_next_result(handle: i64, timeout_ms: i64) -> i64 {
    let Some(connection) = connection(handle) else {
        return 0;
    };
    let Ok(timeout_ms) = u64::try_from(timeout_ms) else {
        set_error(
            ErrorKind::InvalidInput,
            "MQTT receive timeout cannot be negative",
        );
        return 0;
    };
    let Ok(receiver) = connection.receiver.lock() else {
        set_error(ErrorKind::Operation, "MQTT receive channel is unavailable");
        return 0;
    };
    match receiver.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(message) => {
            clear_error();
            register_message(message)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            clear_error();
            0
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            set_error(ErrorKind::Connection, "MQTT connection was lost");
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn hew_mqtt_message_topic(handle: i64) -> *mut HewString {
    message(handle).map_or(std::ptr::null_mut(), |message| {
        clear_error();
        string_from_str(&message.topic)
    })
}

#[no_mangle]
pub extern "C" fn hew_mqtt_message_payload(handle: i64) -> *mut HewString {
    message(handle).map_or(std::ptr::null_mut(), |message| {
        clear_error();
        string_from_str(String::from_utf8_lossy(&message.payload).as_ref())
    })
}

#[no_mangle]
pub extern "C" fn hew_mqtt_message_free(handle: i64) {
    if let Ok(mut registry) = messages().lock() {
        registry.remove(&handle);
    }
}

#[no_mangle]
pub extern "C" fn hew_mqtt_close(handle: i64) {
    let removed = connections()
        .lock()
        .ok()
        .and_then(|mut registry| registry.remove(&handle));
    drop(removed);
}

#[no_mangle]
pub extern "C" fn hew_mqtt_connection_count() -> i64 {
    connections()
        .lock()
        .ok()
        .and_then(|registry| i64::try_from(registry.len()).ok())
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn hew_mqtt_message_count() -> i64 {
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
    fn invalid_qos_is_rejected() {
        assert!(qos(-1).is_none());
        assert_eq!(hew_mqtt_last_error_kind(), ErrorKind::InvalidInput as i32);
        assert!(qos(3).is_none());
        assert_eq!(hew_mqtt_last_error_kind(), ErrorKind::InvalidInput as i32);
    }

    #[test]
    fn valid_qos_is_exact() {
        assert_eq!(qos(0), Some(QoS::AtMostOnce));
        assert_eq!(qos(1), Some(QoS::AtLeastOnce));
        assert_eq!(qos(2), Some(QoS::ExactlyOnce));
    }

    #[test]
    fn message_lifecycle_is_registry_guarded() {
        let before = hew_mqtt_message_count();
        let handle = register_message(MqttMessage {
            topic: "weather/edmonton".to_owned(),
            payload: b"21".to_vec(),
        });
        assert_eq!(hew_mqtt_message_count(), before + 1);
        let topic = hew_mqtt_message_topic(handle);
        assert!(!topic.is_null());
        // SAFETY: the accessor just returned this owner.
        assert_eq!(unsafe { owned_text(topic) }, "weather/edmonton");
        hew_mqtt_message_free(handle);
        hew_mqtt_message_free(handle);
        assert_eq!(hew_mqtt_message_count(), before);
        assert!(hew_mqtt_message_topic(handle).is_null());
        assert_eq!(hew_mqtt_last_error_kind(), ErrorKind::Closed as i32);
    }

    #[test]
    fn stale_connection_handle_fails_closed() {
        assert_eq!(hew_mqtt_next_result(i64::MAX, 0), 0);
        assert_eq!(hew_mqtt_last_error_kind(), ErrorKind::Closed as i32);
        hew_mqtt_close(i64::MAX);
        hew_mqtt_close(i64::MAX);
    }

    #[cfg(feature = "integration")]
    fn connect() -> i64 {
        static NEXT_ID: AtomicI64 = AtomicI64::new(1);
        let host = managed("127.0.0.1");
        let id = managed(&format!(
            "hew-mqtt-test-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        // SAFETY: `host` and `client_id` are live managed strings for the call.
        let handle = unsafe { hew_mqtt_connect(host, 11883, id, 30) };
        // SAFETY: this test owns both handles.
        unsafe {
            string_release(host);
            string_release(id);
        }
        assert_ne!(handle, 0, "MQTT broker is unavailable: {}", unsafe {
            // SAFETY: `hew_mqtt_last_error` just returned this owner.
            owned_text(hew_mqtt_last_error())
        });
        handle
    }

    #[test]
    #[cfg(feature = "integration")]
    fn publish_subscribe_roundtrip_and_cleanup() {
        let before_connections = hew_mqtt_connection_count();
        let before_messages = hew_mqtt_message_count();
        let handle = connect();
        // A payload over 16 bytes with non-ASCII characters exercises the
        // exact managed-string round trip rather than a short ASCII stand-in.
        let topic = format!("hew/tests/{handle}");
        let payload = "hello mqtt — 雪の結晶";
        let topic_arg = managed(&topic);
        let payload_arg = managed(payload);
        // SAFETY: `topic_arg` is a live managed string for the call.
        assert_eq!(unsafe { hew_mqtt_subscribe(handle, topic_arg, 1) }, 0);
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: `topic_arg` and `payload_arg` are live managed strings.
        let status = unsafe { hew_mqtt_publish(handle, topic_arg, payload_arg, 1, 0) };
        assert_eq!(status, 0);
        // SAFETY: this test owns both handles.
        unsafe {
            string_release(topic_arg);
            string_release(payload_arg);
        }
        let message = hew_mqtt_next_result(handle, 3000);
        assert_ne!(message, 0);
        // SAFETY: the accessors just returned these owners.
        unsafe {
            assert_eq!(owned_text(hew_mqtt_message_topic(message)), topic);
            assert_eq!(owned_text(hew_mqtt_message_payload(message)), payload);
        }
        hew_mqtt_message_free(message);
        hew_mqtt_close(handle);
        assert_eq!(hew_mqtt_connection_count(), before_connections);
        assert_eq!(hew_mqtt_message_count(), before_messages);
    }
}
