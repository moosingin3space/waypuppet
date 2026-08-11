#!/bin/sh
# Assert what a waypuppet binary needs from the machine it lands on.
#
#   ci/abi-baseline.sh target/release/waypuppet
#
# Two properties, and both are the difference between a binary that runs on a
# stranger's machine and one that does not. Neither shows up in a test suite:
# the binary that fails these still passes everything, on the machine it was
# built on.
#
# 1. **No glibc symbol newer than the baseline.** waypuppet is distributed as a
#    prebuilt file, so the oldest glibc it must run against is a decision, not an
#    accident. Adding one dependency that calls a newer libc function moves it
#    silently, and the symptom is a version-mismatch error on somebody else's
#    machine long after the commit that caused it.
#
# 2. **The NEEDED list has not grown.** This is the load-bearing one. waypuppet
#    dlopens libEGL rather than linking it, which is why it starts at all on a
#    machine with no GPU stack — and starting is what lets `doctor` say "nothing
#    to render with here, use a runtime" instead of failing to load with a
#    linker error nobody can act on. The whole sandbox-autonomy feature rests on
#    that property, and nothing else checks it. One crate that links libgbm at
#    build time takes it away, and every test still passes.
set -eu

binary=${1:-target/release/waypuppet}

# Ubuntu 22.04. Chosen as the floor rather than the newest thing that works:
# it covers Debian 12, CentOS Stream 10 and current Fedora, and the cost of
# building against it is nothing. Raising it is a distribution decision, so it
# is written here rather than inferred from whatever the builder happened to be.
baseline=${WAYPUPPET_GLIBC_BASELINE:-2.35}

# Everything waypuppet may require at load time, and nothing else.
#
# libwayland-server and libxkbcommon are the two real ones: small, pure C, on
# every desktop, and the only libraries the compositor cannot defer. The rest is
# what a glibc program drags in regardless.
#
# This is the *direct* NEEDED list, not what `ldd` prints. `ldd` walks the whole
# transitive closure — libffi shows up there through libwayland-server — and
# those belong to the libraries that asked for them. What matters here is what
# waypuppet itself adds.
expected="ld-linux-x86-64.so.2
libc.so.6
libgcc_s.so.1
libm.so.6
libwayland-server.so.0
libxkbcommon.so.0"

if [ ! -f "$binary" ]; then
    echo "abi-baseline: no binary at $binary" >&2
    exit 1
fi

fail=0

# --- glibc symbol versions -------------------------------------------------

required=$(objdump -T "$binary" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sort -u -V || true)
highest=$(printf '%s\n' "$required" | tail -n 1)
highest=${highest#GLIBC_}

if [ -n "$highest" ]; then
    # `sort -V` puts the larger last. The baseline has to *be* the larger, or the
    # binary wants something newer than the floor allows.
    #
    # Written this way round after the first version took `head -n 1` and asked
    # whether the baseline came first, which is true precisely when the binary is
    # too new — it passed a binary needing 2.39 against a 2.35 floor and said ok.
    # Caught by running it against a binary that had to fail.
    largest=$(printf '%s\n%s\n' "$baseline" "$highest" | sort -V | tail -n 1)
    if [ "$largest" != "$baseline" ]; then
        echo "abi-baseline: needs glibc $highest, baseline is $baseline" >&2
        echo "  symbols required:" >&2
        printf '%s\n' "$required" | sed 's/^/    /' >&2
        echo "  Build in ci/Containerfile.release, or raise WAYPUPPET_GLIBC_BASELINE" >&2
        echo "  deliberately if the floor is meant to move." >&2
        fail=1
    else
        echo "glibc      needs $highest, baseline $baseline"
    fi
fi

# --- load-time dependencies ------------------------------------------------

needed=$(objdump -p "$binary" | awk '/NEEDED/ { print $2 }' | sort -u)
# The interpreter is a load-time dependency too, and is not in NEEDED.
interpreter=$(objdump -p "$binary" | awk '/interpreter/ { sub(/.*\//, "", $NF); sub(/]$/, "", $NF); print $NF }')
observed=$(printf '%s\n%s\n' "$needed" "$interpreter" | grep -v '^$' | sort -u)

unexpected=$(printf '%s\n' "$observed" | grep -vxF "$expected" || true)
if [ -n "$unexpected" ]; then
    echo "abi-baseline: new load-time dependencies:" >&2
    printf '%s\n' "$unexpected" | sed 's/^/    /' >&2
    echo "  waypuppet has to start on a machine with no GPU stack so that \`doctor\`" >&2
    echo "  can say so. Anything linked here must be present everywhere, or it must" >&2
    echo "  be dlopened the way libEGL is. If this one really is everywhere, add it" >&2
    echo "  to \`expected\` in this script and say why." >&2
    fail=1
fi

# A dependency that *went away* is not a failure, but it is a surprise worth
# printing: it usually means a feature was switched off by accident.
missing=$(printf '%s\n' "$expected" | grep -vxF "$observed" || true)
if [ -n "$missing" ]; then
    echo "abi-baseline: expected but absent (harmless, but check it was meant):" >&2
    printf '%s\n' "$missing" | sed 's/^/    /' >&2
fi

if [ "$fail" -eq 0 ]; then
    echo "linkage    $(printf '%s\n' "$observed" | tr '\n' ' ')"
    echo "abi-baseline: ok"
fi
exit "$fail"
