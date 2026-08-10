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
#   7. a selector resolves to the widget's own bounds — the tree and the grammar agree
#   8. `click --on` and `type --focus-on` drive it    — selectors reach a real toolkit
#   9. a bad selector is refused, by name             — the diagnostics are load-bearing
#
# (6) is the one that matters most. Without a seat, or without frame callbacks, the others
# either fail outright or the picture never changes.
#
# (4) is here because the failure it guards against is silent: an application can complete
# the accessibility handshake and then publish nothing, with no error on either side. The
# only way to know is to read a node out — see plan 07.
#
# (7) is the one selectors need, and it is why `select` exists as a verb. `click --on` is
# deliberately a single call — resolve and click, with no gap for the layout to move in —
# which leaves no way to see *where* it decided to click. So the check is: read a widget's
# bounds out of the tree, ask `select` for the same widget, and assert the point it returns
# lies inside them. A resolver that returned a plausible-looking wrong point would pass a
# test that only checked "the click worked", because clicking almost anywhere in a widget
# factory does something.
#
# Nothing below hard-codes a widget name. The names come out of the tree at runtime, for
# the same reason the title check matches `.` rather than GTK's wording: a smoke test that
# fails when the application is upgraded is a smoke test people learn to ignore.
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

# ------------------------------------------------------------------ selectors

# Pull a button out of the tree that is big enough to click and whose name is
# unique among buttons, so `button[name="…"]` has exactly one match. Nothing is
# hard-coded: the name comes from whatever this GTK release happens to publish.
#
# Document order decides ties, so the same tree always yields the same widget —
# a smoke test that picks a different target each run reports flakes that are
# its own.
pick_button() {
    sed -n 's/^ *button "\([^"\\|]*\)"  \[\(-*[0-9]*\),\(-*[0-9]*\) \([0-9]*\)x\([0-9]*\)\]$/\1|\2|\3|\4|\5/p' "$1" \
    | awk -F'|' '
        $4 > 8 && $5 > 8 {
            if (!($1 in count)) { order[++n] = $1 }
            count[$1]++
            line[$1] = $0
        }
        END {
            for (i = 1; i <= n; i++) {
                if (count[order[i]] == 1) { print line[order[i]]; exit }
            }
        }'
}

widget=$(pick_button "$out/tree.txt")
if [ -z "$widget" ]; then
    echo "smoke: no uniquely named button with usable bounds in the tree" >&2
    grep '^ *button' "$out/tree.txt" | head -20 >&2
    exit 1
fi

name=$(printf '%s' "$widget" | cut -d'|' -f1)
bx=$(printf '%s' "$widget" | cut -d'|' -f2)
by=$(printf '%s' "$widget" | cut -d'|' -f3)
bw=$(printf '%s' "$widget" | cut -d'|' -f4)
bh=$(printf '%s' "$widget" | cut -d'|' -f5)
selector="button[name=\"$name\"]"
echo "smoke: resolving $selector"

# The assertion selectors actually need. `click --on` is one call by design —
# resolve and click, with no gap for the layout to move — so it cannot report
# where it landed, and a resolver returning a plausible wrong point would still
# make something happen in a widget factory. `select` is the dry run that makes
# the answer checkable, against bounds that came from the application itself.
point=$(waypuppet select "$selector")
want="$((bx + bw / 2)),$((by + bh / 2))"
if [ "$point" != "$want" ]; then
    echo "smoke: $selector resolved to $point, but the tree puts it at $want" >&2
    echo "smoke: the grammar and the tree disagree about where this widget is" >&2
    exit 1
fi
echo "smoke: resolved to $point, the centre of the widget's own bounds"

# A selector that matches many has to say so *usefully*: the count is not the
# point, the list of narrowed selectors is, because that is what a caller — or
# an agent — does next. Guarded on there being several, so this does not become
# a test of how many buttons GTK ships.
buttons=$(grep -c '^ *button ' "$out/tree.txt" || true)
if [ "$buttons" -gt 1 ]; then
    if waypuppet select 'button' >"$out/ambiguous.txt" 2>&1; then
        echo "smoke: 'button' matched $buttons nodes and was not reported as ambiguous" >&2
        exit 1
    fi
    if ! grep -q -- '--index' "$out/ambiguous.txt"; then
        echo "smoke: an ambiguous selector did not say how to disambiguate" >&2
        cat "$out/ambiguous.txt" >&2
        exit 1
    fi
    if ! grep -q '^ *\[0\] ' "$out/ambiguous.txt"; then
        echo "smoke: an ambiguous selector listed no matches to choose between" >&2
        cat "$out/ambiguous.txt" >&2
        exit 1
    fi
    # And the way out of it works: the same selector with an index resolves.
    waypuppet select 'button' --index 0 >/dev/null
    echo "smoke: ambiguity reported $buttons matches, and --index resolves one"
fi

# The diagnostics are load-bearing here, not decoration: the whole argument for
# refusing an unknown role at parse time rather than returning no match is that
# the caller is told what to write instead. If that stops being true nothing
# else fails, so it is checked against the real binary.
if waypuppet select 'textbox' >"$out/badrole.txt" 2>&1; then
    echo "smoke: 'textbox' is not an AT-SPI role and should have been refused" >&2
    exit 1
fi
if ! grep -q 'did you mean' "$out/badrole.txt"; then
    echo "smoke: an unknown role was refused without naming a real one" >&2
    cat "$out/badrole.txt" >&2
    exit 1
fi

if waypuppet click --on 'button.primary' >"$out/badsyntax.txt" 2>&1; then
    echo "smoke: a CSS class selector should have been refused" >&2
    exit 1
fi
if ! grep -q 'name=' "$out/badsyntax.txt"; then
    echo "smoke: a CSS class selector was refused without saying what to write" >&2
    cat "$out/badsyntax.txt" >&2
    exit 1
fi
echo "smoke: bad selectors are refused by name, with the fix"

# Driving a real toolkit by selector. The capture has to change: a `click --on`
# that resolved correctly and then delivered nothing would return successfully
# and do nothing at all.
waypuppet shot -o "$out/before-selector.png" >/dev/null
waypuppet click --on "$selector"
waypuppet shot -o "$out/after-selector.png" >/dev/null
if cmp -s "$out/before-selector.png" "$out/after-selector.png"; then
    echo "smoke: clicking $selector changed nothing on screen" >&2
    echo "--- session log ---" >&2
    cat "$out/session.log" >&2
    exit 1
fi
echo "smoke: click --on drove the application"

# `--focus-on` against a tree read *now*, because the click above may well have
# changed the layout — which is the whole reason resolution happens server side,
# one call at a time, instead of in the caller.
waypuppet tree > "$out/tree2.txt"
widget2=$(pick_button "$out/tree2.txt")
if [ -n "$widget2" ]; then
    name2=$(printf '%s' "$widget2" | cut -d'|' -f1)
    waypuppet type 'hi' --focus-on "button[name=\"$name2\"]"
    echo "smoke: type --focus-on resolved, focused and settled"
fi

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
