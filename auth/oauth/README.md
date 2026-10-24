# hew.auth.oauth

An actor-owned OAuth 2.0 client supporting client credentials, authorization
code with PKCE S256, callback-state validation, refresh, and typed endpoint
errors. Tokens expose optional expiry, refresh-token, and scope values as
`Option`s.

The package is pure Hew: token requests are form-encoded POSTs sent with
`std.net.http.client`, PKCE and state values come from
`std.crypto.hash`, and token responses are read with `std.encoding.json`.
It has no native library to build.

```hew
import hew.auth.oauth;

fn main() {
    let client = spawn oauth.Client { client_id: "demo", client_secret: "secret" };
    match client.auth_url("https://authorization.example/authorize", "https://app.example/callback", "profile", "") {
        .Ok(url) => println(f"send the user to {url}"),
        .Err(.Failed(error)) => println(oauth.error_message(error)),
        .Err(_) => println("OAuth actor stopped before replying"),
    }
    stop(client);
    stopped(client);
}
```

`auth_url` always adds PKCE S256 parameters, generates the `state` when you
pass an empty one, and retains both the state and the code verifier on the
client — so validating the callback needs no storage of your own.

The checked example at [`examples/basic.hew`](examples/basic.hew) walks that
whole flow and needs no authorization server, because every step up to the code
exchange is local. Run it from the ecosystem checkout:

```sh
hew run --pkg-path . auth/oauth/examples/basic.hew
```

It prints the authorization URL, then accepts the state `auth_url` retained and
rejects an empty callback state as `OAuthError.InvalidInput`.

## Errors

Token requests are `receive fn`s declared `-> Token fails OAuthError`, so a call
replies `Result<Token, ActorError<OAuthError>>`: the handler's error arrives as
`.Err(.Failed(error))`, and any other `.Err` means the actor did not reply.
`auth_url` and `validate_state` fail the same way; `current_state` and
`current_code_verifier` cannot fail and reply `Option<string>`.

- `InvalidInput` — a local precondition failed before any request was sent:
  an empty URL, refresh token, verifier, or callback state, or
  `exchange_code` called before `auth_url` stored a verifier.
- `Transport` — the token endpoint could not be reached; the message starts
  with `transport_error:`.
- `TokenEndpoint(status, message)` — the endpoint answered with a non-2xx
  status, an RFC 6749 error body (`error: error_description`), a body that is
  not JSON (`invalid_json: …`), or no `access_token`.
- `Internal` — the system entropy source could not supply PKCE or state bytes.

## Tests

`tests/public_api.hew` drives every grant against a loopback token endpoint in
the same process; the in-file tests cover the RFC 7636 S256 vector and token
body parsing:

```sh
hew build --pkg-path . auth/oauth/tests/public_api.hew -o oauth-tests && ./oauth-tests
hew test auth/oauth/oauth.hew
```
