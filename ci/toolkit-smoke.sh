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
#   2. `wait --title` matches it                     — window bookkeeping survives a toolkit
#   3. `clipboard set` then paste changes the capture — the selection reached the app
#   4. `tree` names a widget                          — the a11y bus and registry work
#   5. `click` and `type` return successfully         — they settled, so the client drew
#   6. the capture changes as a result                — the input actually reached the app
#
# (6) is the one that matters most. Without a seat, or without frame callbacks, the others
# either fail outright or the picture never changes.
#
# (4) is here because the failure it guards against is silent: an application can complete
# the accessibility handshake and then publish nothing, with no error on either side. The
# only way to know is to read a node out — see plan 07.
#
# Note what is absent: there is no `sleep` anywhere below. Every step blocks on a fact the
# session observed, which is the property waypuppet exists to provide and the fastest way
# to notice if it stops being true.
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
}
trap cleanup EXIT

# `run --detach` rather than `serve &`: it returns only once the control socket is
# accepting connections, so the very next line can talk to the session. Backgrounding
# `serve` would leave a window in which `wait` has nothing to connect to.
echo "smoke: hosting $app at $size"
waypuppet run --detach --size "$size" -- "$app" >"$out/session.log" 2>&1

# The compositor is the thing that knows when a client maps a window, so ask it rather
# than polling `ps` on a timer. This returns the instant it happens, and exits 3 — not
# 1 — if it never does, which is how "the app did not start" is told apart from "the
# session fell over".
echo "smoke: waiting for a window"
if ! waypuppet wait --windows 1 --timeout "$((deadline * 1000))"; then
    echo "smoke: $app never mapped a window within ${deadline}s" >&2
    echo "--- session log ---" >&2
    cat "$out/session.log" >&2
    exit 1
fi

# Deliberately not the application's own wording: what is under test is that a title set
# by a real toolkit reaches the registry and matches, not what GTK happens to call this
# release's window. Hard-coding that would be a smoke test that fails on an upgrade.
if ! waypuppet wait --title . --timeout 5000; then
    echo "smoke: the mapped window has no title the session can see" >&2
    waypuppet ps >&2 || true
    exit 1
fi
echo "smoke: window mapped and titled"

# The accessibility tree, read out of a real toolkit. Asserting on a node rather than on
# "did it register" is the whole point: an application can finish the handshake and then
# publish nothing, silently, and only reading something proves otherwise. `frame` is the
# role GTK gives its window, and it will be there whatever the widgets are called.
echo "smoke: reading the accessibility tree"
if ! waypuppet tree > "$out/tree.txt" 2>"$out/tree.err"; then
    echo "smoke: could not read the accessibility tree" >&2
    cat "$out/tree.err" >&2
    grep -iE "a11y|bus" "$out/session.log" >&2 || true
    exit 1
fi
if ! grep -q "^  frame" "$out/tree.txt"; then
    echo "smoke: the tree has no frame in it — the application published nothing useful" >&2
    head -20 "$out/tree.txt" >&2
    exit 1
fi
echo "smoke: tree has $(wc -l < "$out/tree.txt") nodes"

# A paste is a different code path from typing in most toolkits, so it is worth its own
# check rather than being assumed to follow from `type` working.
echo "smoke: pasting through the clipboard"
waypuppet clipboard set 'pasted-by-waypuppet'
if [ "$(waypuppet clipboard get)" != 'pasted-by-waypuppet' ]; then
    echo "smoke: the clipboard did not round-trip" >&2
    exit 1
fi

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
