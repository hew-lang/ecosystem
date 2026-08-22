use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{Ipv6Addr, Shutdown, TcpListener, TcpStream};
use std::os::raw::c_char;
use std::ptr;
use std::slice;
use std::thread;
use std::time::{Duration, Instant};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const ACCEPT_TIMEOUT: Duration = Duration::from_millis(250);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(5);

unsafe fn utf8_with_len<'a>(value: *const c_char, len: i64) -> Option<&'a str> {
    let len = usize::try_from(len).ok()?;
    if value.is_null() {
        return (len == 0).then_some("");
    }
    // SAFETY: this function's caller contract requires `value` to point to
    // `len` initialized bytes when non-null, which is checked above.
    let bytes = unsafe { slice::from_raw_parts(value.cast::<u8>(), len) };
    std::str::from_utf8(bytes).ok()
}

fn alloc_c_string(value: &str) -> *mut c_char {
    let size = value.len() + 1;
    // SAFETY: `size` is a nonzero byte count (`value.len() + 1`); `malloc`
    // may return null, which is checked immediately below.
    let output = unsafe { libc::malloc(size) }.cast::<u8>();
    if output.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `output` was just allocated above with room for `value.len()`
    // bytes plus one trailing byte, and was checked non-null.
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr(), output, value.len());
        output.add(value.len()).write(0);
    }
    output.cast::<c_char>()
}

fn str_to_malloc(value: &str) -> *mut c_char {
    if value.as_bytes().contains(&0) {
        set_error(
            ErrorKind::Internal,
            "string result contained NUL and could not cross the C ABI",
        );
        return alloc_c_string("");
    }
    alloc_c_string(value)
}

fn no_nul(value: &str) -> bool {
    !value.as_bytes().contains(&0)
}

fn input_string<'a>(
    value: *const c_char,
    len: i64,
    kind: ErrorKind,
    invalid_utf8: &'static str,
    contained_nul: &'static str,
) -> Option<&'a str> {
    // SAFETY: `value`/`len` are the raw pointer and declared byte length
    // this function received from its caller, matching `utf8_with_len`'s
    // contract.
    let Some(value) = (unsafe { utf8_with_len(value, len) }) else {
        set_error(kind, invalid_utf8);
        return None;
    };
    if !no_nul(value) {
        set_error(kind, contained_nul);
        return None;
    }
    Some(value)
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    Listen = 1,
    Accept = 2,
    Read = 3,
    Parse = 4,
    Write = 5,
    MissingHeader = 6,
    Decode = 7,
    MissingFormField = 8,
    Internal = 9,
}

#[derive(Debug)]
struct ErrorState {
    kind: ErrorKind,
    status: i32,
    message: String,
}

thread_local! {
    static LAST_ERROR: RefCell<ErrorState> = const { RefCell::new(ErrorState {
        kind: ErrorKind::None,
        status: 0,
        message: String::new(),
    }) };
}

fn clear_error() {
    LAST_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.kind = ErrorKind::None;
        state.status = 0;
        state.message.clear();
    });
}

fn set_error(kind: ErrorKind, message: impl Into<String>) {
    LAST_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.kind = kind;
        state.status = -1;
        state.message = message.into();
    });
}

#[derive(Debug)]
struct NativeError {
    kind: ErrorKind,
    message: String,
}

impl NativeError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
struct HttpServer {
    listener: TcpListener,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: String,
    stream: Option<TcpStream>,
}

unsafe fn server_mut(handle: i64) -> Option<&'static mut HttpServer> {
    // SAFETY: this function's caller contract requires `handle` to be zero
    // or a pointer produced by `Box::into_raw` in `hew_ecosystem_http_listen`
    // that has not yet been freed.
    unsafe { (handle as *mut HttpServer).as_mut() }
}

unsafe fn request_ref(handle: i64) -> Option<&'static HttpRequest> {
    // SAFETY: this function's caller contract requires `handle` to be zero
    // or a pointer produced by `Box::into_raw` in
    // `hew_ecosystem_http_server_accept` that has not yet been freed.
    unsafe { (handle as *const HttpRequest).as_ref() }
}

unsafe fn request_mut(handle: i64) -> Option<&'static mut HttpRequest> {
    // SAFETY: same contract as `request_ref` above, with exclusive access
    // required by the caller for the duration of the borrow.
    unsafe { (handle as *mut HttpRequest).as_mut() }
}

/// Bind an HTTP listener. Returns zero and records `Listen` on failure.
///
/// # Safety
/// `addr` must point to `addr_len` bytes of valid UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_listen(addr: *const c_char, addr_len: i64) -> i64 {
    let Some(addr) = input_string(
        addr,
        addr_len,
        ErrorKind::Listen,
        "listen failed: invalid UTF-8 address",
        "listen failed: address contained NUL",
    ) else {
        return 0;
    };
    match TcpListener::bind(addr) {
        Ok(listener) => {
            if let Err(error) = listener.set_nonblocking(true) {
                set_error(
                    ErrorKind::Listen,
                    format!("listen failed: could not enable bounded accept: {error}"),
                );
                return 0;
            }
            clear_error();
            Box::into_raw(Box::new(HttpServer { listener })) as i64
        }
        Err(error) => {
            set_error(
                ErrorKind::Listen,
                format!("listen failed for {addr:?}: {error}"),
            );
            0
        }
    }
}

/// Accept and parse exactly one HTTP/1.1 request.
///
/// # Safety
/// `server` must be zero or a live handle returned by
/// [`hew_ecosystem_http_listen`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_server_accept(server: i64) -> i64 {
    // SAFETY: `server` is caller-provided per this function's `# Safety`
    // contract above.
    let Some(server) = (unsafe { server_mut(server) }) else {
        set_error(ErrorKind::Accept, "accept failed: null server handle");
        return 0;
    };
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    let stream = loop {
        match server.listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    set_error(
                        ErrorKind::Accept,
                        "accept timed out after 250 ms without a request",
                    );
                    return 0;
                }
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => {
                set_error(ErrorKind::Accept, format!("accept failed: {error}"));
                return 0;
            }
        }
    };
    if let Err(error) = stream.set_nonblocking(true) {
        set_error(
            ErrorKind::Read,
            format!("read failed: could not enable bounded request reads: {error}"),
        );
        return 0;
    }
    match read_request(stream, deadline) {
        Ok(request) => {
            clear_error();
            Box::into_raw(Box::new(request)) as i64
        }
        Err(error) => {
            set_error(error.kind, error.message);
            0
        }
    }
}

fn read_with_deadline(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
    phase: &str,
) -> Result<usize, NativeError> {
    loop {
        if Instant::now() >= deadline {
            return Err(NativeError::new(
                ErrorKind::Read,
                format!("read timed out after 250 ms while reading {phase}"),
            ));
        }
        match stream.read(buffer) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(NativeError::new(
                    ErrorKind::Read,
                    format!("read failed while reading {phase}: {error}"),
                ));
            }
        }
    }
}

/// A parsed request line and header list, ready for body reading.
struct ParsedHead {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    content_length: usize,
    expects_continue: bool,
}

fn read_request_head(
    stream: &mut TcpStream,
    deadline: Instant,
    chunk: &mut [u8; 4096],
) -> Result<(Vec<u8>, usize), NativeError> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        let count = read_with_deadline(stream, chunk, deadline, "headers")?;
        if count == 0 {
            return Err(NativeError::new(
                ErrorKind::Read,
                "read failed: connection closed before headers completed",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(offset) = find_header_end(&bytes) {
            if offset + 4 > MAX_HEADER_BYTES {
                return Err(NativeError::new(
                    ErrorKind::Parse,
                    "parse failed: request headers exceeded 64 KiB",
                ));
            }
            break offset;
        }
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(NativeError::new(
                ErrorKind::Parse,
                "parse failed: request headers exceeded 64 KiB",
            ));
        }
    };
    Ok((bytes, header_end))
}

