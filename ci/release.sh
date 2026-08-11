#!/bin/sh
# Build the distributable waypuppet binary and check what it needs to run.
#
#   ci/release.sh              # build, then assert the ABI baseline
#
# The artifact lands in ci/release/waypuppet. It is built against Ubuntu 22.04's
# glibc so that it runs on anything newer, and ci/abi-baseline.sh is run on it
# immediately — a release binary that quietly needs a newer libc, or that has
# picked up a load-time dependency on mesa, is one whose failure happens on
# somebody else's machine.
#
# Cache and target directory live under ci/.cache-release, so this never fights
# the Flatpak build for target/flatpak or the test image for ci/.cache.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=${WAYPUPPET_RELEASE_IMAGE:-waypuppet-release:local}
cache="$root/ci/.cache-release"
out="$root/ci/release"

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
    echo "release: no podman or docker on PATH" >&2
    exit 1
fi

mkdir -p "$cache/registry" "$cache/git" "$cache/target" "$out"

if ! "$engine" image exists "$image" 2>/dev/null \
    && ! "$engine" image inspect "$image" >/dev/null 2>&1; then
    echo "release: building $image"
    "$engine" build --file "$root/ci/Containerfile.release" --tag "$image" "$root"
fi

# `:z` relabels for SELinux, which an image-based host needs and which is a no-op
# elsewhere. Without it the container cannot read its own checkout.
"$engine" run --rm \
    --volume "$root:/workspace:z" \
    --volume "$cache/registry:/root/.cargo/registry:z" \
    --volume "$cache/git:/root/.cargo/git:z" \
    --volume "$cache/target:/workspace/ci/.cache-release/target:z" \
    --env CARGO_TARGET_DIR=/workspace/ci/.cache-release/target \
    --workdir /workspace \
    "$image" \
    sh -eu -c '
        mise exec -- cargo build --release -p waypuppet
        cp ci/.cache-release/target/release/waypuppet ci/release/waypuppet
        ci/abi-baseline.sh ci/release/waypuppet
    '

echo
echo "release: ci/release/waypuppet"
ls -l "$out/waypuppet"
