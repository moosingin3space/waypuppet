//! A client that draws through `zwp_linux_dmabuf_v1` is composited.
//!
//! This is the gate on hosting a real toolkit. Anything Skia-backed — freya,
//! which is what waypuppet was built to look at — renders through EGL and has
//! nowhere to put its buffers without this protocol, so `wl_shm` working proves
//! nothing about whether the interesting case works.
//!
//! The fixture allocates on a render node with gbm, writes a known colour into
//! the mapped buffer, exports it as a dmabuf and hands the file descriptors to
//! the compositor. If the pixel comes back out of a capture, every step of the
//! import path ran: the global was advertised, the format was one the renderer
//! accepts, `dmabuf_imported` said yes, and the texture composited.
//!
//! # This does not skip
//!
//! Allocation needs a render node the *test process* may open, and a machine
//! without one fails here rather than quietly passing. The rest of the suite
//! already needs a rendering device — `capture.rs` cannot run without one — so
//! a skip would buy nothing and cost the only signal this test has. An earlier
//! version did skip, and swallowed a deliberately broken import as "this
//! machine cannot do dmabuf": it passed against a compositor that rejected
//! every buffer.

mod fixture;

use waypuppet_proto::Size;

const OUTPUT: Size = Size {
    width: 320,
    height: 240,
};

/// Different in all three channels, so a swapped red and blue cannot pass.
/// Deliberately not the colour `capture.rs` uses: if the two ever got crossed,
/// identical constants would hide it.
const PAINT: [u8; 4] = [0xC0, 0x30, 0x80, 0xFF];

#[test]
fn a_dmabuf_client_is_composited() {
    fixture::guarded("dmabuf", OUTPUT, |harness| {
        let mut client = harness.start_client("dmabuf-fixture");
        harness.configure(&mut client);

        harness
            .paint_dmabuf(&mut client, PAINT)
            .expect("the client can allocate and export a dmabuf");

        harness.drive_until("the client's dmabuf to land", |state| {
            state.records.values().any(|record| record.mapped)
        });

        let frame = harness.capture();
        let centre = frame
            .pixel(OUTPUT.width / 2, OUTPUT.height / 2)
            .expect("the centre is inside the frame");
        assert_eq!(
            centre,
            (PAINT[0], PAINT[1], PAINT[2], PAINT[3]),
            "the capture should show what the client wrote into its dmabuf; \
             a black frame means the buffer was accepted but never sampled",
        );

        harness.stop(client);
    });
}

#[test]
fn a_dmabuf_client_and_an_shm_client_agree() {
    // The two import paths have to produce the same picture, or a golden image
    // is only valid for whichever kind of client generated it. This is the
    // cheapest way to catch a format or byte-order mistake on one path that the
    // other does not have.
    fixture::guarded("dmabuf-vs-shm", OUTPUT, |harness| {
        let mut client = harness.start_client("dmabuf-fixture");
        harness.configure(&mut client);

        harness
            .paint_dmabuf(&mut client, PAINT)
            .expect("the client can allocate and export a dmabuf");
        harness.drive_until("the client's dmabuf to land", |state| {
            state.records.values().any(|record| record.mapped)
        });
        let through_dmabuf = harness.capture();

        // The same client, the same colour, the other buffer type.
        harness.paint(&mut client, PAINT);
        harness.drive_until("the shm buffer to land", |state| {
            state
                .records
                .values()
                .any(|record| record.mapped && record.frames > 1)
        });
        let through_shm = harness.capture();

        assert_eq!(
            through_dmabuf.pixels, through_shm.pixels,
            "the same colour through the two import paths must composite identically",
        );

        harness.stop(client);
    });
}