/// Validate the `Host`, `Content-Length`, `Transfer-Encoding`, and `Expect`
/// headers of an already-parsed request and derive the values `read_request`
/// needs from them.
fn validate_request_headers(headers: &[(String, String)]) -> Result<(usize, bool), NativeError> {
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(NativeError::new(
            ErrorKind::Parse,
            "parse failed: Transfer-Encoding is not supported",
        ));
    }
    let hosts: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name == "host")
        .map(|(_, value)| value.as_str())
        .collect();
    match hosts.as_slice() {
        [host] if valid_host(host) => {}
        [_] => {
            return Err(NativeError::new(
                ErrorKind::Parse,
                "parse failed: invalid Host header",
            ));
        }
        _ => {
            return Err(NativeError::new(
                ErrorKind::Parse,
                "parse failed: HTTP/1.1 requires exactly one Host header",
            ));
        }
    }

    let content_lengths: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .map(|(_, value)| value.as_str())
        .collect();
    if content_lengths.len() > 1 {
        return Err(NativeError::new(
            ErrorKind::Parse,
            "parse failed: multiple Content-Length headers",
        ));
    }
    let content_length = match content_lengths.first() {
        None => 0,
        Some(value) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
            value.parse::<usize>().map_err(|_| {
                NativeError::new(ErrorKind::Parse, "parse failed: invalid Content-Length")
            })?
        }
        Some(_) => {
            return Err(NativeError::new(
                ErrorKind::Parse,
                "parse failed: invalid Content-Length",
            ));
        }
    };
    if content_length > MAX_BODY_BYTES {
        return Err(NativeError::new(
            ErrorKind::Parse,
            "parse failed: request body exceeded 8 MiB",
        ));
    }

    let expects_continue = match headers
        .iter()
        .filter(|(name, _)| name == "expect")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => false,
        [value] if value.eq_ignore_ascii_case("100-continue") => true,
        _ => {
            return Err(NativeError::new(
                ErrorKind::Parse,
                "parse failed: unsupported Expect header",
            ));
        }
    };

    Ok((content_length, expects_continue))
}

fn parse_request_head(head_bytes: &[u8]) -> Result<ParsedHead, NativeError> {
    let mut header_storage = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut header_storage);
    match parsed.parse(head_bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(NativeError::new(
                ErrorKind::Parse,
                "parse failed: incomplete HTTP request headers",
            ));
        }
        Err(error) => {
            return Err(NativeError::new(
                ErrorKind::Parse,
                format!("parse failed: {error}"),
            ));
        }
    }
    if parsed.version != Some(1) {
        return Err(NativeError::new(
            ErrorKind::Parse,
            "parse failed: only HTTP/1.1 requests are supported",
        ));
    }
    let method = parsed
        .method
        .ok_or_else(|| NativeError::new(ErrorKind::Parse, "parse failed: missing method"))?
        .to_owned();
    let target = parsed.path.ok_or_else(|| {
        NativeError::new(ErrorKind::Parse, "parse failed: missing request target")
    })?;
    let (path, query) = parse_request_target(&method, target)?;

    let mut headers = Vec::with_capacity(parsed.headers.len());
    for header in parsed.headers.iter() {
        let value = std::str::from_utf8(header.value).map_err(|_| {
            NativeError::new(
                ErrorKind::Parse,
                format!("parse failed: header {:?} was not UTF-8", header.name),
            )
        })?;
        if !no_nul(value) {
            return Err(NativeError::new(
                ErrorKind::Parse,
                format!("parse failed: header {:?} contained NUL", header.name),
            ));
        }
        headers.push((header.name.to_ascii_lowercase(), value.to_owned()));
    }
    let (content_length, expects_continue) = validate_request_headers(&headers)?;

    Ok(ParsedHead {
        method,
        path,
        query,
        headers,
        content_length,
        expects_continue,
    })
}

fn read_request_body(
    stream: &mut TcpStream,
    deadline: Instant,
    chunk: &mut [u8; 4096],
    head_bytes: &[u8],
    body_start: usize,
    content_length: usize,
    expects_continue: bool,
) -> Result<String, NativeError> {
    let buffered = head_bytes
        .len()
        .saturating_sub(body_start)
        .min(content_length);
    let mut body_bytes = Vec::with_capacity(content_length);
    body_bytes.extend_from_slice(&head_bytes[body_start..body_start + buffered]);
    if expects_continue && body_bytes.len() < content_length {
        write_with_deadline(
            stream,
            b"HTTP/1.1 100 Continue\r\n\r\n",
            deadline,
            "100 Continue",
        )?;
    }
    while body_bytes.len() < content_length {
        let remaining = content_length - body_bytes.len();
        let read_limit = remaining.min(chunk.len());
        let count = read_with_deadline(stream, &mut chunk[..read_limit], deadline, "body")?;
        if count == 0 {
            return Err(NativeError::new(
                ErrorKind::Read,
                format!(
                    "read failed: body ended at {} of {content_length} bytes",
                    body_bytes.len()
                ),
            ));
        }
        body_bytes.extend_from_slice(&chunk[..count]);
    }
    let body = String::from_utf8(body_bytes).map_err(|_| {
        NativeError::new(ErrorKind::Parse, "parse failed: request body was not UTF-8")
    })?;
    if !no_nul(&body) {
        return Err(NativeError::new(
            ErrorKind::Parse,
            "parse failed: request body contained NUL",
        ));
    }
    Ok(body)
}

fn read_request(mut stream: TcpStream, deadline: Instant) -> Result<HttpRequest, NativeError> {
    let mut chunk = [0_u8; 4096];
    let (bytes, header_end) = read_request_head(&mut stream, deadline, &mut chunk)?;
    let head = parse_request_head(&bytes[..header_end + 4])?;
    let body_start = header_end + 4;
    let body = read_request_body(
        &mut stream,
        deadline,
        &mut chunk,
        &bytes,
        body_start,
        head.content_length,
        head.expects_continue,
    )?;

    Ok(HttpRequest {
        method: head.method,
        path: head.path,
        query: head.query,
        headers: head.headers,
        body,
        stream: Some(stream),
    })
}

fn valid_host(value: &str) -> bool {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['/', '@', '#', '?'])
    {
        return false;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let Some((literal, suffix)) = rest.split_once(']') else {
            return false;
        };
        return valid_ip_literal(literal)
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port));
    }
    if value.matches(':').count() > 1 {
        return false;
    }
    let (name, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(name, port)| (name, Some(port)));
    valid_reg_name(name) && port.is_none_or(valid_port)
}

fn parse_request_target(method: &str, target: &str) -> Result<(String, String), NativeError> {
    let invalid = || NativeError::new(ErrorKind::Parse, "parse failed: invalid request target");
    if target == "*" {
        return if method.eq_ignore_ascii_case("OPTIONS") {
            Ok(("*".to_owned(), String::new()))
        } else {
            Err(invalid())
        };
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        return if valid_authority_form(target) {
            Ok((target.to_owned(), String::new()))
        } else {
            Err(invalid())
        };
    }
    if target.starts_with('/') {
        return parse_origin_form(target).ok_or_else(invalid);
    }
    parse_absolute_form(target).ok_or_else(invalid)
}

