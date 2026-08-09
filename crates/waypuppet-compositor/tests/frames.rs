//! Clients are paced, whether or not anyone is watching.
//!
//! A toolkit does not draw whenever it likes. It requests a
//! `wl_surface.frame` callback before committing, and does not draw the next
//! frame until that callback fires. A compositor that never fires them gets
//! exactly one frame out of the application and then nothing.
//!
//! waypuppet sent them only from `capture`, on the reasoning that rendering is
//! on demand. That conflated two jobs. Drawing a frame is on demand and stays
//! that way — it is what makes two captures of the same state identical.
//! *Pacing a client* is not rendering, and withholding it made every real
//! toolkit look hung: `gtk3-widget-factory` mapped a window, drew once, and
//! then ignored every click and keystroke, because it was still waiting to be
//! told its first frame had been presented.
//!
//! Nothing captures here, deliberately. A test that took a screenshot would
//! send the callbacks itself and pass against the bug.

mod fixture;

use waypuppet_proto::Size;

const OUTPUT: Size = Size {
    width: 160,
    height: 120,
};

const PAINT: [u8; 4] = [0x40, 0x80, 0xC0, 0xFF];

#[test]
fn a_frame_callback_arrives_without_anyone_capturing() {
    fixture::guarded("frames", OUTPUT, |harness| {
        let mut client = harness.start_client("frame-fixture");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);
        harness.drive_until("the client to map", |state| {
            state.records.values().any(|record| record.mapped)
        });

        // Blocks until the compositor answers, ticking the loop meanwhile. If
        // callbacks only came from `capture`, this would sit here until the
        // harness gave up and named the wait.
        harness.await_frame(&mut client);

        harness.stop(client);
    });
}

#[test]
fn callbacks_keep_coming() {
    // One callback could be a coincidence of some other code path. A client
    // draws for as long as it is running, so the pacing has to keep up.
    fixture::guarded("frames-repeat", OUTPUT, |harness| {
        let mut client = harness.start_client("frame-fixture");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);
        harness.drive_until("the client to map", |state| {
            state.records.values().any(|record| record.mapped)
        });

        for _ in 0..5 {
            harness.await_frame(&mut client);
        }

        harness.stop(client);
    });
}
