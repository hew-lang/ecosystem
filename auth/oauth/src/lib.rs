//! Hew runtime: `auth_oauth` module.
//!
//! Provides OAuth 2.0 client flows for compiled Hew programs.
//! Strings cross the boundary as managed Hew strings: inbound handles are
//! borrowed for the call, and returned handles are freshly allocated owners
//! the caller releases. Opaque client and token handles are freed via the
//! corresponding close/free functions.
//!
//! Uses `ureq` directly for HTTP rather than `std::net::http_client` because
//! the stdlib HTTP client does not yet expose response status codes, headers,
//! or body text. Replace with stdlib once `Response` gains those capabilities.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
#[cfg(test)]
use hew_cabi::string::string_release;
use hew_cabi::string::{string_as_str, string_from_str, HewString};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    sync::atomic::{AtomicI64, Ordering},
};

static ACTIVE_CLIENTS: AtomicI64 = AtomicI64::new(0);
static ACTIVE_TOKENS: AtomicI64 = AtomicI64::new(0);

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
/// Returns a heap-allocated [`HewOauthClient`]. The caller must close the
/// client with [`hew_oauth_close`].
///
/// # Safety
///
/// Each string argument must be null (the empty string) or a live managed
/// Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_new(
    client_id: *const HewString,
    client_secret: *const HewString,
) -> *mut HewOauthClient {
    // SAFETY: `client_id` is a managed handle borrowed for this call.
    let client_id = unsafe { string_as_str(client_id) }.to_owned();
    // SAFETY: `client_secret` is a managed handle borrowed for this call.
    let client_secret = unsafe { string_as_str(client_secret) }.to_owned();
    ACTIVE_CLIENTS.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(Box::new(HewOauthClient {
        client_id,
        client_secret,
        pending_state: String::new(),
        pending_code_verifier: String::new(),
    }))
}

/// Obtain an access token using the client credentials grant (machine-to-machine).
///
/// POSTs to `token_url` with `grant_type=client_credentials`. `scope` may be
/// empty. Returns a heap-allocated [`HewOauthToken`], carrying the endpoint
/// diagnostic when the exchange failed, or null when `client_ptr` is null.
///
/// The caller must free the token with [`hew_oauth_token_free`].
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string argument must be null (the empty string) or a live managed
///   Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_client_credentials(
    client_ptr: *mut HewOauthClient,
    token_url: *const HewString,
    scope: *const HewString,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `token_url` is a managed handle borrowed for this call.
    let token_url_str = unsafe { string_as_str(token_url) };
    // SAFETY: `scope` is a managed handle borrowed for this call.
    let scope_str = unsafe { string_as_str(scope) };
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
/// Returns a freshly allocated managed string the caller releases. Returns
/// null (the empty string) when `client_ptr` is null or the CSPRNG fails.
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string argument must be null (the empty string) or a live managed
///   Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_auth_url(
    client_ptr: *mut HewOauthClient,
    auth_url: *const HewString,
    redirect_uri: *const HewString,
    scope: *const HewString,
    state: *const HewString,
) -> *mut HewString {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `auth_url` is a managed handle borrowed for this call.
    let auth_url_str = unsafe { string_as_str(auth_url) };
    // SAFETY: `redirect_uri` is a managed handle borrowed for this call.
    let redirect_uri_str = unsafe { string_as_str(redirect_uri) };
    // SAFETY: `scope` is a managed handle borrowed for this call.
    let scope_str = unsafe { string_as_str(scope) };
    // SAFETY: `state` is a managed handle borrowed for this call.
    let state_str = unsafe { string_as_str(state) };
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
    string_from_str(&url)
}

/// Exchange an authorization code for a token (authorization code grant).
///
/// POSTs to `token_url` with `grant_type=authorization_code`. Returns a
/// heap-allocated [`HewOauthToken`], or null when `client_ptr` is null.
///
/// The caller must free the token with [`hew_oauth_token_free`].
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string argument must be null (the empty string) or a live managed
///   Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_exchange_code(
    client_ptr: *mut HewOauthClient,
    token_url: *const HewString,
    code: *const HewString,
    redirect_uri: *const HewString,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `token_url` is a managed handle borrowed for this call.
    let token_url_str = unsafe { string_as_str(token_url) };
    // SAFETY: `code` is a managed handle borrowed for this call.
    let code_str = unsafe { string_as_str(code) };
    // SAFETY: `redirect_uri` is a managed handle borrowed for this call.
    let redirect_uri_str = unsafe { string_as_str(redirect_uri) };
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
/// - Each string argument must be null (the empty string) or a live managed
///   Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_exchange_code_with_verifier(
    client_ptr: *mut HewOauthClient,
    token_url: *const HewString,
    code: *const HewString,
    redirect_uri: *const HewString,
    code_verifier: *const HewString,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return token_error(0, "null_client");
    }
    // SAFETY: `token_url` is a managed handle borrowed for this call.
    let token_url_str = unsafe { string_as_str(token_url) };
    // SAFETY: `code` is a managed handle borrowed for this call.
    let code_str = unsafe { string_as_str(code) };
    // SAFETY: `redirect_uri` is a managed handle borrowed for this call.
    let redirect_uri_str = unsafe { string_as_str(redirect_uri) };
    // SAFETY: `code_verifier` is a managed handle borrowed for this call.
    let code_verifier_str = unsafe { string_as_str(code_verifier) };
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