fn parse_origin_form(target: &str) -> Option<(String, String)> {
    if target.contains('#') {
        return None;
    }
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    if valid_uri_component(path, true, false) && valid_uri_component(query, true, true) {
        Some((path.to_owned(), query.to_owned()))
    } else {
        None
    }
}

fn parse_absolute_form(target: &str) -> Option<(String, String)> {
    let (scheme, rest) = target.split_once("://")?;
    if !valid_scheme(scheme) || target.contains('#') {
        return None;
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if !valid_host(authority) {
        return None;
    }
    let suffix = &rest[authority_end..];
    if suffix.is_empty() {
        return Some(("/".to_owned(), String::new()));
    }
    if let Some(query) = suffix.strip_prefix('?') {
        return valid_uri_component(query, true, true).then(|| ("/".to_owned(), query.to_owned()));
    }
    parse_origin_form(suffix)
}

fn valid_authority_form(target: &str) -> bool {
    if let Some(rest) = target.strip_prefix('[') {
        let Some((literal, port)) = rest.split_once("]:") else {
            return false;
        };
        return valid_ip_literal(literal) && valid_port(port);
    }
    let Some((host, port)) = target.rsplit_once(':') else {
        return false;
    };
    valid_reg_name(host) && valid_port(port)
}

fn valid_scheme(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn valid_uri_component(value: &str, allow_slash: bool, allow_question: bool) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if uri_unreserved(byte)
            || uri_sub_delim(byte)
            || matches!(byte, b':' | b'@')
            || (allow_slash && byte == b'/')
            || (allow_question && byte == b'?')
        {
            index += 1;
        } else if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            index += 3;
        } else {
            return false;
        }
    }
    true
}

fn valid_ip_literal(value: &str) -> bool {
    if value.parse::<Ipv6Addr>().is_ok() {
        return true;
    }
    let Some(versioned) = value.strip_prefix('v').or_else(|| value.strip_prefix('V')) else {
        return false;
    };
    let Some((version, address)) = versioned.split_once('.') else {
        return false;
    };
    !version.is_empty()
        && version.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !address.is_empty()
        && address
            .bytes()
            .all(|byte| uri_unreserved(byte) || uri_sub_delim(byte) || byte == b':')
}

fn valid_reg_name(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if uri_unreserved(byte) || uri_sub_delim(byte) {
            index += 1;
        } else if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            index += 3;
        } else {
            return false;
        }
    }
    true
}

fn uri_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn uri_sub_delim(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}

fn valid_port(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u16>().is_ok()
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Close and free a listener. Zero is a no-op.
///
/// # Safety
/// A nonzero `server` must be a live handle returned by
/// [`hew_ecosystem_http_listen`] and must be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_server_close(server: i64) {
    if server != 0 {
        // SAFETY: this function's caller contract requires `server` to be a
        // live handle from `hew_ecosystem_http_listen`, freed exactly once.
        drop(unsafe { Box::from_raw(server as *mut HttpServer) });
    }
}

/// Free a request and close its client connection. Zero is a no-op.
///
/// # Safety
/// A nonzero `request` must be a live handle returned by
/// [`hew_ecosystem_http_server_accept`] and must be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_free(request: i64) {
    if request != 0 {
        // SAFETY: this function's caller contract requires `request` to be
        // a live handle from `hew_ecosystem_http_server_accept`, freed
        // exactly once.
        drop(unsafe { Box::from_raw(request as *mut HttpRequest) });
    }
}

unsafe fn request_string(request: i64, field: fn(&HttpRequest) -> &str) -> *mut c_char {
    // SAFETY: this function's caller contract requires `request` to be zero
    // or a live handle from `hew_ecosystem_http_server_accept`.
    let Some(request) = (unsafe { request_ref(request) }) else {
        set_error(
            ErrorKind::Internal,
            "request accessor failed: null request handle",
        );
        return str_to_malloc("");
    };
    clear_error();
    str_to_malloc(field(request))
}

/// Return the request method. Returns an empty string and records
/// `Internal` when `request` is null.
///
/// # Safety
/// `request` must be zero or a live handle returned by
/// [`hew_ecosystem_http_server_accept`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_method(request: i64) -> *mut c_char {
    // SAFETY: forwards this function's own `request` contract to
    // `request_string`.
    unsafe { request_string(request, |request| &request.method) }
}

/// Return the request path without its query string. Returns an empty
/// string and records `Internal` when `request` is null.
///
/// # Safety
/// `request` must be zero or a live handle returned by
/// [`hew_ecosystem_http_server_accept`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_path(request: i64) -> *mut c_char {
    // SAFETY: forwards this function's own `request` contract to
    // `request_string`.
    unsafe { request_string(request, |request| &request.path) }
}

/// Return the request's query string without its leading `?`. Returns an
/// empty string and records `Internal` when `request` is null.
///
/// # Safety
/// `request` must be zero or a live handle returned by
/// [`hew_ecosystem_http_server_accept`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_query(request: i64) -> *mut c_char {
    // SAFETY: forwards this function's own `request` contract to
    // `request_string`.
    unsafe { request_string(request, |request| &request.query) }
}

/// Return the request body as UTF-8 text. Returns an empty string and
/// records `Internal` when `request` is null.
///
/// # Safety
/// `request` must be zero or a live handle returned by
/// [`hew_ecosystem_http_server_accept`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_body(request: i64) -> *mut c_char {
    // SAFETY: forwards this function's own `request` contract to
    // `request_string`.
    unsafe { request_string(request, |request| &request.body) }
}

/// Return a header value, setting `MissingHeader` when absent.
///
/// # Safety
/// `request` must be live and `name` must point to `name_len` UTF-8 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_header(
    request: i64,
    name: *const c_char,
    name_len: i64,
) -> *mut c_char {
    // SAFETY: this function's caller contract requires `request` to be a
    // live handle from `hew_ecosystem_http_server_accept`.
    let Some(request) = (unsafe { request_ref(request) }) else {
        set_error(ErrorKind::Internal, "header failed: null request handle");
        return str_to_malloc("");
    };
    let Some(name) = input_string(
        name,
        name_len,
        ErrorKind::Internal,
        "header failed: invalid UTF-8 name",
        "header failed: name contained NUL",
    ) else {
        return str_to_malloc("");
    };
    let lower = name.to_ascii_lowercase();
    if let Some((_, value)) = request.headers.iter().find(|(key, _)| key == &lower) {
        clear_error();
        str_to_malloc(value)
    } else {
        set_error(ErrorKind::MissingHeader, name);
        str_to_malloc("")
    }
}

fn reason_phrase(status: i64) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        205 => "Reset Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn response_parts<'a>(
    request: &HttpRequest,
    status: i64,
    body: &'a str,
) -> (Option<usize>, &'a str) {
    match status {
        204 => (None, ""),
        205 => (Some(0), ""),
        304 => (Some(body.len()), ""),
        _ if request.method.eq_ignore_ascii_case("HEAD") => (Some(body.len()), ""),
        _ => (Some(body.len()), body),
    }
}

