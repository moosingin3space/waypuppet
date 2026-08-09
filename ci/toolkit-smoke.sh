#!/bin/sh
# Drive a real toolkit under waypuppet and check that driving it does something.
#
# The unit suite covers the protocol with a `wl_shm` fixture, and that fixture is better
# behaved than any real client: it never waits on a frame callback, and it does not need a
# seat to exist before it can be sent input. Both of the bugs that stopped GTK working got
# past the whole suite for exactly that reason. This is the job that would have caught
# them, so it is worth its runtime.
#
# What it asserts, in order:
#
#   1. a real GTK 3 application maps a window        — the compositor is a usable host
#   2. `click` and `type` return successfully        — they settled, so the client drew
#   3. the capture changes as a result               — the input actually reached the app
#
# (3) is the one that matters. Without a seat, or without frame callbacks, the first two
# either fail outright or the picture never changes.
set -eu

session=toolkit-smoke
size=${WAYPUPPET_SMOKE_SIZE:-1600x900}
out=${WAYPUPPET_SMOKE_OUT:-ci/.cache/smoke}
app=${WAYPUPPET_SMOKE_APP:-gtk3-widget-factory}

# How long to wait for the application to map a window. Generous because llvmpipe is
# several times slower than a GPU and this is the one place a real toolkit starts up.
deadline=${WAYPUPPET_SMOKE_TIMEOUT:-60}

mkdir -p "$out"

# Built once, then invoked directly. `cargo run` would work, but several of the calls
# below overlap with a session running in the background, and two cargo invocations
# contend for the same build lock — a hang that would read as waypuppet not answering.
cargo build --quiet -p waypuppet
bin="${CARGO_TARGET_DIR:-target}/debug/waypuppet"

waypuppet() { "$bin" --session "$session" "$@"; }

cleanup() {
    waypuppet kill >/dev/null 2>&1 || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

echo "smoke: hosting $app at $size"
waypuppet serve --size "$size" -- "$app" >"$out/session.log" 2>&1 &

# Polling on a protocol fact the session reports, not a blind sleep. This becomes
# `waypuppet wait --windows 1` once plan 05 lands; until then the loop is here rather than
# a `sleep 5`, because a fixed sleep is either flaky or slow and usually both.
mapped=
i=0
while [ "$i" -lt "$deadline" ]; do
    if waypuppet ps --json 2>/dev/null | grep -q '"mapped":true'; then
        mapped=yes
        break
    fi
    sleep 1
    i=$((i + 1))
done

if [ -z "$mapped" ]; then
    echo "smoke: $app never mapped a window within ${deadline}s" >&2
    echo "--- session log ---" >&2
    cat "$out/session.log" >&2
    exit 1
fi
echo "smoke: window mapped after ${i}s"

waypuppet shot -o "$out/before.png" >/dev/null
echo "smoke: clicking and typing"
waypuppet click --at 200,90
waypuppet type hello
waypuppet shot -o "$out/after.png" >/dev/null

if cmp -s "$out/before.png" "$out/after.png"; then
    echo "smoke: the capture did not change after clicking and typing" >&2
    echo "smoke: input reached the compositor but not the application" >&2
    echo "--- session log ---" >&2
    cat "$out/session.log" >&2
    exit 1
fi

echo "smoke: the capture changed, so the input landed"
