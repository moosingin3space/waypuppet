//! Phase 02's acceptance test: a committed frame comes back as pixels.
//!
//! The compositor runs on the test's own thread, driven a tick at a time, and
//! the client lives on another. That way round because capturing needs the
//! compositor between iterations of its loop — which is the same shape the
//! control socket will need in plan 03.

mod fixture;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use waypuppet_compositor::{Compositor, Config};
use waypuppet_proto::Size;

const OUTPUT: Size = Size {
    width: 320,
    height: 240,
};

/// Distinctive on purpose: a colour that is not grey, not black, and has
/// different values in all three channels catches a swapped red and blue, which
/// an all-white or all-grey fixture would sail straight past.
const PAINT: [u8; 4] = [0x20, 0xC0, 0x40, 0xFF];

#[test]
fn a_committed_frame_is_captured_and_written() {
    let mut compositor = Compositor::new(&Config {
        output: OUTPUT,
        ..Config::default()
    })
    .expect("compositor starts");

    let socket = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set in the sandbox"),
    )
    .join(compositor.socket_name());

    let stop = Arc::new(AtomicBool::new(false));
    let client = thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let mut client = fixture::Client::connect(&socket, "capture-fixture");
            let size = client.wait_for_configure();
            client.paint(size, PAINT);

            // Stay connected, and do nothing else. A surface whose client has
            // gone is unmapped and there would be nothing left to capture, but
            // pumping here would deadlock: the compositor stops being ticked
            // once the main thread has its capture, so a roundtrip would never
            // return and the join below would wait on it forever.
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(2));
            }
            drop(client);
        }
    });

    // Drive the compositor until the client's buffer has landed.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        compositor.tick(Duration::from_millis(5)).expect("tick");
        if compositor
            .state()
            .records
            .values()
            .any(|record| record.mapped)
        {
            break;
        }
    }

    let record_frames = compositor
        .state()
        .records
        .values()
        .find(|record| record.mapped)
        .map(|record| record.frames);
    assert!(
        record_frames.is_some_and(|frames| frames > 0),
        "the client never committed a buffer",
    );

    let frame = compositor.capture().expect("capture succeeds");

    assert_eq!(frame.width, OUTPUT.width);
    assert_eq!(frame.height, OUTPUT.height);

    let centre = frame
        .pixel(OUTPUT.width / 2, OUTPUT.height / 2)
        .expect("the centre is inside the frame");
    assert_eq!(
        centre,
        (PAINT[0], PAINT[1], PAINT[2], PAINT[3]),
        "the centre of the capture should be the colour the client painted",
    );

    // And it survives a round trip through PNG.
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("capture.png");
    frame.write_png(&path).expect("png is written");
    assert!(
        path.metadata().is_ok_and(|meta| meta.len() > 0),
        "the PNG should not be empty",
    );

    stop.store(true, Ordering::Relaxed);
    client.join().expect("the client thread finishes");
}

#[test]
fn capturing_an_empty_session_gives_the_clear_colour() {
    let mut compositor = Compositor::new(&Config {
        output: OUTPUT,
        ..Config::default()
    })
    .expect("compositor starts");

    compositor.tick(Duration::from_millis(5)).expect("tick");
    let frame = compositor.capture().expect("capture succeeds");

    assert_eq!(
        frame.pixel(0, 0),
        Some((0, 0, 0, 255)),
        "an empty output should be opaque black",
    );
}