fn write_with_deadline(
    stream: &mut TcpStream,
    response: &[u8],
    deadline: Instant,
    phase: &str,
) -> Result<i64, NativeError> {
    let mut offset = 0;
    while offset < response.len() {
        if Instant::now() >= deadline {
            let message = if phase == "response" {
                "write timed out after 250 ms".to_owned()
            } else {
                format!("write timed out after 250 ms while sending {phase}")
            };
            return Err(NativeError::new(ErrorKind::Write, message));
        }
        match stream.write(&response[offset..]) {
            Ok(0) => {
                let message = if phase == "response" {
                    "write failed: connection closed before response completed".to_owned()
                } else {
                    format!("write failed: connection closed while sending {phase}")
                };
                return Err(NativeError::new(ErrorKind::Write, message));
            }
            Ok(count) => offset += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                let message = if phase == "response" {
                    format!("write failed: {error}")
                } else {
                    format!("write failed while sending {phase}: {error}")
                };
                return Err(NativeError::new(ErrorKind::Write, message));
            }
        }
    }
    i64::try_from(response.len()).map_err(|_| {
        NativeError::new(
            ErrorKind::Write,
            "write failed: response exceeded i64 byte range",
        )
    })
}

fn write_response(request: &mut HttpRequest, response: &[u8]) -> Result<i64, NativeError> {
    let Some(mut stream) = request.stream.take() else {
        return Err(NativeError::new(
            ErrorKind::Write,
            "write failed: response already sent",
        ));
    };
    let written = write_with_deadline(
        &mut stream,
        response,
        Instant::now() + ACCEPT_TIMEOUT,
        "response",
    )?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(written)
}

/// Send one complete HTTP response.
///
/// # Safety
/// `request` must be live; string pointers must reference their declared byte
/// lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_respond(
    request: i64,
    status: i64,
    body: *const c_char,
    body_len: i64,
    content_type: *const c_char,
    content_type_len: i64,
) -> i64 {
    // SAFETY: this function's caller contract requires `request` to be
    // a live handle from `hew_ecosystem_http_server_accept`.
    let Some(request) = (unsafe { request_mut(request) }) else {
        set_error(ErrorKind::Write, "write failed: null request handle");
        return -1;
    };
    // SAFETY: `body`/`body_len` are the raw pointer and declared byte
    // length this function received from its caller, matching
    // `utf8_with_len`'s contract.
    let Some(body) = (unsafe { utf8_with_len(body, body_len) }) else {
        set_error(ErrorKind::Write, "write failed: invalid UTF-8 body");
        return -1;
    };
    let Some(content_type) = input_string(
        content_type,
        content_type_len,
        ErrorKind::Write,
        "write failed: invalid UTF-8 Content-Type",
        "write failed: Content-Type contained NUL",
    ) else {
        return -1;
    };
    if !(200..=599).contains(&status) {
        set_error(
            ErrorKind::Write,
            format!("write failed: invalid status {status}"),
        );
        return -1;
    }
    if content_type.contains(['\r', '\n']) {
        set_error(
            ErrorKind::Write,
            "write failed: Content-Type contained CR or LF",
        );
        return -1;
    }
    if request.method.eq_ignore_ascii_case("CONNECT") && (200..=299).contains(&status) {
        set_error(
            ErrorKind::Write,
            "write failed: successful CONNECT tunnelling is not supported",
        );
        return -1;
    }
    let (content_length, transmitted_body) = response_parts(request, status, body);
    let length_header = content_length
        .map(|length| format!("Content-Length: {length}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n{length_header}Connection: close\r\n\r\n{transmitted_body}",
        reason_phrase(status)
    );
    match write_response(request, response.as_bytes()) {
        Ok(written) => {
            clear_error();
            written
        }
        Err(error) => {
            set_error(error.kind, error.message);
            -1
        }
    }
}

/// Send a 302 redirect with an empty body.
///
/// # Safety
/// `request` must be live and `location` must point to `location_len` UTF-8
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_request_redirect(
    request: i64,
    location: *const c_char,
    location_len: i64,
) -> i64 {
    // SAFETY: this function's caller contract requires `request` to be
    // a live handle from `hew_ecosystem_http_server_accept`.
    let Some(request) = (unsafe { request_mut(request) }) else {
        set_error(ErrorKind::Write, "write failed: null request handle");
        return -1;
    };
    let Some(location) = input_string(
        location,
        location_len,
        ErrorKind::Write,
        "write failed: invalid UTF-8 Location",
        "write failed: Location contained NUL",
    ) else {
        return -1;
    };
    if location.contains(['\r', '\n']) {
        set_error(
            ErrorKind::Write,
            "write failed: Location contained CR or LF",
        );
        return -1;
    }
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    match write_response(request, response.as_bytes()) {
        Ok(written) => {
            clear_error();
            written
        }
        Err(error) => {
            set_error(error.kind, error.message);
            -1
        }
    }
}

fn decode(input: &str) -> Result<String, NativeError> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(NativeError::new(
                        ErrorKind::Decode,
                        format!("decode failed: incomplete percent escape at byte {index}"),
                    ));
                }
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| {
                    NativeError::new(
                        ErrorKind::Decode,
                        format!("decode failed: invalid percent escape at byte {index}"),
                    )
                })?;
                let value = u8::from_str_radix(hex, 16).map_err(|_| {
                    NativeError::new(
                        ErrorKind::Decode,
                        format!("decode failed: invalid percent escape at byte {index}"),
                    )
                })?;
                decoded.push(value);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| NativeError::new(ErrorKind::Decode, "decode failed: result was not UTF-8"))
}

/// Strictly decode URL-encoded UTF-8.
///
/// # Safety
/// `input` must point to `input_len` UTF-8 bytes. The returned string is
/// caller-freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_url_decode(
    input: *const c_char,
    input_len: i64,
) -> *mut c_char {
    let Some(input) = input_string(
        input,
        input_len,
        ErrorKind::Decode,
        "decode failed: input was not UTF-8",
        "decode failed: input contained NUL",
    ) else {
        return str_to_malloc("");
    };
    match decode(input) {
        Ok(value) if no_nul(&value) => {
            clear_error();
            str_to_malloc(&value)
        }
        Ok(_) => {
            set_error(ErrorKind::Decode, "decode failed: result contained NUL");
            str_to_malloc("")
        }
        Err(error) => {
            set_error(error.kind, error.message);
            str_to_malloc("")
        }
    }
}

