//! Phase 02's acceptance test: a committed frame comes back as pixels.

mod fixture;

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
    committed_frame("capture", waypuppet_compositor::Rendering::Fastest);
}

/// The same on llvmpipe, which is what a runner with no render node gets.
///
/// Not redundant with the test above: the software fallback is the path most of
/// waypuppet's users and all of its CI will take, and "it works on the GPU here"
/// says nothing about it.
#[test]
fn a_committed_frame_is_captured_in_software() {
    committed_frame(
        "capture-software",
        waypuppet_compositor::Rendering::Software,
    );
}

fn committed_frame(name: &'static str, rendering: waypuppet_compositor::Rendering) {
    // Only checked in one direction. `Software` is a promise and is asserted;
    // `Fastest` is "whatever this machine has", which on a runner with no
    // render node is software too — asserting otherwise would fail CI for
    // being CI.
    let must_be_software = rendering == waypuppet_compositor::Rendering::Software;
    fixture::guarded_on(name, OUTPUT, rendering, move |harness| {
        assert!(
            !must_be_software || harness.renders_in_software(),
            "--software was asked for and this session is not on it, so the test \
             proves nothing about the software path",
        );

        let mut client = harness.start_client("capture-fixture");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);

        harness.drive_until("the client's buffer to land", |state| {
            state.records.values().any(|record| record.mapped)
        });

        let frames = harness
            .state()
            .records
            .values()
            .find(|record| record.mapped)
            .map(|record| record.frames);
        assert!(
            frames.is_some_and(|frames| frames > 0),
            "the client never committed a buffer",
        );

        let frame = harness.capture();

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

        harness.stop(client);
    });
}

#[test]
fn capturing_an_empty_session_gives_the_clear_colour() {
    fixture::guarded("empty-capture", OUTPUT, |harness| {
        let frame = harness.capture();
        assert_eq!(
            frame.pixel(0, 0),
            Some((0, 0, 0, 255)),
            "an empty output should be opaque black",
        );
    });
}
