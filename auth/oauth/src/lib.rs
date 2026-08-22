//! Hew runtime: `auth_oauth` module.
//!
//! Provides OAuth 2.0 client flows for compiled Hew programs.
//! Returned strings are allocation-base, NUL-terminated `libc::malloc`
//! buffers. Hew takes ownership and releases the allocation base. Opaque
//! handles are freed via the corresponding free/close functions.
//!
//! Uses `ureq` directly for HTTP rather than `std::net::http_client` because
//! the stdlib HTTP client does not yet expose response status codes, headers,
//! or body text. Replace with stdlib once `Response` gains those capabilities.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    os::raw::c_char,
    sync::atomic::{AtomicI64, Ordering},
};

static ACTIVE_CLIENTS: AtomicI64 = AtomicI64::new(0);
static ACTIVE_TOKENS: AtomicI64 = AtomicI64::new(0);

fn str_to_malloc(value: &str) -> *mut c_char {
    if value.as_bytes().contains(&0) {
        return std::ptr::null_mut();
    }
    // SAFETY: requesting value.len() + 1 bytes; the null check below covers allocation failure.
    let output = unsafe { libc::malloc(value.len() + 1) }.cast::<u8>();
    if output.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: output was just allocated with value.len() + 1 bytes and is non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), output, value.len());
        output.add(value.len()).write(0);
    }
    output.cast::<c_char>()
}

unsafe fn utf8_with_len<'a>(value: *const c_char, len: i64) -> Option<&'a str> {
    let len = usize::try_from(len).ok()?;
    if value.is_null() {
        return (len == 0).then_some("");
    }
    // SAFETY: value addresses len readable bytes per caller contract (checked above).
    let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), len) };
    std::str::from_utf8(bytes).ok()
}

/// Opaque OAuth client handle holding client credentials.
///
/// Returned by [`hew_oauth_new`]. Must be closed with [`hew_oauth_close`].
pub struct HewOauthClient {
    client_id: String,
    client_secret: String,
    pending_state: String,
    pending_code_verifier: String,
}

impl std::fmt::Debug for HewOauthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HewOauthClient").finish_non_exhaustive()
    }
}

/// Token response returned by OAuth token endpoint calls.
///
/// Returned by [`hew_oauth_client_credentials`], [`hew_oauth_exchange_code`],
/// and [`hew_oauth_refresh`]. Must be freed with [`hew_oauth_token_free`].
#[derive(Debug)]
pub struct HewOauthToken {
    ok: bool,
    access_token: String,
    token_type: String,
    expires_in: i64,
    refresh_token: String,
    scope: String,
    error_status: i64,
    error_message: String,
}

/// Percent-encode a string for use in `application/x-www-form-urlencoded` bodies
/// and query strings. Unreserved characters (RFC 3986) are passed through as-is.
fn url_encode(s: &str) -> String {
    s.as_bytes()
        .iter()
        .flat_map(|&b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                vec![char::from(b)]
            } else {
                format!("%{b:02X}").chars().collect()
            }
        })
        .collect()
}