/// Extract a strictly decoded URL-encoded form field.
///
/// # Safety
/// `body` and `key` must point to their declared UTF-8 byte lengths. The
/// returned string is caller-freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_form_value(
    body: *const c_char,
    body_len: i64,
    key: *const c_char,
    key_len: i64,
) -> *mut c_char {
    let Some(body) = input_string(
        body,
        body_len,
        ErrorKind::Decode,
        "decode failed: form body was not UTF-8",
        "decode failed: form body contained NUL",
    ) else {
        return str_to_malloc("");
    };
    let Some(key) = input_string(
        key,
        key_len,
        ErrorKind::Decode,
        "decode failed: form key was not UTF-8",
        "decode failed: form key contained NUL",
    ) else {
        return str_to_malloc("");
    };
    for pair in body.split('&') {
        let (encoded_key, encoded_value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded_key = match decode(encoded_key) {
            Ok(value) => value,
            Err(error) => {
                set_error(error.kind, error.message);
                return str_to_malloc("");
            }
        };
        if decoded_key == key {
            return match decode(encoded_value) {
                Ok(value) if no_nul(&value) => {
                    clear_error();
                    str_to_malloc(&value)
                }
                Ok(_) => {
                    set_error(ErrorKind::Decode, "decode failed: form value contained NUL");
                    str_to_malloc("")
                }
                Err(error) => {
                    set_error(error.kind, error.message);
                    str_to_malloc("")
                }
            };
        }
    }
    set_error(ErrorKind::MissingFormField, key);
    str_to_malloc("")
}

#[derive(Debug)]
struct TestClient {
    worker: thread::JoinHandle<Result<Vec<u8>, String>>,
}

#[derive(Debug)]
struct TestFixture {
    addr: String,
}

fn run_test_client(
    addr: &str,
    request_head: &[u8],
    request_body: &[u8],
    wait_for_continue: bool,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(stream) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::ConnectionRefused
                    && Instant::now() < deadline =>
            {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => return Err(format!("test client connect failed: {error}")),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("test client read timeout setup failed: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("test client write timeout setup failed: {error}"))?;
    stream
        .write_all(request_head)
        .map_err(|error| format!("test client header write failed: {error}"))?;

    let mut response = Vec::new();
    if wait_for_continue {
        let mut byte = [0_u8; 1];
        while find_header_end(&response).is_none() {
            let count = stream
                .read(&mut byte)
                .map_err(|error| format!("test client interim read failed: {error}"))?;
            if count == 0 {
                return Ok(response);
            }
            response.push(byte[0]);
        }
        if response.starts_with(b"HTTP/1.1 100 Continue\r\n") {
            stream
                .write_all(request_body)
                .map_err(|error| format!("test client body write failed: {error}"))?;
        }
    } else {
        stream
            .write_all(request_body)
            .map_err(|error| format!("test client body write failed: {error}"))?;
    }
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| format!("test client write shutdown failed: {error}"))?;
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("test client response read failed: {error}"))?;
    Ok(response)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_ecosystem_http_test_fixture_new() -> i64 {
    match TcpListener::bind("127.0.0.1:0").and_then(|listener| listener.local_addr()) {
        Ok(addr) => {
            clear_error();
            Box::into_raw(Box::new(TestFixture {
                addr: addr.to_string(),
            })) as i64
        }
        Err(error) => {
            set_error(
                ErrorKind::Internal,
                format!("test address allocation failed: {error}"),
            );
            0
        }
    }
}

/// Return the loopback address of a test fixture's listener.
///
/// # Safety
/// `fixture` must be zero or a live handle returned by
/// [`hew_ecosystem_http_test_fixture_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_test_fixture_addr(fixture: i64) -> *mut c_char {
    // SAFETY: this function's caller contract requires `fixture` to be zero
    // or a live handle from `hew_ecosystem_http_test_fixture_new`.
    let Some(fixture) = (unsafe { (fixture as *const TestFixture).as_ref() }) else {
        set_error(ErrorKind::Internal, "test fixture handle was null");
        return str_to_malloc("");
    };
    clear_error();
    str_to_malloc(&fixture.addr)
}

/// Free a test fixture. Zero is a no-op.
///
/// # Safety
/// A nonzero `fixture` must be a live handle returned by
/// [`hew_ecosystem_http_test_fixture_new`] and must be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_test_fixture_free(fixture: i64) {
    if fixture != 0 {
        // SAFETY: this function's caller contract requires `fixture` to be
        // a live handle from `hew_ecosystem_http_test_fixture_new`, freed
        // exactly once.
        drop(unsafe { Box::from_raw(fixture as *mut TestFixture) });
    }
}

/// Start a background client that plays one fixed test-request scenario
/// against `fixture`'s listener.
///
/// # Safety
/// `fixture` must be zero or a live handle returned by
/// [`hew_ecosystem_http_test_fixture_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_test_client_start_case(
    fixture: i64,
    case_id: i32,
) -> i64 {
    // SAFETY: this function's caller contract requires `fixture` to be zero
    // or a live handle from `hew_ecosystem_http_test_fixture_new`.
    let Some(fixture) = (unsafe { (fixture as *const TestFixture).as_ref() }) else {
        set_error(ErrorKind::Internal, "test fixture handle was null");
        return 0;
    };
    let (request_head, request_body, wait_for_continue) = match case_id {
        0 => (
            b"GET /test HTTP/1.1\r\nHost: test\r\n\r\n".to_vec(),
            Vec::new(),
            false,
        ),
        1 => (
            b"GET ! HTTP/1.1\r\nHost: test\r\n\r\n".to_vec(),
            Vec::new(),
            false,
        ),
        2 => (
            b"GET http://example.test/absolute?q=hew HTTP/1.1\r\nHost: proxy.test\r\n\r\n"
                .to_vec(),
            Vec::new(),
            false,
        ),
        3 => (
            b"POST /upload HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n"
                .to_vec(),
            b"body".to_vec(),
            true,
        ),
        4 => (
            b"POST /upload HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\nExpect: unsupported\r\n\r\n"
                .to_vec(),
            b"body".to_vec(),
            true,
        ),
        5 => (
            b"POST /nul-body HTTP/1.1\r\nHost: test\r\nContent-Length: 3\r\n\r\n".to_vec(),
            b"a\0b".to_vec(),
            false,
        ),
        6 => (
            b"GET /nul-header HTTP/1.1\r\nHost: test\r\nX-Test: a\0b\r\n\r\n".to_vec(),
            Vec::new(),
            false,
        ),
        _ => {
            set_error(
                ErrorKind::Internal,
                format!("unknown test client case {case_id}"),
            );
            return 0;
        }
    };
    let worker = thread::spawn({
        let addr = fixture.addr.clone();
        move || run_test_client(&addr, &request_head, &request_body, wait_for_continue)
    });
    clear_error();
    Box::into_raw(Box::new(TestClient { worker })) as i64
}

