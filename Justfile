# waypuppet development commands. `just` on its own lists them.
#
# Everything builds and runs inside the Flatpak SDK. The SDK supplies the system
# libraries — libwayland-server, EGL, GBM, libdrm and xkbcommon, plus a C toolchain for
# gbm's bindings — so the build does not depend on what any given machine happens to have
# installed, while mise supplies the pinned Rust toolchain *inside* it. That split keeps
# mise.toml the single source of truth for the compiler.
#
# Run `just setup` once after checkout.

sdk := "org.freedesktop.Sdk//25.08"
brew_prefix := env("WAYPUPPET_BREW_PREFIX", "/home/linuxbrew/.linuxbrew")
home := env("HOME")

# `--user` disambiguates: the SDK may be installed both system- and user-wide, and
# flatpak stops to ask which otherwise.
#
# `--device=dri` is the load-bearing one. waypuppet renders offscreen on a DRM render
# node, so without it there is nothing to render with and `just doctor` says so.
#
# Deliberately absent: `--socket=wayland`. waypuppet must work with no compositor to
# nest inside, since that is the situation it exists to fix, and the surest way to keep
# that true is to develop it that way. `just nested` opts back in for debugging.
#
# mise itself comes from brew, mounted read-only; its installs, rustup and cargo homes
# are mounted read-write so the toolchain it manages is the same one the host sees.
#
# MISE_TRUSTED_CONFIG_PATHS stands in for the trust store, which lives outside the
# mounted paths and would otherwise make every mise call fail as untrusted.
#
# A separate CARGO_TARGET_DIR keeps sandbox artifacts from colliding with any host
# build that happens outside these recipes.
sandbox_args := "--user --devel --share=network --share=ipc --device=dri" \
    + " --filesystem=" + justfile_directory() \
    + " --filesystem=" + home / ".local/share/mise" \
    + " --filesystem=" + home / ".cargo" \
    + " --filesystem=" + home / ".rustup" \
    + " --filesystem=" + brew_prefix + ":ro" \
    + " --env=MISE_TRUSTED_CONFIG_PATHS=" + justfile_directory() \
    + " --env=CARGO_TARGET_DIR=target/flatpak"

# mise is invoked by absolute path rather than by putting brew's bin on PATH.
#
# That prepend used to be here, and it reached further than intended: `just cli run -- app`
# spawns the application under test as a descendant of this shell, so the application
# inherited it too. A GTK client started that way rendered every glyph as a box — text
# silently unreadable, no warning anywhere — while the same binary launched from a plain
# SDK shell rendered normally. Since the only thing here that needs brew is mise, naming it
# outright keeps the toolchain out of the environment the application under test sees.
sandbox_prelude := "cd " + justfile_directory() + ";"
mise := brew_prefix / "bin/mise"

# Default recipe: list everything.
default:
    @just --list

# Install the SDK and the pinned toolchain. Idempotent; run once after checkout.
setup:
    flatpak install --user --noninteractive {{ sdk }}
    mise install

# Run any command inside the SDK with the mise toolchain active.
[private]
sdk +cmd:
    flatpak run {{ sandbox_args }} --command=sh {{ sdk }} \
        -c '{{ sandbox_prelude }} {{ mise }} exec -- {{ cmd }}'

# Build the workspace.
build: (sdk "cargo build --workspace")

# Report what waypuppet can render with. Run this first in a new container.
doctor: (sdk "cargo run -p waypuppet -- doctor")

# Run the CLI. Flags pass straight through: `just cli shot -o /tmp/out.png`.
# Do not add a `--` separator; this recipe already supplies one.
cli +args="--help": (sdk ("cargo run -p waypuppet -- " + args))

# `--include-ignored` picks up `tests/dmabuf.rs`, whose *client* half allocates with gbm
# and so needs a `/dev/dri/renderD*`. The sandbox is given one with `--device=dri`; a
# GitHub runner has none, which is why those tests declare the precondition instead of
# discovering it.
[doc("Run the full test suite, including the tests that need a render node.")]
test: (sdk "cargo test --workspace -- --include-ignored")

# Clippy + fmt check, matching CI expectations.
lint: (sdk "cargo clippy --workspace --all-targets") (sdk "cargo fmt --all --check")

# Apply rustfmt in place.
fmt: (sdk "cargo fmt --all")

# Clippy + fmt + tests, in that order. Run before opening a PR.
check: lint test

# Interactive shell in the SDK, toolchain on PATH.
shell:
    flatpak run {{ sandbox_args }} --command=bash {{ sdk }} \
        -c '{{ sandbox_prelude }} eval "$(mise activate bash)"; exec bash'

# Same, but with the host's Wayland socket passed in, so a client started by hand can
# be pointed at the real desktop for comparison. Not how waypuppet is meant to run.
nested:
    flatpak run {{ sandbox_args }} --socket=wayland --command=bash {{ sdk }} \
        -c '{{ sandbox_prelude }} eval "$(mise activate bash)"; exec bash'

# The Flatpak path above is the inner loop; this is the answer to "will CI agree?" without
# pushing to find out. It needs podman or docker, builds `ci/Containerfile` on first use,
# and keeps its cargo cache under `ci/.cache` so the two builds do not invalidate each
# other.
[doc("Run what CI runs, in the image CI runs it in.")]
ci:
    ci/run.sh cargo fmt --all --check
    ci/run.sh cargo clippy --workspace --all-targets
    ci/run.sh cargo test --workspace
    ci/run.sh cargo run --quiet -p waypuppet -- doctor

# The unit suite proves the protocol against a fixture that behaves better than any real
# toolkit. This proves the thing the fixture cannot.
[doc("Drive a real GTK application under waypuppet, in the CI image.")]
toolkit:
    ci/run.sh ci/toolkit-smoke.sh

# The distributable binary is built somewhere older than anything else here, and
# deliberately: it is glibc 2.35 (Ubuntu 22.04), which is the floor waypuppet
# promises to run against. The Flatpak SDK is newer and the CI image is newer
# still, so neither of them can answer "will this run on a stranger's machine".

# Build the release binary against the oldest supported glibc, and check its ABI.
[doc("Build the distributable binary and assert what it needs to run.")]
release:
    ci/release.sh

# Cheap enough to run against any binary, and worth running against one built any
# other way before wondering why it will not start somewhere.

# Assert a binary's glibc floor and load-time dependencies.
[doc("Check a binary against the distribution ABI baseline.")]
abi binary="ci/release/waypuppet":
    ci/abi-baseline.sh {{ binary }}