/// Return the current generated OAuth state for callback validation as a
/// freshly allocated managed string the caller releases.
///
/// # Safety
///
/// `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_current_state(
    client_ptr: *const HewOauthClient,
) -> *mut HewString {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    string_from_str(&unsafe { &*client_ptr }.pending_state)
}

/// Return the current generated PKCE code verifier for external storage as a
/// freshly allocated managed string the caller releases.
///
/// # Safety
///
/// `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_current_code_verifier(
    client_ptr: *const HewOauthClient,
) -> *mut HewString {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    string_from_str(&unsafe { &*client_ptr }.pending_code_verifier)
}

/// Validate a callback state against the current generated state.
///
/// Returns 1 on match and 0 on mismatch or missing state.
///
/// # Safety
///
/// `client_ptr` must be valid. `callback_state` must be null (the empty
/// string) or a live managed Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_validate_state(
    client_ptr: *const HewOauthClient,
    callback_state: *const HewString,
) -> i32 {
    if client_ptr.is_null() {
        return 0;
    }
    // SAFETY: `callback_state` is a managed handle borrowed for this call.
    let callback_state_str = unsafe { string_as_str(callback_state) };
    // SAFETY: client_ptr is a valid HewOauthClient pointer per caller contract.
    let client = unsafe { &*client_ptr };
    i32::from(!client.pending_state.is_empty() && client.pending_state == callback_state_str)
}

/// Refresh an access token using a refresh token.
///
/// POSTs to `token_url` with `grant_type=refresh_token`. Returns a
/// heap-allocated [`HewOauthToken`], or null when `client_ptr` is null.
///
/// The caller must free the token with [`hew_oauth_token_free`].
///
/// # Safety
///
/// - `client_ptr` must be a valid pointer returned by [`hew_oauth_new`].
/// - Each string argument must be null (the empty string) or a live managed
///   Hew string handle.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_refresh(
    client_ptr: *mut HewOauthClient,
    token_url: *const HewString,
    refresh_token: *const HewString,
) -> *mut HewOauthToken {
    if client_ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `token_url` is a managed handle borrowed for this call.
    let token_url_str = unsafe { string_as_str(token_url) };
    // SAFETY: `refresh_token` is a managed handle borrowed for this call.
    let refresh_token_str = unsafe { string_as_str(refresh_token) };
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

/// Return the access token as a freshly allocated managed string the caller
/// releases.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_access_token(
    token: *const HewOauthToken,
) -> *mut HewString {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    string_from_str(&unsafe { &*token }.access_token)
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

/// Return an error diagnostic for failed token responses as a freshly
/// allocated managed string the caller releases.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_error_message(
    token: *const HewOauthToken,
) -> *mut HewString {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    string_from_str(&unsafe { &*token }.error_message)
}

/// Return the token type as a freshly allocated managed string the caller
/// releases (usually "Bearer").
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_type(token: *const HewOauthToken) -> *mut HewString {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    string_from_str(&unsafe { &*token }.token_type)
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

/// Return the refresh token as a freshly allocated managed string the caller
/// releases, or the empty string when the endpoint provided none.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_refresh_token(
    token: *const HewOauthToken,
) -> *mut HewString {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    string_from_str(&unsafe { &*token }.refresh_token)
}