/// Join a background test client and compare its captured response bytes
/// against the fixed expected bytes for `case_id`. Frees `client`.
///
/// # Safety
/// A nonzero `client` must be a live handle returned by
/// [`hew_ecosystem_http_test_client_start_case`] and must be freed exactly
/// once (this call frees it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_ecosystem_http_test_client_expect_case(
    client: i64,
    case_id: i32,
) -> i32 {
    if client == 0 {
        set_error(ErrorKind::Internal, "test client handle was null");
        return 0;
    }
    let expected: &[u8] = match case_id {
        0 => b"",
        1 => b"HTTP/1.1 204 No Content\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n",
        2 => b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone",
        3 => b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 5\r\nConnection: close\r\n\r\ncaf\xc3\xa9",
        _ => {
            set_error(
                ErrorKind::Internal,
                format!("unknown test response case {case_id}"),
            );
            return 0;
        }
    };
    // SAFETY: this function's caller contract requires `client` to be a
    // live handle from `hew_ecosystem_http_test_client_start_case`, freed
    // exactly once (checked non-zero above).
    let client = unsafe { Box::from_raw(client as *mut TestClient) };
    match client.worker.join() {
        Ok(Ok(actual)) if actual == expected => {
            clear_error();
            1
        }
        Ok(Ok(actual)) => {
            set_error(
                ErrorKind::Internal,
                format!("test client response mismatch: expected {expected:?}, got {actual:?}"),
            );
            0
        }
        Ok(Err(error)) => {
            set_error(ErrorKind::Internal, error);
            0
        }
        Err(_) => {
            set_error(ErrorKind::Internal, "test client thread panicked");
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_ecosystem_http_last_error() -> *mut c_char {
    let message = LAST_ERROR.with(|state| state.borrow().message.clone());
    str_to_malloc(&message)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_ecosystem_http_last_error_kind() -> i32 {
    LAST_ERROR.with(|state| state.borrow().kind as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_ecosystem_http_last_status() -> i32 {
    LAST_ERROR.with(|state| state.borrow().status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::sync::mpsc;

    unsafe fn raw_listen(value: &str) -> i64 {
        // SAFETY: `value` is a `&str`; its pointer and byte length match.
        unsafe { hew_ecosystem_http_listen(value.as_ptr().cast(), value.len().try_into().unwrap()) }
    }

    unsafe fn raw_header(request: i64, name: &str) -> *mut c_char {
        // SAFETY: `request` is a live handle and `name` is a `&str`; the pointer/length pair matches.
        unsafe {
            hew_ecosystem_http_request_header(
                request,
                name.as_ptr().cast(),
                name.len().try_into().unwrap(),
            )
        }
    }

    unsafe fn raw_respond(request: i64, status: i64, body: &str, content_type: &str) -> i64 {
        // SAFETY: `request` is a live handle; `body`/`content_type` pointers and lengths match their source slice/str.
        unsafe { raw_respond_bytes(request, status, body.as_bytes(), content_type) }
    }

    unsafe fn raw_respond_bytes(request: i64, status: i64, body: &[u8], content_type: &str) -> i64 {
        // SAFETY: `request` is a live handle; `body`/`content_type` pointers and lengths match their source slice/str.
        unsafe {
            hew_ecosystem_http_request_respond(
                request,
                status,
                body.as_ptr().cast(),
                body.len().try_into().unwrap(),
                content_type.as_ptr().cast(),
                content_type.len().try_into().unwrap(),
            )
        }
    }

    unsafe fn raw_redirect(request: i64, location: &str) -> i64 {
        // SAFETY: `request` is a live handle and `location` is a `&str`; its pointer/length match.
        unsafe {
            hew_ecosystem_http_request_redirect(
                request,
                location.as_ptr().cast(),
                location.len().try_into().unwrap(),
            )
        }
    }

    unsafe fn raw_url_decode(input: &str) -> *mut c_char {
        // SAFETY: `input` is a `&str`; its pointer and byte length match.
        unsafe {
            hew_ecosystem_http_url_decode(input.as_ptr().cast(), input.len().try_into().unwrap())
        }
    }

    unsafe fn raw_form_value(body: &str, key: &str) -> *mut c_char {
        // SAFETY: `body`/`key` are `&str`s; their pointers and byte lengths match.
        unsafe {
            hew_ecosystem_http_form_value(
                body.as_ptr().cast(),
                body.len().try_into().unwrap(),
                key.as_ptr().cast(),
                key.len().try_into().unwrap(),
            )
        }
    }

    unsafe fn owned_string(raw: *mut c_char) -> String {
        // SAFETY: `raw` is a non-null C string allocated by this crate's malloc-based allocator, per `owned_string`'s caller contract.
        let value = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        // SAFETY: `raw` was allocated by this crate's malloc-based allocator and is freed exactly once here.
        unsafe { libc::free(raw.cast()) };
        value
    }

    fn error_snapshot() -> (i32, i32, String) {
        // SAFETY: `hew_ecosystem_http_last_error` returns a freshly allocated C string, freed immediately by `owned_string`.
        unsafe {
            (
                hew_ecosystem_http_last_status(),
                hew_ecosystem_http_last_error_kind(),
                owned_string(hew_ecosystem_http_last_error()),
            )
        }
    }

    fn listener() -> (i64, u16) {
        // SAFETY: the address argument is a `&str` literal; its pointer and byte length match.
        let handle = unsafe { raw_listen("127.0.0.1:0") };
        assert_ne!(handle, 0, "{:?}", error_snapshot());
        // SAFETY: `handle` is the live handle just returned by `raw_listen` above.
        let port = unsafe { server_mut(handle) }
            .unwrap()
            .listener
            .local_addr()
            .unwrap()
            .port();
        (handle, port)
    }

    fn accept_bytes(bytes: Vec<u8>) -> (i64, i64) {
        let (server, port) = listener();
        std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client.write_all(&bytes).unwrap();
            let _ = client.shutdown(Shutdown::Write);
        });
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        let request = unsafe { hew_ecosystem_http_server_accept(server) };
        (server, request)
    }

    fn request_with_client() -> (i64, i64, mpsc::Receiver<String>) {
        request_with_method("GET")
    }

    fn request_with_method(method: &str) -> (i64, i64, mpsc::Receiver<String>) {
        let (server, port) = listener();
        let (sender, receiver) = mpsc::channel();
        let request_line = if method == "CONNECT" {
            "CONNECT example.test:443 HTTP/1.1".to_owned()
        } else {
            format!("{method} /go?q=1 HTTP/1.1")
        };
        std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                client,
                "{request_line}\r\nHost: example.test\r\nX-Test: exact\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            sender.send(response).unwrap();
        });
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        let request = unsafe { hew_ecosystem_http_server_accept(server) };
        assert_ne!(request, 0);
        (server, request, receiver)
    }

    fn assert_exact_response(method: &str, status: i64, body: &str, expected: &str) {
        let (server, request, receiver) = request_with_method(method);
        // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
        let written = unsafe { raw_respond(request, status, body, "text/plain") };
        assert_eq!(written, i64::try_from(expected.len()).unwrap());
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            expected
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn invalid_address_returns_listen_error() {
        // SAFETY: the address argument is a `&str` literal; its pointer and byte length match.
        let handle = unsafe { raw_listen("not-an-address") };
        assert_eq!(handle, 0);
        let (status, kind, message) = error_snapshot();
        assert_eq!(status, -1);
        assert_eq!(kind, ErrorKind::Listen as i32);
        assert!(message.starts_with("listen failed for \"not-an-address\":"));
    }

    #[test]
    fn available_address_returns_listener_and_clear_status() {
        let (server, _) = listener();
        assert_eq!(error_snapshot(), (0, 0, String::new()));
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
        assert_eq!(error_snapshot(), (0, 0, String::new()));
    }

    #[test]
    fn null_server_returns_accept_error() {
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        assert_eq!(unsafe { hew_ecosystem_http_server_accept(0) }, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Accept as i32,
                "accept failed: null server handle".to_owned()
            )
        );
    }

    #[test]
    fn idle_listener_returns_bounded_accept_timeout() {
        let (server, _) = listener();
        let started = Instant::now();
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        assert_eq!(unsafe { hew_ecosystem_http_server_accept(server) }, 0);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Accept as i32,
                "accept timed out after 250 ms without a request".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn fragmented_request_within_deadline_returns_request() {
        let (server, port) = listener();
        std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client.write_all(b"GET /fragmented HTTP/1.1\r\n").unwrap();
            std::thread::sleep(Duration::from_millis(25));
            client.write_all(b"Host: example.test\r\n\r\n").unwrap();
            let _ = client.shutdown(Shutdown::Write);
        });
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        let request = unsafe { hew_ecosystem_http_server_accept(server) };
        assert_ne!(request, 0, "{:?}", error_snapshot());
        assert_eq!(
            // SAFETY: `request` is the live handle accepted earlier in this test.
            unsafe { owned_string(hew_ecosystem_http_request_path(request)) },
            "/fragmented"
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn incomplete_headers_return_bounded_read_timeout() {
        let (server, port) = listener();
        std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client.write_all(b"GET /slow HTTP/1.1\r\n").unwrap();
            std::thread::sleep(Duration::from_secs(1));
        });
        let started = Instant::now();
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        assert_eq!(unsafe { hew_ecosystem_http_server_accept(server) }, 0);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Read as i32,
                "read timed out after 250 ms while reading headers".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn valid_request_returns_exact_accessors_and_missing_header_status() {
        let (server, request) = accept_bytes(
            b"POST /submit?q=hew HTTP/1.1\r\nHost: example.test\r\nX-Test: exact\r\nContent-Length: 4\r\n\r\nbody".to_vec(),
        );
        assert_ne!(request, 0);
        // SAFETY: `request` is the live handle accepted earlier in this test.
        unsafe {
            assert_eq!(
                owned_string(hew_ecosystem_http_request_method(request)),
                "POST"
            );
            assert_eq!(
                owned_string(hew_ecosystem_http_request_path(request)),
                "/submit"
            );
            assert_eq!(
                owned_string(hew_ecosystem_http_request_query(request)),
                "q=hew"
            );
            assert_eq!(
                owned_string(hew_ecosystem_http_request_body(request)),
                "body"
            );
            assert_eq!(owned_string(raw_header(request, "x-test")), "exact");
            assert_eq!(error_snapshot(), (0, 0, String::new()));
            assert_eq!(owned_string(raw_header(request, "missing")), "");
            assert_eq!(error_snapshot(), (-1, 6, "missing".to_owned()));
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn invalid_content_length_returns_parse_error() {
        let (server, request) =
            accept_bytes(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: nope\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (-1, 4, "parse failed: invalid Content-Length".to_owned())
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn non_form_request_target_returns_parse_error() {
        let (server, request) = accept_bytes(b"GET ! HTTP/1.1\r\nHost: test\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: invalid request target".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn absolute_form_returns_origin_path_and_query() {
        let (server, request) = accept_bytes(
            b"GET http://example.test/absolute?q=hew HTTP/1.1\r\nHost: proxy.test\r\n\r\n".to_vec(),
        );
        assert_ne!(request, 0, "{:?}", error_snapshot());
        // SAFETY: `request` is the live handle accepted earlier in this test.
        unsafe {
            assert_eq!(
                owned_string(hew_ecosystem_http_request_path(request)),
                "/absolute"
            );
            assert_eq!(
                owned_string(hew_ecosystem_http_request_query(request)),
                "q=hew"
            );
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn connect_requires_authority_form() {
        let (server, request) =
            accept_bytes(b"CONNECT /tunnel HTTP/1.1\r\nHost: example.test\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: invalid request target".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn asterisk_form_is_only_valid_for_options() {
        assert_eq!(
            parse_request_target("OPTIONS", "*").unwrap(),
            ("*".to_owned(), String::new())
        );
        assert!(parse_request_target("GET", "*").is_err());
    }

    #[test]
    fn missing_host_returns_parse_error() {
        let (server, request) = accept_bytes(b"GET / HTTP/1.1\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: HTTP/1.1 requires exactly one Host header".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn duplicate_host_returns_parse_error() {
        let (server, request) = accept_bytes(
            b"GET / HTTP/1.1\r\nHost: first.test\r\nHost: second.test\r\n\r\n".to_vec(),
        );
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: HTTP/1.1 requires exactly one Host header".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn malformed_host_returns_parse_error() {
        let (server, request) = accept_bytes(b"GET / HTTP/1.1\r\nHost: bad host\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: invalid Host header".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn invalid_bracketed_host_returns_parse_error() {
        let (server, request) =
            accept_bytes(b"GET / HTTP/1.1\r\nHost: [not-an-ip-literal]\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: invalid Host header".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn ipv6_host_with_port_returns_request() {
        let (server, request) =
            accept_bytes(b"GET / HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n".to_vec());
        assert_ne!(request, 0, "{:?}", error_snapshot());
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn ipvfuture_host_returns_request() {
        let (server, request) =
            accept_bytes(b"GET / HTTP/1.1\r\nHost: [v1.example]\r\n\r\n".to_vec());
        assert_ne!(request, 0, "{:?}", error_snapshot());
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn registered_name_with_subdelimiter_returns_request() {
        let (server, request) =
            accept_bytes(b"GET / HTTP/1.1\r\nHost: service!name.example\r\n\r\n".to_vec());
        assert_ne!(request, 0, "{:?}", error_snapshot());
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn oversized_body_declaration_returns_parse_error() {
        let size = MAX_BODY_BYTES + 1;
        let wire = format!("POST / HTTP/1.1\r\nHost: test\r\nContent-Length: {size}\r\n\r\n");
        let (server, request) = accept_bytes(wire.into_bytes());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                4,
                "parse failed: request body exceeded 8 MiB".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn oversized_headers_return_parse_error() {
        let wire = format!(
            "GET / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        let (server, request) = accept_bytes(wire.into_bytes());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                4,
                "parse failed: request headers exceeded 64 KiB".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn truncated_body_returns_read_error() {
        let (server, request) =
            accept_bytes(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\nabc".to_vec());
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (-1, 3, "read failed: body ended at 3 of 5 bytes".to_owned())
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn request_body_with_nul_returns_parse_error() {
        let (server, request) = accept_bytes(
            b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 3\r\n\r\na\0b".to_vec(),
        );
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: request body contained NUL".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn request_header_with_nul_returns_parse_error() {
        let (server, request) =
            accept_bytes(b"GET / HTTP/1.1\r\nHost: test\r\nX-Test: a\0b\r\n\r\n".to_vec());
        assert_eq!(request, 0);
        let (status, kind, message) = error_snapshot();
        assert_eq!(status, -1);
        assert_eq!(kind, ErrorKind::Parse as i32);
        assert!(message.starts_with("parse failed:"));
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn expect_continue_is_sent_before_reading_body() {
        let (server, port) = listener();
        let (continue_sender, continue_receiver) = mpsc::channel();
        let (response_sender, response_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client
                .write_all(
                    b"POST /upload HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n",
                )
                .unwrap();
            let mut interim = [0_u8; 25];
            client.read_exact(&mut interim).unwrap();
            continue_sender.send(interim).unwrap();
            client.write_all(b"body").unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response_sender.send(response).unwrap();
        });
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        let request = unsafe { hew_ecosystem_http_server_accept(server) };
        assert_ne!(request, 0, "{:?}", error_snapshot());
        assert_eq!(
            continue_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            *b"HTTP/1.1 100 Continue\r\n\r\n"
        );
        // SAFETY: `request` is the live handle accepted earlier in this test.
        unsafe {
            assert_eq!(
                owned_string(hew_ecosystem_http_request_body(request)),
                "body"
            );
            assert!(raw_respond(request, 204, "", "text/plain") > 0);
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(
            response_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            "HTTP/1.1 204 No Content\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn unsupported_expectation_returns_parse_error_without_waiting_for_body() {
        let (server, request) = accept_bytes(
            b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\nExpect: something-else\r\n\r\n"
                .to_vec(),
        );
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Parse as i32,
                "parse failed: unsupported Expect header".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn unsupported_transfer_encoding_returns_parse_error() {
        let (server, request) = accept_bytes(
            b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                .to_vec(),
        );
        assert_eq!(request, 0);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                4,
                "parse failed: Transfer-Encoding is not supported".to_owned()
            )
        );
        // SAFETY: `server` is the live handle from `listener()`/`accept_bytes()` earlier in this test, freed at most once here.
        unsafe { hew_ecosystem_http_server_close(server) };
    }

    #[test]
    fn text_response_writes_reason_headers_and_exact_body() {
        let (server, request, receiver) = request_with_client();
        // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
        let written = unsafe { raw_respond(request, 200, "hello", "text/plain") };
        let expected = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
        assert_eq!(written, i64::try_from(expected.len()).unwrap());
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            expected
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn response_body_with_nul_is_transmitted_without_truncation() {
        let (server, request, receiver) = request_with_client();
        // SAFETY: `request` is a live handle; `body`/`content_type` pointers and lengths match their source slice/str.
        let written = unsafe { raw_respond_bytes(request, 200, b"left\0right", "text/plain") };
        let expected = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nleft\0right";
        assert_eq!(written, i64::try_from(expected.len()).unwrap());
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            expected
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn nonreading_client_returns_bounded_write_timeout() {
        let (server, port) = listener();
        std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            client
                .write_all(b"GET /slow-response HTTP/1.1\r\nHost: example.test\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_secs(1));
        });
        // SAFETY: `server` is the live handle returned by `listener()` earlier in this test.
        let request = unsafe { hew_ecosystem_http_server_accept(server) };
        assert_ne!(request, 0, "{:?}", error_snapshot());
        let body = "x".repeat(16 * 1024 * 1024);
        let started = Instant::now();
        assert_eq!(
            // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
            unsafe { raw_respond(request, 200, &body, "text/plain") },
            -1
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write timed out after 250 ms".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn head_response_declares_length_without_transmitting_body() {
        assert_exact_response(
            "HEAD",
            200,
            "hello",
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
        );
    }

    #[test]
    fn informational_response_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        assert_eq!(
            // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
            unsafe { raw_respond(request, 100, "ignored", "text/plain") },
            -1
        );
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: invalid status 100".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn no_content_response_omits_length_and_body() {
        assert_exact_response(
            "GET",
            204,
            "ignored",
            "HTTP/1.1 204 No Content\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
        );
    }

    #[test]
    fn reset_content_response_declares_zero_and_omits_body() {
        assert_exact_response(
            "GET",
            205,
            "ignored",
            "HTTP/1.1 205 Reset Content\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    }

    #[test]
    fn not_modified_response_declares_representation_length_without_body() {
        assert_exact_response(
            "GET",
            304,
            "hello",
            "HTTP/1.1 304 Not Modified\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
        );
    }

    #[test]
    fn successful_connect_returns_unsupported_write_error() {
        let (server, request, receiver) = request_with_method("CONNECT");
        assert_eq!(
            // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
            unsafe { raw_respond(request, 200, "", "application/octet-stream") },
            -1
        );
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: successful CONNECT tunnelling is not supported".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn second_response_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        // SAFETY: `request` is a live handle; the first response frees it, so the second call is expected to report `write failed: response already sent`.
        unsafe {
            assert!(raw_respond(request, 204, "", "text/plain") > 0);
            assert_eq!(raw_redirect(request, "/"), -1);
        }
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            "HTTP/1.1 204 No Content\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(
            error_snapshot(),
            (-1, 5, "write failed: response already sent".to_owned())
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn content_type_injection_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        assert_eq!(
            // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
            unsafe { raw_respond(request, 200, "body", "text/plain\r\nX: bad") },
            -1
        );
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: Content-Type contained CR or LF".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn content_type_with_nul_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        assert_eq!(
            // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
            unsafe { raw_respond(request, 200, "body", "text/plain\0ignored") },
            -1
        );
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: Content-Type contained NUL".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn status_outside_http_range_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        assert_eq!(
            // SAFETY: `request` is a live handle; `body`/`content_type` are `&str`s whose pointers and lengths match.
            unsafe { raw_respond(request, 99, "body", "text/plain") },
            -1
        );
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: invalid status 99".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn redirect_writes_exact_response() {
        let (server, request, receiver) = request_with_client();
        // SAFETY: `request` is a live handle and `location` is a `&str`; its pointer/length match.
        let written = unsafe { raw_redirect(request, "/next") };
        let expected = "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        assert_eq!(written, i64::try_from(expected.len()).unwrap());
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            expected
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
    }

    #[test]
    fn location_injection_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        // SAFETY: `request` is a live handle and `location` is a `&str`; its pointer/length match.
        assert_eq!(unsafe { raw_redirect(request, "/\r\nX: bad") }, -1);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: Location contained CR or LF".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn location_with_nul_returns_write_error() {
        let (server, request, receiver) = request_with_client();
        // SAFETY: `request` is a live handle and `location` is a `&str`; its pointer/length match.
        assert_eq!(unsafe { raw_redirect(request, "/next\0ignored") }, -1);
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Write as i32,
                "write failed: Location contained NUL".to_owned()
            )
        );
        // SAFETY: `request`/`server` are the live handles accepted/listened earlier in this test, each freed at most once here.
        unsafe {
            hew_ecosystem_http_request_free(request);
            hew_ecosystem_http_server_close(server);
        }
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn multibyte_percent_input_returns_exact_utf8() {
        // SAFETY: `input` is a `&str`; its pointer and byte length match.
        let value = unsafe { owned_string(raw_url_decode("caf%C3%A9+menu")) };
        assert_eq!(value, "café menu");
        assert_eq!(error_snapshot(), (0, 0, String::new()));
    }

    #[test]
    fn malformed_percent_input_returns_decode_error() {
        // SAFETY: `input` is a `&str`; its pointer and byte length match.
        let value = unsafe { owned_string(raw_url_decode("bad%2")) };
        assert_eq!(value, "");
        assert_eq!(
            error_snapshot(),
            (
                -1,
                7,
                "decode failed: incomplete percent escape at byte 3".to_owned()
            )
        );
    }

    #[test]
    fn decoded_nul_returns_decode_error() {
        // SAFETY: `input` is a `&str`; its pointer and byte length match.
        let value = unsafe { owned_string(raw_url_decode("left%00right")) };
        assert_eq!(value, "");
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Decode as i32,
                "decode failed: result contained NUL".to_owned()
            )
        );
    }

    #[test]
    fn present_form_field_returns_exact_decoded_value() {
        // SAFETY: `body`/`key` are `&str`s; their pointers and byte lengths match.
        let value = unsafe { owned_string(raw_form_value("title=caf%C3%A9+menu&empty=", "title")) };
        assert_eq!(value, "café menu");
        assert_eq!(error_snapshot(), (0, 0, String::new()));
    }

    #[test]
    fn missing_form_field_returns_missing_status() {
        // SAFETY: `body`/`key` are `&str`s; their pointers and byte lengths match.
        let value = unsafe { owned_string(raw_form_value("title=hello", "body")) };
        assert_eq!(value, "");
        assert_eq!(error_snapshot(), (-1, 8, "body".to_owned()));
    }

    #[test]
    fn malformed_form_value_returns_decode_error() {
        // SAFETY: `body`/`key` are `&str`s; their pointers and byte lengths match.
        let value = unsafe { owned_string(raw_form_value("title=bad%GG", "title")) };
        assert_eq!(value, "");
        assert_eq!(
            error_snapshot(),
            (
                -1,
                7,
                "decode failed: invalid percent escape at byte 3".to_owned()
            )
        );
    }

    #[test]
    fn null_request_accessors_return_internal_error_and_empty_strings() {
        // SAFETY: `request` is the live handle accepted earlier in this test.
        unsafe {
            assert_eq!(owned_string(hew_ecosystem_http_request_method(0)), "");
            assert_eq!(error_snapshot().1, ErrorKind::Internal as i32);
            assert_eq!(owned_string(hew_ecosystem_http_request_path(0)), "");
            assert_eq!(owned_string(hew_ecosystem_http_request_query(0)), "");
            assert_eq!(owned_string(hew_ecosystem_http_request_body(0)), "");
            hew_ecosystem_http_request_free(0);
            hew_ecosystem_http_server_close(0);
        }
        assert_eq!(
            error_snapshot(),
            (
                -1,
                ErrorKind::Internal as i32,
                "request accessor failed: null request handle".to_owned()
            )
        );
    }
}
