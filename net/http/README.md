# hew.net.http

`hew.net.http` is a small HTTP/1.1 server with actor-owned listener and
connection lifecycles plus typed errors. Each `accept_one` call waits for one
request for up to 250 milliseconds; the package does not hide a loop or promise
an unavailable request generator.

```hew
import hew.net.http;

fn main() {
    let server = spawn http.Server(addr: "127.0.0.1:8080");
    var attempts_left = 120;
    var served = false;
    while attempts_left > 0 && !served {
        attempts_left = attempts_left - 1;
        match await server.accept_one() {
            .Ok(result) => match result {
                .Ok(request) => {
                    println(f"{request.method()} {request.path()}");
                    match await server.respond_text(200, "Hello from Hew!\n") {
                        .Ok(response_result) => match response_result {
                            .Ok(_) => {},
                            .Err(error) => println(http.error_message(error)),
                        },
                        .Err(_) => println("HTTP server actor stopped before replying"),
                    }
                    served = true;
                },
                // An idle quarter second is the documented outcome of
                // accept_one(), so it is a retry rather than a failure.
                .Err(.Accept(_)) => {},
                .Err(error) => {
                    println(http.error_message(error));
                    served = true;
                },
            },
            .Err(_) => {
                println("HTTP server actor stopped before replying");
                served = true;
            },
        }
    }
    server.close();
}
```

The retry loop is not ceremony: without it the 250-millisecond timeout fires
before anyone can send a request, so the only outcome a reader could ever
observe is the `Accept` error.

The checked example at [`examples/hello.hew`](examples/hello.hew) is this
program with the listening banner and a message for the case where the thirty
seconds elapse unused. Run it from the ecosystem checkout:

```sh
hew run --pkg-path . net/http/examples/hello.hew
```

then, from a second terminal within those thirty seconds:

```sh
curl http://127.0.0.1:8080/hello
```

The server prints `GET /hello` and exits; `curl` prints `Hello from Hew!`. With
no request at all it prints `no request arrived within 30 seconds` and still
exits successfully.

All operations that can fail return `Result<_, HttpError>`. The actor retains
each accepted connection until a response method or `close()` releases it.
It also releases listener and request handles from its stop hook, including
failure and caller-abandonment paths.

Only one request may be pending in the actor at a time. Call one of the
response methods or `server.close()` before accepting another request. An idle
`accept_one()` returns an `Accept` error after 250 milliseconds, which keeps a
queued close or actor stop from being trapped behind a blocking socket call;
servers normally retry `accept_one()` after that timeout.

Header and response methods return `HttpError.NoPending` when the server is
open but no request is awaiting a response. `HttpError.Closed` is reserved for
operations attempted after `server.close()`.

`await server.header(name)` performs a case-insensitive lookup on the pending
request and returns `Err(HttpError.MissingHeader(name))` when the header is
absent. An empty header value is therefore distinct from a missing header.
Incoming HTTP/1.1 requests must contain exactly one valid `Host` header.
Response methods accept final status codes from 200 through 599; informational
responses are rejected because this one-response API cannot send the required
later final response.

Response bodies cross the native ABI with an explicit byte length, so embedded
NUL bytes are transmitted without truncation. HTTP header values containing NUL
are rejected, as are request bodies containing NUL because the current
native-to-Hew string return boundary cannot represent them losslessly.

`await server.url_decode(text)` decodes percent escapes as UTF-8 and converts
`+` to a space. Malformed escapes and decoded bytes that are not UTF-8 return
`HttpError.Decode`. `await server.form_value(body, key)` applies the same
strict decoding to URL-encoded form fields; a missing key returns
`HttpError.MissingFormField` rather than an empty-string sentinel.
