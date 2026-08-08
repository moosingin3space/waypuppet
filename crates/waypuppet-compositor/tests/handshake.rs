//! Phase 01's acceptance test: a real Wayland client connects and maps a
//! window.
//!
//! This drives the compositor from the outside, with `wayland-client`, rather
//! than reaching into its state. That is deliberate — a toolkit only cares
//! about what it can observe over the protocol, so a test that asserts on the
//! same things fails for the same reasons a real client would.

mod fixture;

use waypuppet_proto::Size;

const OUTPUT: Size = Size {
    width: 640,
    height: 480,
};

#[test]
fn a_client_connects_configures_and_commits() {
    fixture::guarded("handshake", OUTPUT, |harness| {
        let mut client = harness.start_client("handshake");
        let (width, height) = harness.configure(&mut client);

        assert!(
            client.surface_was_configured(),
            "xdg_toplevel.configure arrived without xdg_surface.configure",
        );
        assert_eq!(
            (width, height),
            (
                i32::try_from(OUTPUT.width).unwrap(),
                i32::try_from(OUTPUT.height).unwrap()
            ),
            "the window should be configured to fill the output",
        );

        // Mapping is the part that can still fail: attaching a buffer and
        // committing exercises `wl_shm`, the buffer handler and the commit path.
        harness.paint(&mut client, [0xFF, 0xFF, 0xFF, 0xFF]);
        harness.drive_until("the window to map", |state| {
            state.records.values().any(|record| record.mapped)
        });

        harness.stop(client);
    });
}
