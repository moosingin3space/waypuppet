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

#[test]
fn a_connected_client_is_visible_as_one_until_it_leaves() {
    fixture::guarded("handshake-connections", OUTPUT, |harness| {
        assert_eq!(
            harness.state().connections(),
            0,
            "nothing has connected yet",
        );

        let mut client = harness.start_client("handshake-connections");
        harness.configure(&mut client);

        // The only consumer is `ps`, which reports `Starting` until this is
        // non-zero — so getting it wrong means a running application described
        // as still starting up, forever. That is a wrong answer rather than a
        // crash, which is exactly the kind that survives a test suite: the
        // previous version counted from `ClientData::initialized`, which the
        // backend this crate builds against never calls.
        assert_eq!(
            harness.state().connections(),
            1,
            "a client that has connected and been configured is connected",
        );

        harness.stop(client);
        harness.drive_until("the client to be forgotten", |state| {
            state.connections() == 0
        });

        // And having gone, it stays gone. A count that went negative here used
        // to wrap to `usize::MAX`, which reads as "connected" ever after — the
        // same bug pointing the other way.
        assert_eq!(
            harness.state().connections(),
            0,
            "a departed client must not leave anything behind",
        );
    });
}