fn random_urlsafe(bytes_len: usize) -> Result<String, String> {
    let mut bytes = vec![0_u8; bytes_len];
    getrandom::getrandom(&mut bytes).map_err(|err| format!("csprng_failed: {err}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn generate_state_value() -> Result<String, String> {
    random_urlsafe(32)
}

fn generate_code_verifier() -> Result<String, String> {
    random_urlsafe(32)
}

fn code_challenge_for_verifier(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn token_error(status: i64, message: impl Into<String>) -> *mut HewOauthToken {
    ACTIVE_TOKENS.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(Box::new(HewOauthToken {
        ok: false,
        access_token: String::new(),
        token_type: String::new(),
        expires_in: -1,
        refresh_token: String::new(),
        scope: String::new(),
        error_status: status,
        error_message: message.into(),
    }))
}

fn oauth_error_message(json: &serde_json::Value, fallback: &str) -> String {
    let Some(error) = json["error"].as_str() else {
        return fallback.to_owned();
    };
    match json["error_description"].as_str() {
        Some(description) if !description.is_empty() => format!("{error}: {description}"),
        _ => error.to_owned(),
    }
}

/// Parse a JSON token response into a heap-allocated [`HewOauthToken`].
///
/// Returns `None` if `access_token` is missing (required field).
fn parse_token_response(json: &serde_json::Value) -> Result<*mut HewOauthToken, String> {
    if json["error"].as_str().is_some() {
        return Err(oauth_error_message(json, "oauth_error"));
    }
    let access_token = json["access_token"]
        .as_str()
        .ok_or_else(|| "missing access_token in token response".to_owned())?
        .to_owned();
    let token_type = json["token_type"].as_str().unwrap_or("Bearer").to_owned();
    let expires_in = json["expires_in"].as_i64().unwrap_or(-1);
    let refresh_token = json["refresh_token"].as_str().unwrap_or("").to_owned();
    let scope = json["scope"].as_str().unwrap_or("").to_owned();
    ACTIVE_TOKENS.fetch_add(1, Ordering::Relaxed);
    Ok(Box::into_raw(Box::new(HewOauthToken {
        ok: true,
        access_token,
        token_type,
        expires_in,
        refresh_token,
        scope,
        error_status: 0,
        error_message: String::new(),
    })))
}

fn parse_token_body(status: i64, body: &str) -> *mut HewOauthToken {
    let json: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(err) => return token_error(status, format!("invalid_json: {err}")),
    };

    if !(200..300).contains(&status) {
        return token_error(status, oauth_error_message(&json, "http_error"));
    }

    match parse_token_response(&json) {
        Ok(token) => token,
        Err(message) => token_error(status, message),
    }
}

fn post_form_token(token_url: &str, form: &str) -> *mut HewOauthToken {
    match ureq::post(token_url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(form)
    {
        Ok(resp) => {
            let status = i64::from(resp.status());
            match resp.into_string() {
                Ok(body) => parse_token_body(status, &body),
                Err(err) => token_error(status, format!("read_response_failed: {err}")),
            }
        }
        Err(ureq::Error::Status(status, resp)) => match resp.into_string() {
            Ok(body) => parse_token_body(i64::from(status), &body),
            Err(err) => token_error(i64::from(status), format!("read_response_failed: {err}")),
        },
        Err(ureq::Error::Transport(err)) => token_error(0, format!("transport_error: {err}")),
    }
}

/// Create a new OAuth client with the given client credentials.
///
/// Returns a heap-allocated [`HewOauthClient`] on success, or null on error.
/// The caller must close the client with [`hew_oauth_close`].
///
/// # Safety
///
/// Each pointer must address its paired length in readable bytes containing
/// valid UTF-8. A null pointer is allowed only when its paired length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_new(
    client_id: *const c_char,
    client_id_len: i64,
    client_secret: *const c_char,
    client_secret_len: i64,
) -> *mut HewOauthClient {
    // SAFETY: client_id addresses client_id_len readable bytes per caller contract.
    let Some(client_id_str) = (unsafe { utf8_with_len(client_id, client_id_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: client_secret addresses client_secret_len readable bytes per caller contract.
    let Some(client_secret_str) = (unsafe { utf8_with_len(client_secret, client_secret_len) })
    else {
        return std::ptr::null_mut();
    };
    ACTIVE_CLIENTS.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(Box::new(HewOauthClient {
        client_id: client_id_str.to_owned(),
        client_secret: client_secret_str.to_owned(),
        pending_state: String::new(),
        pending_code_verifier: String::new(),
    }))
}

/// Obtain an access token using the client credentials grant (machine-to-machine).
///
/// POSTs to `token_url` with `grant_type=client_credentials`. `scope` may be
/// empty. Returns a heap-allocated [`HewOauthToken`] on success, or null on
/// error (network failure or missing `access_token` in response).
///
/// The caller must free the token with [`hew_oauth_token_free`].
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string pointer must address its paired length in readable bytes
///   containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_client_credentials(
    client_ptr: *mut HewOauthClient,
    token_url: *const c_char,
    token_url_len: i64,
    scope: *const c_char,
    scope_len: i64,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token_url addresses token_url_len readable bytes per caller contract.
    let Some(token_url_str) = (unsafe { utf8_with_len(token_url, token_url_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: scope addresses scope_len readable bytes per caller contract.
    let scope_str = unsafe { utf8_with_len(scope, scope_len) }.unwrap_or("");
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &*client_ptr };

    let mut form = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}",
        url_encode(&client.client_id),
        url_encode(&client.client_secret),
    );
    if !scope_str.is_empty() {
        let _ = write!(form, "&scope={}", url_encode(scope_str));
    }

    post_form_token(token_url_str, &form)
}

/// Generate an authorization URL for the authorization code grant.
///
/// Builds a URL with `response_type=code` and the provided parameters.
/// Returns an allocation-base, NUL-terminated `libc::malloc` buffer that Hew
/// takes ownership of. Returns null when an input or allocation is invalid.
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string pointer must address its paired length in readable bytes
///   containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_auth_url(
    client_ptr: *mut HewOauthClient,
    auth_url: *const c_char,
    auth_url_len: i64,
    redirect_uri: *const c_char,
    redirect_uri_len: i64,
    scope: *const c_char,
    scope_len: i64,
    state: *const c_char,
    state_len: i64,
) -> *mut c_char {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: auth_url addresses auth_url_len readable bytes per caller contract.
    let Some(auth_url_str) = (unsafe { utf8_with_len(auth_url, auth_url_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: redirect_uri addresses redirect_uri_len readable bytes per caller contract.
    let Some(redirect_uri_str) = (unsafe { utf8_with_len(redirect_uri, redirect_uri_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: scope addresses scope_len readable bytes per caller contract.
    let scope_str = unsafe { utf8_with_len(scope, scope_len) }.unwrap_or("");
    // SAFETY: state addresses state_len readable bytes per caller contract.
    let state_str = unsafe { utf8_with_len(state, state_len) }.unwrap_or("");
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &mut *client_ptr };

    let state = if state_str.is_empty() {
        match generate_state_value() {
            Ok(value) => value,
            Err(_) => return std::ptr::null_mut(),
        }
    } else {
        state_str.to_owned()
    };
    let Ok(code_verifier) = generate_code_verifier() else {
        return std::ptr::null_mut();
    };
    let code_challenge = code_challenge_for_verifier(&code_verifier);
    client.pending_state.clone_from(&state);
    client.pending_code_verifier = code_verifier;

    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        auth_url_str,
        url_encode(&client.client_id),
        url_encode(redirect_uri_str),
        url_encode(scope_str),
        url_encode(&state),
        url_encode(&code_challenge),
    );
    str_to_malloc(&url)
}

/// Exchange an authorization code for a token (authorization code grant).
///
/// POSTs to `token_url` with `grant_type=authorization_code`. Returns a
/// heap-allocated [`HewOauthToken`] on success, or null on error.
///
/// The caller must free the token with [`hew_oauth_token_free`].
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string pointer must address its paired length in readable bytes
///   containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_exchange_code(
    client_ptr: *mut HewOauthClient,
    token_url: *const c_char,
    token_url_len: i64,
    code: *const c_char,
    code_len: i64,
    redirect_uri: *const c_char,
    redirect_uri_len: i64,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token_url addresses token_url_len readable bytes per caller contract.
    let Some(token_url_str) = (unsafe { utf8_with_len(token_url, token_url_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: code addresses code_len readable bytes per caller contract.
    let Some(code_str) = (unsafe { utf8_with_len(code, code_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: redirect_uri addresses redirect_uri_len readable bytes per caller contract.
    let Some(redirect_uri_str) = (unsafe { utf8_with_len(redirect_uri, redirect_uri_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &*client_ptr };

    if client.pending_code_verifier.is_empty() {
        return token_error(0, "missing_code_verifier: call auth_url before exchange_code or use exchange_code_with_verifier");
    }

    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}&code_verifier={}",
        url_encode(code_str),
        url_encode(redirect_uri_str),
        url_encode(&client.client_id),
        url_encode(&client.client_secret),
        url_encode(&client.pending_code_verifier),
    );

    post_form_token(token_url_str, &form)
}

/// Exchange an authorization code using an explicit PKCE verifier.
///
/// This is useful when the verifier is stored outside of this client handle.
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string pointer must address its paired length in readable bytes
///   containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_exchange_code_with_verifier(
    client_ptr: *mut HewOauthClient,
    token_url: *const c_char,
    token_url_len: i64,
    code: *const c_char,
    code_len: i64,
    redirect_uri: *const c_char,
    redirect_uri_len: i64,
    code_verifier: *const c_char,
    code_verifier_len: i64,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return token_error(0, "null_client");
    }
    // SAFETY: token_url addresses token_url_len readable bytes per caller contract.
    let Some(token_url_str) = (unsafe { utf8_with_len(token_url, token_url_len) }) else {
        return token_error(0, "invalid_token_url");
    };
    // SAFETY: code addresses code_len readable bytes per caller contract.
    let Some(code_str) = (unsafe { utf8_with_len(code, code_len) }) else {
        return token_error(0, "invalid_code");
    };
    // SAFETY: redirect_uri addresses redirect_uri_len readable bytes per caller contract.
    let Some(redirect_uri_str) = (unsafe { utf8_with_len(redirect_uri, redirect_uri_len) }) else {
        return token_error(0, "invalid_redirect_uri");
    };
    // SAFETY: code_verifier addresses code_verifier_len readable bytes per caller contract.
    let Some(code_verifier_str) = (unsafe { utf8_with_len(code_verifier, code_verifier_len) })
    else {
        return token_error(0, "invalid_code_verifier");
    };
    if code_verifier_str.is_empty() {
        return token_error(0, "empty_code_verifier");
    }
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &*client_ptr };

    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}&code_verifier={}",
        url_encode(code_str),
        url_encode(redirect_uri_str),
        url_encode(&client.client_id),
        url_encode(&client.client_secret),
        url_encode(code_verifier_str),
    );

    post_form_token(token_url_str, &form)
}

/// Return the current generated OAuth state for callback validation as an
/// allocation-base, NUL-terminated `libc::malloc` buffer.
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_current_state(client_ptr: *const HewOauthClient) -> *mut c_char {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    str_to_malloc(&unsafe { &*client_ptr }.pending_state)
}

/// Return the current generated PKCE code verifier for external storage as an
/// allocation-base, NUL-terminated `libc::malloc` buffer.
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_current_code_verifier(
    client_ptr: *const HewOauthClient,
) -> *mut c_char {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    str_to_malloc(&unsafe { &*client_ptr }.pending_code_verifier)
}

/// Validate a callback state against the current generated state.
///
/// Returns 1 on match and 0 on mismatch or missing state.
///
/// # Safety
///
/// `client_ptr` must be valid. `callback_state` must address
/// `callback_state_len` readable bytes containing valid UTF-8; null is allowed
/// only when the length is zero.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_validate_state(
    client_ptr: *const HewOauthClient,
    callback_state: *const c_char,
    callback_state_len: i64,
) -> i32 {
    if client_ptr.is_null() {
        return 0;
    }
    // SAFETY: callback_state addresses callback_state_len readable bytes per caller contract.
    let Some(callback_state_str) = (unsafe { utf8_with_len(callback_state, callback_state_len) })
    else {
        return 0;
    };
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &*client_ptr };
    i32::from(!client.pending_state.is_empty() && client.pending_state == callback_state_str)
}

/// Refresh an access token using a refresh token.
///
/// POSTs to `token_url` with `grant_type=refresh_token`. Returns a
/// heap-allocated [`HewOauthToken`] on success, or null on error.
///
/// The caller must free the token with [`hew_oauth_token_free`].
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string pointer must address its paired length in readable bytes
///   containing valid UTF-8; null is allowed only with a zero length.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_refresh(
    client_ptr: *mut HewOauthClient,
    token_url: *const c_char,
    token_url_len: i64,
    refresh_token: *const c_char,
    refresh_token_len: i64,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token_url addresses token_url_len readable bytes per caller contract.
    let Some(token_url_str) = (unsafe { utf8_with_len(token_url, token_url_len) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: refresh_token addresses refresh_token_len readable bytes per caller contract.
    let Some(refresh_token_str) = (unsafe { utf8_with_len(refresh_token, refresh_token_len) })
    else {
        return std::ptr::null_mut();
    };
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &*client_ptr };

    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        url_encode(refresh_token_str),
        url_encode(&client.client_id),
        url_encode(&client.client_secret),
    );

    post_form_token(token_url_str, &form)
}

/// Return the access token as an allocation-base, NUL-terminated
/// `libc::malloc` buffer.
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_access_token(token: *const HewOauthToken) -> *mut c_char {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    str_to_malloc(&unsafe { &*token }.access_token)
}

/// Return 1 when the token response is successful, or 0 when it carries an error.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_is_ok(token: *const HewOauthToken) -> i32 {
    if token.is_null() {
        return 0;
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    i32::from(unsafe { &*token }.ok)
}

/// Return the HTTP status for token errors, or 0 when unavailable/not an error.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_error_status(token: *const HewOauthToken) -> i64 {
    if token.is_null() {
        return -1;
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    unsafe { &*token }.error_status
}

/// Return an error diagnostic for failed token responses as an allocation-base,
/// NUL-terminated `libc::malloc` buffer.
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_error_message(token: *const HewOauthToken) -> *mut c_char {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    str_to_malloc(&unsafe { &*token }.error_message)
}

/// Return the token type as an allocation-base, NUL-terminated
/// `libc::malloc` buffer (usually "Bearer").
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_type(token: *const HewOauthToken) -> *mut c_char {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    str_to_malloc(&unsafe { &*token }.token_type)
}

/// Return the token expiry in seconds from issuance, or -1 if not provided.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_expires_in(token: *const HewOauthToken) -> i64 {
    if token.is_null() {
        return -1;
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    unsafe { &*token }.expires_in
}

/// Return the refresh token as an allocation-base, NUL-terminated
/// `libc::malloc` buffer, or an allocated empty string if not provided.
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_refresh_token(token: *const HewOauthToken) -> *mut c_char {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    str_to_malloc(&unsafe { &*token }.refresh_token)
}

/// Return the token scope as an allocation-base, NUL-terminated
/// `libc::malloc` buffer, or an allocated empty string if not provided.
///
/// Hew takes ownership and releases the allocation base.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_scope(token: *const HewOauthToken) -> *mut c_char {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    str_to_malloc(&unsafe { &*token }.scope)
}

/// Free a [`HewOauthToken`] previously returned by a token-producing function.
///
/// # Safety
///
/// `token` must be a pointer previously returned by a token-producing function,
/// and must not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_free(token: *mut HewOauthToken) {
    if token.is_null() {
        return;
    }
    // SAFETY: token was allocated with Box::into_raw in parse_token_response.
    let _ = unsafe { Box::from_raw(token) };
    ACTIVE_TOKENS.fetch_sub(1, Ordering::Relaxed);
    // Box is dropped here, freeing all owned Strings.
}

/// Close an OAuth client and release its resources.
///
/// # Safety
///
/// `client_ptr` must be a pointer previously returned by [`hew_oauth_new`],
/// and must not have been closed already.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_close(client_ptr: *mut HewOauthClient) {
    if client_ptr.is_null() {
        return;
    }
    // SAFETY: client_ptr was allocated with Box::into_raw in hew_oauth_new.
    let _ = unsafe { Box::from_raw(client_ptr) };
    ACTIVE_CLIENTS.fetch_sub(1, Ordering::Relaxed);
    // Box is dropped here, freeing all owned Strings.
}

/// Return the number of live client handles (for lifecycle verification).
#[no_mangle]
pub extern "C" fn hew_oauth_client_count() -> i64 {
    ACTIVE_CLIENTS.load(Ordering::Relaxed)
}

/// Return the number of live token handles (for lifecycle verification).
#[no_mangle]
pub extern "C" fn hew_oauth_token_count() -> i64 {
    ACTIVE_TOKENS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("foo@bar.com"), "foo%40bar.com");
        assert_eq!(url_encode("safe-string_123"), "safe-string_123");
        assert_eq!(url_encode("ümlaut"), "%C3%BCmlaut");
    }

    #[test]
    fn test_parse_token_response() {
        let json = serde_json::json!({
            "access_token": "tok123",
            "token_type": "Bearer",
            "expires_in": 3600,
        });
        let ptr = parse_token_response(&json).unwrap();
        // SAFETY: ptr is a valid HewOauthToken we just created.
        unsafe {
            assert_eq!((*ptr).access_token, "tok123");
            assert_eq!((*ptr).expires_in, 3600);
            drop(Box::from_raw(ptr));
        }
    }

    #[test]
    fn test_parse_token_response_missing_access_token() {
        let json = serde_json::json!({ "token_type": "Bearer" });
        assert!(parse_token_response(&json).is_err());
    }

    #[test]
    fn test_parse_token_response_defaults() {
        let json = serde_json::json!({ "access_token": "abc" });
        let ptr = parse_token_response(&json).unwrap();
        // SAFETY: ptr is a valid HewOauthToken we just created.
        unsafe {
            assert_eq!((*ptr).token_type, "Bearer");
            assert_eq!((*ptr).expires_in, -1);
            assert_eq!((*ptr).refresh_token, "");
            assert_eq!((*ptr).scope, "");
            drop(Box::from_raw(ptr));
        }
    }

    #[test]
    fn test_token_accessors() {
        let json = serde_json::json!({
            "access_token": "mytoken",
            "token_type": "Bearer",
            "expires_in": 7200,
            "refresh_token": "refresh_abc",
            "scope": "read write",
        });
        let ptr = parse_token_response(&json).unwrap();

        // SAFETY: ptr is a valid HewOauthToken we just created.
        unsafe {
            let at = hew_oauth_token_access_token(ptr);
            assert!(!at.is_null());
            assert_eq!(CStr::from_ptr(at).to_str().unwrap(), "mytoken");
            libc::free(at.cast());

            let tt = hew_oauth_token_type(ptr);
            assert!(!tt.is_null());
            assert_eq!(CStr::from_ptr(tt).to_str().unwrap(), "Bearer");
            libc::free(tt.cast());

            assert_eq!(hew_oauth_token_expires_in(ptr), 7200);

            let rt = hew_oauth_token_refresh_token(ptr);
            assert!(!rt.is_null());
            assert_eq!(CStr::from_ptr(rt).to_str().unwrap(), "refresh_abc");
            libc::free(rt.cast());

            let sc = hew_oauth_token_scope(ptr);
            assert!(!sc.is_null());
            assert_eq!(CStr::from_ptr(sc).to_str().unwrap(), "read write");
            libc::free(sc.cast());

            hew_oauth_token_free(ptr);
        }
    }

    #[test]
    fn test_null_token_accessors() {
        // SAFETY: passing null pointers — functions must handle them gracefully.
        unsafe {
            assert!(hew_oauth_token_access_token(std::ptr::null()).is_null());
            assert_eq!(hew_oauth_token_is_ok(std::ptr::null()), 0);
            assert_eq!(hew_oauth_token_error_status(std::ptr::null()), -1);
            assert!(hew_oauth_token_error_message(std::ptr::null()).is_null());
            assert!(hew_oauth_token_type(std::ptr::null()).is_null());
            assert_eq!(hew_oauth_token_expires_in(std::ptr::null()), -1);
            assert!(hew_oauth_token_refresh_token(std::ptr::null()).is_null());
            assert!(hew_oauth_token_scope(std::ptr::null()).is_null());
            hew_oauth_token_free(std::ptr::null_mut()); // must not panic
        }
    }

    #[test]
    fn test_url_encode_special_chars() {
        assert_eq!(url_encode("a+b=c&d"), "a%2Bb%3Dc%26d");
        // ~ is unreserved (RFC 3986), / is reserved and must be encoded
        assert_eq!(url_encode("~path.file-name_ok"), "~path.file-name_ok");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode(""), "");
    }

    #[test]
    fn embedded_nul_client_id_is_percent_encoded_not_truncated() {
        let client_id = b"left\0right";
        let secret = b"secret";
        let auth_url = b"https://example.test/authorize";
        let redirect = b"https://client.test/callback";
        let scope = b"openid";
        let state = b"known";
        // SAFETY: pointers/lengths are valid; exercises embedded-NUL handling for test fixtures.
        unsafe {
            let client = hew_oauth_new(
                client_id.as_ptr().cast(),
                len_i64(client_id.len()),
                secret.as_ptr().cast(),
                len_i64(secret.len()),
            );
            assert!(!client.is_null());
            let value = malloc_string(hew_oauth_auth_url(
                client,
                auth_url.as_ptr().cast(),
                len_i64(auth_url.len()),
                redirect.as_ptr().cast(),
                len_i64(redirect.len()),
                scope.as_ptr().cast(),
                len_i64(scope.len()),
                state.as_ptr().cast(),
                len_i64(state.len()),
            ));
            assert!(value.contains("client_id=left%00right"));
            hew_oauth_close(client);
        }
    }

    fn len_i64(n: usize) -> i64 {
        i64::try_from(n).expect("test fixture length fits in i64")
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn malloc_string(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        // SAFETY: ptr is a valid, NUL-terminated CString pointer produced by this test module.
        let value = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
        // SAFETY: ptr was allocated by str_to_malloc/libc::malloc and is freed exactly once here.
        unsafe { libc::free(ptr.cast()) };
        value
    }

    fn serve_once(status: u16, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 512];
            let header_end;
            loop {
                let n = stream.read(&mut chunk).unwrap();
                assert_ne!(n, 0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .and_then(|v| v.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                let n = stream.read(&mut chunk).unwrap();
                assert_ne!(n, 0);
                buf.extend_from_slice(&chunk[..n]);
            }
            let request_body =
                String::from_utf8(buf[header_end..header_end + content_length].to_vec()).unwrap();
            let reason = if status >= 400 { "Bad Request" } else { "OK" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            request_body
        });
        (url, handle)
    }

    #[test]
    fn test_pkce_state_generation_and_auth_url() {
        let client_id = cstr("client ü");
        let client_secret = cstr("secret");
        let auth_url = cstr("https://auth.example/authorize");
        let redirect_uri = cstr("https://app.example/callback");
        let scope = cstr("openid profile");
        let empty_state = cstr("");

        // SAFETY: all pointers/lengths come from valid CString/&str fixtures in this test.
        unsafe {
            let client = hew_oauth_new(
                client_id.as_ptr(),
                len_i64(client_id.as_bytes().len()),
                client_secret.as_ptr(),
                len_i64(client_secret.as_bytes().len()),
            );
            assert!(!client.is_null());

            let url = malloc_string(hew_oauth_auth_url(
                client,
                auth_url.as_ptr(),
                len_i64(auth_url.as_bytes().len()),
                redirect_uri.as_ptr(),
                len_i64(redirect_uri.as_bytes().len()),
                scope.as_ptr(),
                len_i64(scope.as_bytes().len()),
                empty_state.as_ptr(),
                len_i64(empty_state.as_bytes().len()),
            ));
            let state = malloc_string(hew_oauth_current_state(client));
            let verifier = malloc_string(hew_oauth_current_code_verifier(client));
            let expected_challenge = code_challenge_for_verifier(&verifier);

            assert_eq!(state.len(), 43);
            assert_eq!(verifier.len(), 43);
            assert_eq!(
                hew_oauth_validate_state(client, state.as_ptr().cast(), len_i64(state.len())),
                1
            );
            assert_eq!(
                hew_oauth_validate_state(client, b"wrong".as_ptr().cast(), 5),
                0
            );
            assert!(url.contains("response_type=code"));
            assert!(url.contains("client_id=client%20%C3%BC"));
            assert!(url.contains("redirect_uri=https%3A%2F%2Fapp.example%2Fcallback"));
            assert!(url.contains("scope=openid%20profile"));
            assert!(url.contains(&format!("state={state}")));
            assert!(url.contains(&format!("code_challenge={expected_challenge}")));
            assert!(url.contains("code_challenge_method=S256"));

            hew_oauth_close(client);
        }
    }

    #[test]
    fn test_exchange_code_posts_pkce_verifier() {
        let (url, handle) = serve_once(
            200,
            r#"{"access_token":"access","token_type":"Bearer","expires_in":60}"#,
        );
        let client_id = cstr("client");
        let client_secret = cstr("secret");
        let auth_url = cstr("https://auth.example/authorize");
        let redirect_uri = cstr("https://app.example/callback");
        let scope = cstr("");
        let state = cstr("caller-state");
        let token_url = cstr(&url);
        let code = cstr("code 123");

        // SAFETY: all pointers/lengths come from valid CString/&str fixtures in this test.
        unsafe {
            let client = hew_oauth_new(
                client_id.as_ptr(),
                len_i64(client_id.as_bytes().len()),
                client_secret.as_ptr(),
                len_i64(client_secret.as_bytes().len()),
            );
            let auth_url_ptr = hew_oauth_auth_url(
                client,
                auth_url.as_ptr(),
                len_i64(auth_url.as_bytes().len()),
                redirect_uri.as_ptr(),
                len_i64(redirect_uri.as_bytes().len()),
                scope.as_ptr(),
                len_i64(scope.as_bytes().len()),
                state.as_ptr(),
                len_i64(state.as_bytes().len()),
            );
            libc::free(auth_url_ptr.cast());
            let verifier = malloc_string(hew_oauth_current_code_verifier(client));

            let token = hew_oauth_exchange_code(
                client,
                token_url.as_ptr(),
                len_i64(token_url.as_bytes().len()),
                code.as_ptr(),
                len_i64(code.as_bytes().len()),
                redirect_uri.as_ptr(),
                len_i64(redirect_uri.as_bytes().len()),
            );
            assert_eq!(hew_oauth_token_is_ok(token), 1);
            hew_oauth_token_free(token);
            hew_oauth_close(client);

            let form = handle.join().unwrap();
            assert!(form.contains("grant_type=authorization_code"));
            assert!(form.contains("code=code%20123"));
            assert!(form.contains("redirect_uri=https%3A%2F%2Fapp.example%2Fcallback"));
            assert!(form.contains("client_id=client"));
            assert!(form.contains("client_secret=secret"));
            assert!(form.contains(&format!("code_verifier={verifier}")));
        }
    }

    #[test]
    fn test_client_credentials_and_error_paths_are_distinguishable() {
        let (url, handle) = serve_once(
            400,
            r#"{"error":"invalid_client","error_description":"bad credentials"}"#,
        );
        let client_id = cstr("client");
        let client_secret = cstr("secret");
        let token_url = cstr(&url);
        let scope = cstr("read ü");

        // SAFETY: all pointers/lengths come from valid CString/&str fixtures in this test.
        unsafe {
            let client = hew_oauth_new(
                client_id.as_ptr(),
                len_i64(client_id.as_bytes().len()),
                client_secret.as_ptr(),
                len_i64(client_secret.as_bytes().len()),
            );
            let token = hew_oauth_client_credentials(
                client,
                token_url.as_ptr(),
                len_i64(token_url.as_bytes().len()),
                scope.as_ptr(),
                len_i64(scope.as_bytes().len()),
            );
            assert!(!token.is_null());
            assert_eq!(hew_oauth_token_is_ok(token), 0);
            assert_eq!(hew_oauth_token_error_status(token), 400);
            let err = malloc_string(hew_oauth_token_error_message(token));
            assert_eq!(err, "invalid_client: bad credentials");
            hew_oauth_token_free(token);
            hew_oauth_close(client);

            let form = handle.join().unwrap();
            assert!(form.contains("grant_type=client_credentials"));
            assert!(form.contains("scope=read%20%C3%BC"));
        }

        let token = parse_token_body(200, "not-json");
        // SAFETY: token was returned by parse_token_body above and is freed exactly once here.
        unsafe {
            assert_eq!(hew_oauth_token_is_ok(token), 0);
            assert!(malloc_string(hew_oauth_token_error_message(token)).starts_with("invalid_json"));
            hew_oauth_token_free(token);
        }
    }

    #[test]
    fn test_refresh_posts_form_params() {
        let (url, handle) = serve_once(
            200,
            r#"{"access_token":"new-access","token_type":"Bearer","refresh_token":"next"}"#,
        );
        let client_id = cstr("client");
        let client_secret = cstr("secret");
        let token_url = cstr(&url);
        let refresh_token = cstr("refresh token");

        // SAFETY: all pointers/lengths come from valid CString/&str fixtures in this test.
        unsafe {
            let client = hew_oauth_new(
                client_id.as_ptr(),
                len_i64(client_id.as_bytes().len()),
                client_secret.as_ptr(),
                len_i64(client_secret.as_bytes().len()),
            );
            let token = hew_oauth_refresh(
                client,
                token_url.as_ptr(),
                len_i64(token_url.as_bytes().len()),
                refresh_token.as_ptr(),
                len_i64(refresh_token.as_bytes().len()),
            );
            assert_eq!(hew_oauth_token_is_ok(token), 1);
            let access = malloc_string(hew_oauth_token_access_token(token));
            assert_eq!(access, "new-access");
            hew_oauth_token_free(token);
            hew_oauth_close(client);

            let form = handle.join().unwrap();
            assert!(form.contains("grant_type=refresh_token"));
            assert!(form.contains("refresh_token=refresh%20token"));
            assert!(form.contains("client_id=client"));
            assert!(form.contains("client_secret=secret"));
        }
    }

    #[test]
    fn test_exchange_without_pkce_returns_error_token() {
        let client_id = cstr("client");
        let client_secret = cstr("secret");
        let token_url = cstr("http://127.0.0.1:9");
        let code = cstr("code");
        let redirect_uri = cstr("https://app.example/callback");

        // SAFETY: all pointers/lengths come from valid CString/&str fixtures in this test.
        unsafe {
            let client = hew_oauth_new(
                client_id.as_ptr(),
                len_i64(client_id.as_bytes().len()),
                client_secret.as_ptr(),
                len_i64(client_secret.as_bytes().len()),
            );
            let token = hew_oauth_exchange_code(
                client,
                token_url.as_ptr(),
                len_i64(token_url.as_bytes().len()),
                code.as_ptr(),
                len_i64(code.as_bytes().len()),
                redirect_uri.as_ptr(),
                len_i64(redirect_uri.as_bytes().len()),
            );
            assert_eq!(hew_oauth_token_is_ok(token), 0);
            assert_eq!(
                malloc_string(hew_oauth_token_error_message(token)),
                "missing_code_verifier: call auth_url before exchange_code or use exchange_code_with_verifier"
            );
            hew_oauth_token_free(token);
            hew_oauth_close(client);
        }
    }
}