/// Return the token scope as a freshly allocated managed string the caller
/// releases, or the empty string when the endpoint provided none.
///
/// # Safety
///
/// `token` must be a valid pointer returned by a token-producing function.
#[no_mangle]
pub unsafe extern "C" fn hew_oauth_token_scope(token: *const HewOauthToken) -> *mut HewString {
    if token.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: token is a valid HewOauthToken pointer per caller contract.
    string_from_str(&unsafe { &*token }.scope)
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Allocate one managed string argument for a boundary call.
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

        // SAFETY: ptr is a valid HewOauthToken we just created, and every
        // returned managed string is read and released exactly once.
        unsafe {
            assert_eq!(owned_text(hew_oauth_token_access_token(ptr)), "mytoken");
            assert_eq!(owned_text(hew_oauth_token_type(ptr)), "Bearer");
            assert_eq!(hew_oauth_token_expires_in(ptr), 7200);
            assert_eq!(
                owned_text(hew_oauth_token_refresh_token(ptr)),
                "refresh_abc"
            );
            assert_eq!(owned_text(hew_oauth_token_scope(ptr)), "read write");
            hew_oauth_token_free(ptr);
        }
    }

    #[test]
    fn test_null_token_accessors() {
        // SAFETY: passing null pointers — the accessors answer with the empty
        // managed string and must not panic.
        unsafe {
            assert!(owned_text(hew_oauth_token_access_token(std::ptr::null())).is_empty());
            assert_eq!(hew_oauth_token_is_ok(std::ptr::null()), 0);
            assert_eq!(hew_oauth_token_error_status(std::ptr::null()), -1);
            assert!(owned_text(hew_oauth_token_error_message(std::ptr::null())).is_empty());
            assert!(owned_text(hew_oauth_token_type(std::ptr::null())).is_empty());
            assert_eq!(hew_oauth_token_expires_in(std::ptr::null()), -1);
            assert!(owned_text(hew_oauth_token_refresh_token(std::ptr::null())).is_empty());
            assert!(owned_text(hew_oauth_token_scope(std::ptr::null())).is_empty());
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
        let client_id = managed("left\0right");
        let secret = managed("secret");
        let auth_url = managed("https://example.test/authorize");
        let redirect = managed("https://client.test/callback");
        let scope = managed("openid");
        let state = managed("known");
        // SAFETY: every handle is a live managed string this test owns and
        // releases; a managed string carries an exact length, so the embedded
        // NUL reaches the encoder intact.
        unsafe {
            let client = hew_oauth_new(client_id, secret);
            assert!(!client.is_null());
            let value = owned_text(hew_oauth_auth_url(client, auth_url, redirect, scope, state));
            assert!(value.contains("client_id=left%00right"));
            hew_oauth_close(client);
            string_release(client_id);
            string_release(secret);
            string_release(auth_url);
            string_release(redirect);
            string_release(scope);
            string_release(state);
        }
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
        let client_id = managed("client ü");
        let client_secret = managed("secret");
        let auth_url = managed("https://auth.example/authorize");
        let redirect_uri = managed("https://app.example/callback");
        let scope = managed("openid profile");
        // The empty string is the canonical null handle; it asks for a
        // generated state.
        let empty_state = managed("");

        // SAFETY: every handle is a live managed string this test owns and
        // releases; returned owners are read and released exactly once.
        unsafe {
            let client = hew_oauth_new(client_id, client_secret);
            assert!(!client.is_null());

            let url = owned_text(hew_oauth_auth_url(
                client,
                auth_url,
                redirect_uri,
                scope,
                empty_state,
            ));
            let state = owned_text(hew_oauth_current_state(client));
            let verifier = owned_text(hew_oauth_current_code_verifier(client));
            let expected_challenge = code_challenge_for_verifier(&verifier);

            assert_eq!(state.len(), 43);
            assert_eq!(verifier.len(), 43);
            let state_handle = managed(&state);
            assert_eq!(hew_oauth_validate_state(client, state_handle), 1);
            string_release(state_handle);
            let wrong_state = managed("wrong");
            assert_eq!(hew_oauth_validate_state(client, wrong_state), 0);
            string_release(wrong_state);
            assert!(url.contains("response_type=code"));
            assert!(url.contains("client_id=client%20%C3%BC"));
            assert!(url.contains("redirect_uri=https%3A%2F%2Fapp.example%2Fcallback"));
            assert!(url.contains("scope=openid%20profile"));
            assert!(url.contains(&format!("state={state}")));
            assert!(url.contains(&format!("code_challenge={expected_challenge}")));
            assert!(url.contains("code_challenge_method=S256"));

            hew_oauth_close(client);
            string_release(client_id);
            string_release(client_secret);
            string_release(auth_url);
            string_release(redirect_uri);
            string_release(scope);
            string_release(empty_state);
        }
    }

    #[test]
    fn test_exchange_code_posts_pkce_verifier() {
        let (url, handle) = serve_once(
            200,
            r#"{"access_token":"access","token_type":"Bearer","expires_in":60}"#,
        );
        let client_id = managed("client");
        let client_secret = managed("secret");
        let auth_url = managed("https://auth.example/authorize");
        let redirect_uri = managed("https://app.example/callback");
        let scope = managed("");
        let state = managed("caller-state");
        let token_url = managed(&url);
        let code = managed("code 123");

        // SAFETY: every handle is a live managed string this test owns and
        // releases; returned owners are read and released exactly once.
        unsafe {
            let client = hew_oauth_new(client_id, client_secret);
            string_release(hew_oauth_auth_url(
                client,
                auth_url,
                redirect_uri,
                scope,
                state,
            ));
            let verifier = owned_text(hew_oauth_current_code_verifier(client));

            let token = hew_oauth_exchange_code(client, token_url, code, redirect_uri);
            assert_eq!(hew_oauth_token_is_ok(token), 1);
            hew_oauth_token_free(token);
            hew_oauth_close(client);
            string_release(client_id);
            string_release(client_secret);
            string_release(auth_url);
            string_release(redirect_uri);
            string_release(scope);
            string_release(state);
            string_release(token_url);
            string_release(code);

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
        let client_id = managed("client");
        let client_secret = managed("secret");
        let token_url = managed(&url);
        let scope = managed("read ü");

        // SAFETY: every handle is a live managed string this test owns and
        // releases; returned owners are read and released exactly once.
        unsafe {
            let client = hew_oauth_new(client_id, client_secret);
            let token = hew_oauth_client_credentials(client, token_url, scope);
            assert!(!token.is_null());
            assert_eq!(hew_oauth_token_is_ok(token), 0);
            assert_eq!(hew_oauth_token_error_status(token), 400);
            assert_eq!(
                owned_text(hew_oauth_token_error_message(token)),
                "invalid_client: bad credentials"
            );
            hew_oauth_token_free(token);
            hew_oauth_close(client);
            string_release(client_id);
            string_release(client_secret);
            string_release(token_url);
            string_release(scope);

            let form = handle.join().unwrap();
            assert!(form.contains("grant_type=client_credentials"));
            assert!(form.contains("scope=read%20%C3%BC"));
        }

        let token = parse_token_body(200, "not-json");
        // SAFETY: token was returned by parse_token_body above and is freed exactly once here.
        unsafe {
            assert_eq!(hew_oauth_token_is_ok(token), 0);
            assert!(owned_text(hew_oauth_token_error_message(token)).starts_with("invalid_json"));
            hew_oauth_token_free(token);
        }
    }

    #[test]
    fn test_refresh_posts_form_params() {
        let (url, handle) = serve_once(
            200,
            r#"{"access_token":"new-access","token_type":"Bearer","refresh_token":"next"}"#,
        );
        let client_id = managed("client");
        let client_secret = managed("secret");
        let token_url = managed(&url);
        let refresh_token = managed("refresh token");

        // SAFETY: every handle is a live managed string this test owns and
        // releases; returned owners are read and released exactly once.
        unsafe {
            let client = hew_oauth_new(client_id, client_secret);
            let token = hew_oauth_refresh(client, token_url, refresh_token);
            assert_eq!(hew_oauth_token_is_ok(token), 1);
            assert_eq!(
                owned_text(hew_oauth_token_access_token(token)),
                "new-access"
            );
            hew_oauth_token_free(token);
            hew_oauth_close(client);
            string_release(client_id);
            string_release(client_secret);
            string_release(token_url);
            string_release(refresh_token);

            let form = handle.join().unwrap();
            assert!(form.contains("grant_type=refresh_token"));
            assert!(form.contains("refresh_token=refresh%20token"));
            assert!(form.contains("client_id=client"));
            assert!(form.contains("client_secret=secret"));
        }
    }

    #[test]
    fn test_exchange_without_pkce_returns_error_token() {
        let client_id = managed("client");
        let client_secret = managed("secret");
        let token_url = managed("http://127.0.0.1:9");
        let code = managed("code");
        let redirect_uri = managed("https://app.example/callback");

        // SAFETY: every handle is a live managed string this test owns and
        // releases; returned owners are read and released exactly once.
        unsafe {
            let client = hew_oauth_new(client_id, client_secret);
            let token = hew_oauth_exchange_code(client, token_url, code, redirect_uri);
            assert_eq!(hew_oauth_token_is_ok(token), 0);
            assert_eq!(
                owned_text(hew_oauth_token_error_message(token)),
                "missing_code_verifier: call auth_url before exchange_code or use exchange_code_with_verifier"
            );
            hew_oauth_token_free(token);
            hew_oauth_close(client);
            string_release(client_id);
            string_release(client_secret);
            string_release(token_url);
            string_release(code);
            string_release(redirect_uri);
        }
    }
}
