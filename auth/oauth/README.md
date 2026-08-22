# hew.auth.oauth

An actor-owned OAuth 2.0 client supporting client credentials, authorization
code with PKCE S256, callback-state validation, refresh, and typed endpoint
errors. Tokens expose optional expiry, refresh-token, and scope values without
sentinels and must be freed after use.

```hew
import hew.auth.oauth;

fn main() {
    let client = spawn oauth.Client(client_id: "demo", client_secret: "secret");
    match await client.auth_url("https://authorization.example/authorize", "https://app.example/callback", "profile", "") {
        .Ok(result) => match result {
            .Ok(url) => println(f"send the user to {url}"),
            .Err(error) => println(oauth.error_message(error)),
        },
        .Err(_) => println("OAuth actor stopped before replying"),
    }
    let _ = client.close();
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
