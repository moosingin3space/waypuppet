# Freya 0.5.2 input regression

Run `just freya` to build and drive this app inside the Flatpak SDK. The test
asserts that a selector click reaches an ordinary button and activates it under
server, client, and auto decoration policies. It keeps decorations enabled in the
application, so client mode exercises winit's titlebar and content offset. The
probe must report the policy and acknowledged mode, and client mode must expose
the shifted root surface origin. Captures, probes, and trees are saved under
`ci/.cache/freya-smoke/<policy>/`.

The `freya` CI job runs the same script in the CI image and uploads that directory
as the `freya-diagnostics` artifact, including on failure. Use
`ci/run.sh sh ci/freya-smoke.sh` to reproduce that job locally; it also runs as part
of `just ci`.

The nested button is a separate diagnostic for Freya's `Input::trailing` behavior.
It still does not activate in 0.5.2, including when clicked at the correct visible
coordinates. That behavior is not an expected Waypuppet test outcome.
