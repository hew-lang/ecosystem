# Native release cohorts

Native attachments are qualified for one Hew SDK version, Rust toolchain and
Linux host. Source package versions remain `0.4.0`; each new compiler release
gets its own native attachment identity.

After the next official SDK is published, run the existing **Publish packages**
workflow on `main`, setting:

- `native_packages`: `all`, or package paths such as `net/http db/postgres`.
- `native_hosts`: `both`, `x86_64`, or `aarch64`.
- `native_sdk_version`: the exact official release, for example `0.6.4`.

The native jobs use `ubuntu-24.04` and `ubuntu-24.04-arm`. They download the
matching official SDK and verify its release checksum and reported version.
The repository's pinned Rust toolchain must match the SDK runtime; `hew native
build` checks this before compilation. The SDK supplies the publisher CLI.

For each selected package the batch first checks the local source against its
published version. A fresh consumer install then verifies any existing native
attachment through Hew's ordinary signature, source and native-cache checks.
Verified attachments are recorded and skipped. Missing attachments go through
`hew native build` and `hew publish --auth github-actions --native-archive` in
the same trusted workflow. An unreadable or invalid attachment stops the batch.
Retries use this same path and verify earlier successful uploads before building.

Each host uploads its native archives and `receipt.json`, containing the SDK
archive checksum and each verified, built or published disposition. Existing
source publication and explicit publisher CLI overrides remain available when
`native_packages` is empty.

## Package dispositions

Both Linux hosts have an executable build path for every native manifest:

| Package path | Build and consumer prerequisites |
| --- | --- |
| `db/sqlite` | SQLite is bundled in the Rust archive. The existing 0.6.3 x64 attachment is qualified. |
| `db/postgres` | Native Rust client; PostgreSQL service needed for functional tests. |
| `db/mysql` | Rust client with rustls; MySQL service needed for functional tests. |
| `db/redis` | Native Rust client; Redis service needed for functional tests. |
| `db/mongodb` | Rust client with rustls; MongoDB service needed for functional tests. |
| `net/http` | Native HTTP parser/server adapter. |
| `queue/mqtt` | Plain TCP client; MQTT broker needed for functional tests. |
| `queue/nats` | Native client; NATS service needed for functional tests. |
| `storage/s3` | OpenSSL development files for building and matching runtime libraries for consumers. |
| `metrics` | Native Prometheus adapter. |
| `image/magick` | ImageMagick 7 MagickWand, delegates and development headers; matching runtime libraries remain required by consumers. |

The existing pinned ImageMagick 7 dependency recipe is shared with package CI.
On disposable publishing runners it runs only when a missing selected package
needs a build. Installing an attachment supplies its Rust archive; external
system libraries and service availability remain ordinary package prerequisites.
Host builds and archive publication establish native availability. Actual
consumer execution with the next SDK is a separate qualification result.

## Local preparation

```sh
make native-plan NATIVE_COHORT=all NATIVE_HOST=both
make test-native-release
make native-build NATIVE_COHORT=net/http NATIVE_HOST=x86_64 \
  NATIVE_SDK_ROOT=/path/to/official/sdk NATIVE_SDK_VERSION=0.6.4
```

`native-build` builds missing archives locally. `native-publish` additionally
uses the existing GitHub Actions trusted publishing path. The workflow enables
system-dependency installation on its disposable runners; local builds expect
their package prerequisites to be installed already.
