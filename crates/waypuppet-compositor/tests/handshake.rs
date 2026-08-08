//! Phase 01's acceptance test: a real Wayland client connects and maps a
//! window.
//!
//! This drives the compositor from the outside, with `wayland-client`, rather
//! than reaching into its state. That is deliberate — a toolkit only cares
//! about what it can observe over the protocol, so a test that asserts on the
//! same things fails for the same reasons a real client would.

mod fixture;

use std::{path::PathBuf, sync::mpsc, time::Duration};

use waypuppet_compositor::{Compositor, Config};
use waypuppet_proto::Size;

const OUTPUT: Size = Size {
    width: 640,
    height: 480,
};

#[test]
fn a_client_connects_configures_and_commits() {
    let (socket_tx, socket_rx) = mpsc::channel();

    // The compositor is not `Send`, so it is built on the thread that runs it
    // and only its socket name comes back.
    let compositor = std::thread::spawn(move || {
        let mut compositor = Compositor::new(&Config {
            output: OUTPUT,
            ..Config::default()
        })
        .expect("compositor starts");
        socket_tx
            .send(compositor.socket_name().to_owned())
            .expect("the test is still listening");
        compositor.run().expect("compositor runs");
    });

    let socket_name = socket_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the compositor reports its socket");
    let socket = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set in the sandbox"),
    )
    .join(&socket_name);

    let mut client = fixture::Client::connect(&socket, "handshake");
    let (width, height) = client.wait_for_configure();

    assert!(
        client.events.surface_configured,
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
    client.paint((width, height), [0xFF, 0xFF, 0xFF, 0xFF]);

    drop(client);
    drop(compositor);
}
