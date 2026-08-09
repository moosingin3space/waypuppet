#!/bin/sh
# Run a command inside waypuppet's CI image, the way CI runs it.
#
#   ci/run.sh cargo test --workspace
#   ci/run.sh ci/toolkit-smoke.sh
#
# The point is that CI has no private incantation. Whatever the workflow does, this script
# does, so "it passes on my machine but not in CI" has one fewer place to hide. `just ci`
# runs the whole set.
#
# Cargo's home and target directory are kept outside the checkout, under ci/.cache, so a
# container run does not fight the Flatpak build for target/flatpak and neither one
# invalidates the other.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=${WAYPUPPET_CI_IMAGE:-waypuppet-ci:local}
cache="$root/ci/.cache"

engine=${WAYPUPPET_CONTAINER_ENGINE:-}
if [ -z "$engine" ]; then
    for candidate in podman docker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            engine=$candidate
            break
        fi
    done
fi
if [ -z "$engine" ]; then
    echo "ci/run.sh: no podman or docker on PATH" >&2
    exit 1
fi

# Built on demand locally. CI builds it as its own step so the layers can be cached
# between runs, and this then finds it already present.
if ! "$engine" image exists "$image" >/dev/null 2>&1 &&
    ! "$engine" image inspect "$image" >/dev/null 2>&1; then
    echo "ci/run.sh: building $image" >&2
    "$engine" build -f "$root/ci/Containerfile" -t "$image" "$root" >&2
fi

mkdir -p "$cache/cargo" "$cache/target"

# udmabuf(7), when the host has it. It turns a sealed memfd into a dma-buf with no GPU
# involved, which is what lets `tests/dmabuf.rs` run its client half here — this image
# stands in for a runner, and a runner has no render node.
#
# Deliberately *not* forwarding /dev/dri. This image exists to reproduce a machine without
# one, and handing it the host's GPU is worse than pointless: the container's Mesa does not
# match the host's kernel driver, and the capture tests segfault. The GPU path is covered
# by `just check`, in the Flatpak sandbox, which is built for it.
devices=""
if [ -e /dev/udmabuf ]; then
    devices="--device /dev/udmabuf"
fi

# `:Z` relabels for SELinux, which podman needs on Fedora-family hosts and docker ignores.
# shellcheck disable=SC2086  # $devices is a deliberately word-split flag list
exec "$engine" run --rm \
    $devices \
    --volume "$root:/workspace:Z" \
    --volume "$cache/cargo:/cargo:Z" \
    --volume "$cache/target:/target:Z" \
    --env CARGO_HOME=/cargo \
    --env CARGO_TARGET_DIR=/target \
    --workdir /workspace \
    "$image" "$@"
