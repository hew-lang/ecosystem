#!/usr/bin/env bash
#
# The corpus gate: every Hew source checked, every suite run, every example
# actually executed. CI calls this script and docs/toolchain.md points at it,
# so "what CI runs" and "what to run before opening a pull request" cannot
# drift apart into two lists.
#
# Usage: [HEW=<compiler>] scripts/corpus-gate.sh [check|hew|native|all]
#
#   check   type-check every tracked .hew file
#   hew     the suites and examples that need no ImageMagick and no S3
#   native  the suites and examples that need ImageMagick or S3
#   all     all three, in that order (the default)
#
# Services come from docker-compose.yml; `docker compose up -d` starts them.
# `hew` and `native` need those services running. ImageMagick 7 development
# files and pkg-config are the `native` stage's system dependency.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

HEW="${HEW:-hew}"
stage="${1:-all}"

# Every file is checked on its own, so `dead_code` would fire on every exported
# function whose only callers live in a sibling example, test, or downstream
# program. Hew has no library manifest to mark a public surface as used by
# design, so the lint is switched off for this sweep rather than suppressed one
# function at a time. Every other lint stays on, and this sweep must stay
# warning-free.
run_check() {
    echo "== check: every tracked Hew source"
    # The expected count is asserted by the contract script and read from it
    # here, so the number is written down once.
    local expected actual
    expected="$(python3 scripts/verify_package_contract.py --source-count)"
    actual="$(git ls-files '*.hew' | wc -l | tr -d ' ')"
    if [[ "$actual" != "$expected" ]]; then
        echo "tracked .hew count is $actual, expected $expected;" \
            "add the new file to this gate deliberately" >&2
        exit 1
    fi
    while IFS= read -r file; do
        "$HEW" check -A dead_code --pkg-path "$repo_root" "$repo_root/$file"
    done < <(git ls-files '*.hew')
}

run_program() {
    local seconds="$1"
    shift
    timeout "${seconds}s" "$HEW" run --pkg-path "$repo_root" "$@"
}

# Serve one request through the HTTP example. The example exits after the first
# request, so the request has to come from somewhere: sending it here is what
# proves the happy path. Without it the gate would only ever observe the
# accept timeout, which is exactly how the example's silence went unnoticed.
run_http_example() {
    echo "== run: net/http/examples/hello.hew (with a real request)"
    local log body pid
    log="$(mktemp)"
    body="$(mktemp)"
    run_program 60 "$repo_root/net/http/examples/hello.hew" >"$log" &
    pid=$!
    for _ in {1..40}; do
        if curl --silent --fail --max-time 2 \
            http://127.0.0.1:8080/hello >"$body"; then
            break
        fi
        sleep 1
    done
    wait "$pid"
    grep --quiet '^Hello from Hew!$' "$body"
    grep --quiet '^GET /hello$' "$log"
    rm -f "$log" "$body"
}

run_hew() {
    echo "== test: pure-Hew suites"
    "$HEW" test dag/dag.hew
    "$HEW" test math/stats/stats.hew
    "$HEW" test template/template.hew

    echo "== run: net/http server suite"
    (
        cd net/http
        timeout 30s "$HEW" run --pkg-path "$repo_root" tests/public_server.hew
    )

    echo "== run: suites and examples needing no service"
    local program
    for program in \
        auth/oauth/tests/public_api.hew \
        auth/oauth/examples/basic.hew \
        metrics/tests/public_api.hew \
        math/stats/examples/basic.hew \
        dag/examples/linear_pipeline.hew \
        dag/examples/actor_pipeline.hew \
        dag/examples/cyclic_rejected.hew; do
        run_program 120 "$repo_root/$program"
    done

    # An example that prints nothing on success cannot be told apart from an
    # example that silently did nothing, so this one is checked on its output.
    echo "== run: metrics/examples/basic.hew"
    run_program 60 "$repo_root/metrics/examples/basic.hew" \
        | grep --quiet '^requests_total 3$'

    run_http_example

    echo "== run: service-backed suites and examples"
    for program in \
        db/mongodb/tests/public_api.hew \
        db/mongodb/tests/integration.hew \
        db/mongodb/examples/basic.hew \
        db/mysql/tests/public_api.hew \
        db/mysql/tests/integration.hew \
        db/mysql/examples/basic.hew \
        db/postgres/tests/public_api.hew \
        db/postgres/tests/integration.hew \
        db/postgres/examples/basic.hew \
        db/redis/tests/public_api.hew \
        db/redis/tests/integration.hew \
        db/redis/examples/basic.hew \
        db/sqlite/tests/public_api.hew \
        db/sqlite/tests/integration.hew \
        db/sqlite/examples/basic.hew \
        queue/mqtt/tests/public_api.hew \
        queue/mqtt/tests/integration.hew \
        queue/mqtt/examples/pubsub.hew \
        queue/nats/tests/public_api.hew \
        queue/nats/tests/integration.hew \
        queue/nats/examples/pubsub.hew; do
        run_program 300 "$repo_root/$program"
    done

    echo "== run: template/examples/render_page.hew"
    run_program 30 "$repo_root/template/examples/render_page.hew" \
        | diff -u "$repo_root/template/examples/render_page.expected" -
}

# magick_rust's link directives do not survive the trip through this package's
# staticlib, and a hew.toml [native] section has no way to name a system
# library the current compiler acts on, so MagickWand is named on the link
# line. image/magick/README.md documents the same command for consumers.
run_native() {
    echo "== run: image/magick suites (with MagickWand link flags)"
    pkg-config --exists MagickWand || {
        echo "ImageMagick 7 development files not found;" \
            "see image/magick/README.md" >&2
        exit 1
    }
    local link_args=()
    local library program
    for library in $(pkg-config --libs MagickWand); do
        link_args+=(--link-lib "$library")
    done
    for program in \
        image/magick/tests/public_api.hew \
        image/magick/examples/basic.hew; do
        timeout 120s "$HEW" run --pkg-path "$repo_root" \
            "${link_args[@]}" "$repo_root/$program"
    done

    echo "== run: storage/s3 suites and example"
    for program in \
        storage/s3/tests/public_api.hew \
        storage/s3/tests/integration.hew \
        storage/s3/examples/basic.hew; do
        run_program 120 "$repo_root/$program"
    done
}

case "$stage" in
check) run_check ;;
hew) run_hew ;;
native) run_native ;;
all)
    run_check
    run_hew
    run_native
    ;;
*)
    echo "usage: [HEW=<compiler>] $0 [check|hew|native|all]" >&2
    exit 2
    ;;
esac
