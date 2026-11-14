#!/bin/sh
# Run inside the SDK or CI image. This deliberately leaves decorations
# enabled in the application. Exercise each compositor policy and require a
# selector to activate an ordinary button even with winit's fallback titlebar.
set -eu
cargo build --quiet -p waypuppet
cargo build --quiet --locked --manifest-path ci/freya-smoke/Cargo.toml
target_dir=${CARGO_TARGET_DIR:-target}
case "$target_dir" in
    /*) ;;
    *) target_dir="$(pwd)/$target_dir" ;;
esac
bin="$target_dir/debug/waypuppet"
app="$target_dir/debug/freya-waypuppet-052"
out="$(pwd)/ci/.cache/freya-smoke"
mkdir -p "$out"
session=freya-smoke
waypuppet() { "$bin" --session "$session" "$@"; }
cleanup() {
    result=$?
    if [ "$result" -ne 0 ]; then
        waypuppet logs > "$out/failure.log" 2>&1 || true
        waypuppet decorations --json > "$out/failure-decorations.json" 2>&1 || true
        waypuppet shot -o "$out/failure.png" >/dev/null 2>&1 || true
    fi
    waypuppet kill >/dev/null 2>&1 || true
}
trap cleanup EXIT
for policy in server client auto; do
out="$(pwd)/ci/.cache/freya-smoke/$policy"
mkdir -p "$out"
waypuppet run --detach --software --sandbox none --decorations "$policy" -- "$app"
waypuppet wait --windows 1 --timeout 60000
# A mapped window can precede accessibility registration. The test app explicitly
# waits for AT-SPI activation below through bounded retries on tree availability.
python3 - "$bin" "$session" "$out/before.tree" <<'PY'
import subprocess, sys, time
binary, session, path = sys.argv[1:]
deadline = time.monotonic() + 15
while True:
    result = subprocess.run([binary, '--session', session, 'tree'], capture_output=True, text=True)
    if result.returncode == 0 and 'button "Adjacent"' in result.stdout:
        open(path, 'w').write(result.stdout)
        break
    if time.monotonic() >= deadline:
        sys.exit('Freya never published the Adjacent button: ' + result.stderr)
    time.sleep(0.05)
PY
waypuppet decorations --json > "$out/decorations.json"
python3 - "$out/decorations.json" "$policy" <<'PYPROBE'
import json, sys
windows = json.load(open(sys.argv[1]))
assert len(windows) == 1, windows
observed = windows[0]['decorations']
assert observed['policy'] == sys.argv[2], observed
# Winit prefers server mode. Forced client mode must override that preference.
expected = 'client' if sys.argv[2] == 'client' else 'server'
assert observed['negotiated'] == expected, observed
if expected == 'client':
    assert observed['surface_origin']['y'] > windows[0]['geometry']['origin']['y'], observed
else:
    assert observed['surface_origin'] == windows[0]['geometry']['origin'], observed
PYPROBE
waypuppet click --on 'button[name="Adjacent"]'
waypuppet tree > "$out/after.tree"
# Both a raw press and Freya's activation callback must have reached the app.
grep -q 'label "pointer down"' "$out/after.tree"
grep -q 'label "adjacent clicked"' "$out/after.tree"
waypuppet shot -o "$out/after.png"
if [ "$policy" = client ]; then
    # This pinned winit fixture puts its close button 18px from the right edge,
    # at y=17 in the decorated window. Convert using the probe, then prove the
    # decoration subsurface receives input too. Closing need not settle on a
    # new frame; the child exit below is the observable result.
    close_at=$(python3 - "$out/decorations.json" <<'PYCLOSE'
import json, sys
window = json.load(open(sys.argv[1]))[0]
geometry = window['geometry']
root = window['decorations']['surface_origin']
x = geometry['origin']['x'] + geometry['size']['width'] - 18 - root['x']
y = geometry['origin']['y'] + 17 - root['y']
print(f'{x},{y}')
PYCLOSE
)
    waypuppet click --at "$close_at" > "$out/close.log" 2>&1 || true
    waypuppet wait --exit --timeout 5000
fi
echo "Freya 0.5.2 ($policy): probe and selector click passed"
waypuppet kill
done
