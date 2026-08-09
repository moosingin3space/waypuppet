//! Phase 06's other half: a window smaller than the output.
//!
//! `resize` itself landed with [plan 04](../../../plans/04-input.md), because it
//! is a mutating verb like any other — it reconfigures the `xdg_toplevel` and
//! settles on the frame the client draws in response. What was never covered is
//! the case this file is for: a window that does not fill the output.
//!
//! That is worth its own test because it is the only one where the compositing
//! path has to draw anything of its own. Every other capture in the suite is
//! wall-to-wall client buffer, so a renderer that ignored the clear colour, or
//! placed the window wrongly, or scaled it to fit, would produce an identical
//! image. Here those three produce three different wrong answers.
//!
//! Note what is *not* asserted: that the client honours the size it was given.
//! A configure is a request, and a client is entitled to come back with
//! something else. What waypuppet promises is that the request was made and
//! that whatever came back is composited where it belongs.

mod fixture;

use waypuppet_compositor::Command;
use waypuppet_proto::{Size, Target};

const OUTPUT: Size = Size {
    width: 320,
    height: 240,
};

/// Comfortably inside the output on both axes, and not a neat fraction of it,
/// so an off-by-scale error cannot land on the right answer by accident.
const SMALLER: Size = Size {
    width: 140,
    height: 100,
};

const PAINT: [u8; 4] = [0x20, 0xC0, 0x40, 0xFF];

/// Opaque black, which is what `render_output` is given as the clear colour.
const BACKGROUND: (u8, u8, u8, u8) = (0, 0, 0, 255);

#[test]
fn a_window_smaller_than_the_output_leaves_background_around_it() {
    fixture::guarded("resize-smaller", OUTPUT, |harness| {
        let mut client = harness.start_client("resize-fixture");
        let first = harness.configure(&mut client);
        assert_eq!(
            first,
            (
                i32::try_from(OUTPUT.width).unwrap(),
                i32::try_from(OUTPUT.height).unwrap()
            ),
            "a window starts out filling the output",
        );
        harness.paint(&mut client, PAINT);
        harness.drive_until("the window to map", |state| {
            state.records.values().any(|record| record.mapped)
        });

        // Parked rather than awaited: `resize` settles on the frame the client
        // draws in response, and the client cannot draw it until this test
        // tells it to. Blocking here would be waiting on something this thread
        // is holding up.
        let (reply, mut settled) = tokio::sync::oneshot::channel();
        harness
            .commands()
            .send(Command::Resize {
                size: SMALLER,
                target: Target::Focused,
                reply,
            })
            .expect("the compositor is listening");

        let reconfigured = harness.configure(&mut client);
        assert_eq!(
            reconfigured,
            (
                i32::try_from(SMALLER.width).unwrap(),
                i32::try_from(SMALLER.height).unwrap()
            ),
            "the client should have been asked for the new size",
        );

        // The client honours it, which is what makes the capture below mean
        // something about compositing rather than about configure handling.
        harness.paint(&mut client, PAINT);
        harness.drive_until("the resize to settle", |_| {
            !matches!(
                settled.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            )
        });

        let frame = harness.capture();
        assert_eq!(frame.width, OUTPUT.width, "the output does not resize");
        assert_eq!(frame.height, OUTPUT.height);

        // Inside the window, near its far corner rather than its centre: the
        // centre of a 140x100 window is also well inside a scaled-up one, so it
        // would not catch a renderer that stretched the buffer to fill.
        let inside = frame
            .pixel(SMALLER.width - 2, SMALLER.height - 2)
            .expect("inside the frame");
        assert_eq!(
            inside,
            (PAINT[0], PAINT[1], PAINT[2], PAINT[3]),
            "the window's own pixels should be where the window is",
        );

        // Just outside it on each axis, and far outside. If the buffer were
        // scaled to the output these would all be the paint colour; if the
        // window were placed elsewhere, the first two would be.
        for (x, y, what) in [
            (SMALLER.width + 2, SMALLER.height / 2, "past the right edge"),
            (
                SMALLER.width / 2,
                SMALLER.height + 2,
                "past the bottom edge",
            ),
            (OUTPUT.width - 2, OUTPUT.height - 2, "the far corner"),
        ] {
            assert_eq!(
                frame.pixel(x, y).expect("inside the frame"),
                BACKGROUND,
                "{what} should be background, not window",
            );
        }

        harness.stop(client);
    });
}

#[test]
fn a_resized_window_reports_its_new_geometry() {
    fixture::guarded("resize-geometry", OUTPUT, |harness| {
        let mut client = harness.start_client("resize-fixture");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);
        harness.drive_until("the window to map", |state| {
            state.records.values().any(|record| record.mapped)
        });

        let (reply, mut settled) = tokio::sync::oneshot::channel();
        harness
            .commands()
            .send(Command::Resize {
                size: SMALLER,
                target: Target::Focused,
                reply,
            })
            .expect("the compositor is listening");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);
        harness.drive_until("the resize to settle", |_| {
            !matches!(
                settled.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            )
        });

        // `ps` has to agree with the picture. A capture that shows a small
        // window while `ps` still reports the old size would send a caller
        // hunting for a rendering bug that is really bookkeeping.
        let window = harness
            .state()
            .snapshot()
            .windows
            .first()
            .cloned()
            .expect("the session has a window");
        assert_eq!(
            window.geometry.size, SMALLER,
            "the reported geometry should be the size the client drew at",
        );

        harness.stop(client);
    });
}
