//! Phase 06's acceptance tests: selections move in both directions.
//!
//! Four things have to be true, and they fail for different reasons.
//!
//! **waypuppet can own a selection**, so text set from the command line is
//! there to be pasted. The assertion is deliberately made from the *client's*
//! side, over the protocol: a selection the compositor believes it owns but
//! never offered to anybody would pass any check made against its own state and
//! fail this one.
//!
//! **waypuppet can read a client's selection**, which is the direction that
//! cannot be answered on the spot. The client writes down a pipe whenever it
//! feels like it, so the reply is parked on a transfer the event loop drives —
//! and the loop must keep turning throughout, because the client can only be
//! serviced by the loop that is waiting for it. That is the deadlock this whole
//! project is shaped around, in its smallest form.
//!
//! **Both selections work**, because a terminal uses both and they are separate
//! protocols with separate state.
//!
//! **An empty selection is not an error.** "Nothing has been copied" is an
//! ordinary state of affairs and has to be distinguishable from a transfer that
//! went wrong.

mod fixture;

use std::time::Duration;

use waypuppet_compositor::Command;
use waypuppet_proto::{Failure, Selection, Size};

const OUTPUT: Size = Size {
    width: 320,
    height: 240,
};

const PAINT: [u8; 4] = [0x40, 0x40, 0x80, 0xFF];

/// Long enough for a pipe transfer, short enough to fail while somebody looks.
const LIMIT: Duration = Duration::from_millis(500);

/// Bring a client up, mapped and focused.
///
/// Focus matters more here than anywhere else: a selection is offered to the
/// focused client and to nobody else, so a test that skipped the paint would be
/// asserting against a client that is not entitled to see anything.
fn ready(harness: &mut fixture::Harness) -> fixture::ClientThread {
    let mut client = harness.start_client("clipboard-fixture");
    harness.configure(&mut client);
    harness.paint(&mut client, PAINT);
    harness.drive_until("the client's first frame", |state| {
        state.records.values().any(|record| record.mapped)
    });
    client
}

/// Set a selection through the bridge, as the CLI does.
fn set(harness: &mut fixture::Harness, selection: Selection, text: &str) {
    let outcome = harness.deliver("the selection to be taken", LIMIT, |reply| {
        Command::ClipboardSet {
            selection,
            text: text.to_owned(),
            reply,
        }
    });
    assert_eq!(
        outcome,
        Some(Ok(())),
        "taking a selection is bookkeeping and must answer at once",
    );
}

// ------------------------------------------------------- waypuppet offers --

#[test]
fn text_set_by_waypuppet_is_what_the_client_can_paste() {
    fixture::guarded("clipboard-set", OUTPUT, |harness| {
        let mut client = ready(harness);

        set(harness, Selection::Clipboard, "hello from waypuppet");

        assert_eq!(
            harness.client_sees(&mut client, Selection::Clipboard),
            Some("hello from waypuppet".to_owned()),
            "the client should be able to read back exactly what was offered",
        );

        harness.stop(client);
    });
}

#[test]
fn the_primary_selection_is_separate_from_the_clipboard() {
    fixture::guarded("clipboard-both", OUTPUT, |harness| {
        let mut client = ready(harness);

        // Two protocols, two pieces of state. Setting one must not disturb the
        // other, which is exactly the confusion a terminal would suffer from.
        set(harness, Selection::Clipboard, "copied");
        set(harness, Selection::Primary, "selected");

        assert_eq!(
            harness.client_sees(&mut client, Selection::Clipboard),
            Some("copied".to_owned()),
        );
        assert_eq!(
            harness.client_sees(&mut client, Selection::Primary),
            Some("selected".to_owned()),
        );

        harness.stop(client);
    });
}

#[test]
fn a_selection_survives_being_replaced() {
    fixture::guarded("clipboard-replace", OUTPUT, |harness| {
        let mut client = ready(harness);

        set(harness, Selection::Clipboard, "first");
        set(harness, Selection::Clipboard, "second");

        // The second offer supersedes the first, and the client must be told:
        // a stale offer that still reads "first" is how a paste ends up one
        // copy behind for ever.
        assert_eq!(
            harness.client_sees(&mut client, Selection::Clipboard),
            Some("second".to_owned()),
        );

        harness.stop(client);
    });
}

#[test]
fn a_selection_that_waypuppet_set_reads_back_through_waypuppet() {
    fixture::guarded("clipboard-roundtrip", OUTPUT, |harness| {
        let mut client = ready(harness);

        set(harness, Selection::Clipboard, "round trip");

        // No client is involved in answering this one: asking the client for a
        // selection the compositor owns is asking the wrong party, and smithay
        // rightly refuses. Reading it back has to be answered from what
        // waypuppet itself offered.
        assert_eq!(
            harness.read_selection(&mut client, Selection::Clipboard),
            Ok(Some("round trip".to_owned())),
            "`clipboard set` followed by `clipboard get` must round-trip",
        );

        harness.stop(client);
    });
}

// ---------------------------------------------------------- client offers --

#[test]
fn text_the_client_owns_is_read_back_through_the_pipe() {
    fixture::guarded("clipboard-get", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.own_selection(&mut client, Selection::Clipboard, "hello from the client");

        // The interesting direction. waypuppet hands the client a pipe and the
        // client fills it whenever it likes, so this cannot be answered inline
        // — and the loop has to keep turning while it is outstanding, because
        // the client is only serviced by that same loop.
        assert_eq!(
            harness.read_selection(&mut client, Selection::Clipboard),
            Ok(Some("hello from the client".to_owned())),
        );

        harness.stop(client);
    });
}

#[test]
fn the_clients_primary_selection_is_read_back_too() {
    fixture::guarded("clipboard-get-primary", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.own_selection(&mut client, Selection::Primary, "middle click me");

        assert_eq!(
            harness.read_selection(&mut client, Selection::Primary),
            Ok(Some("middle click me".to_owned())),
        );

        harness.stop(client);
    });
}

#[test]
fn a_selection_larger_than_a_pipe_still_arrives_whole() {
    fixture::guarded("clipboard-large", OUTPUT, |harness| {
        let mut client = ready(harness);

        // Comfortably past the 64KiB a pipe buffers, so the transfer cannot
        // complete in one write and both ends have to come back for more. A
        // version that read once and stopped would return a neat 64KiB prefix
        // and look like it worked.
        let text = "waypuppet ".repeat(30_000);
        assert!(text.len() > 256 * 1024, "the test needs to exceed a pipe");
        harness.own_selection(&mut client, Selection::Clipboard, &text);

        assert_eq!(
            harness.read_selection(&mut client, Selection::Clipboard),
            Ok(Some(text)),
            "a transfer that spans several wake-ups must still be exact",
        );

        harness.stop(client);
    });
}

// ---------------------------------------------------------------- nothing --

#[test]
fn an_empty_selection_is_an_answer_rather_than_a_failure() {
    fixture::guarded("clipboard-empty", OUTPUT, |harness| {
        let mut client = ready(harness);

        // Nobody has copied anything. That is an ordinary state of affairs, and
        // a caller reading it needs to tell it from a transfer that broke —
        // `Ok(None)` against `Err`, not an empty string against a full one.
        assert_eq!(
            harness.read_selection(&mut client, Selection::Clipboard),
            Ok(None),
            "an empty clipboard is not a failure",
        );
        assert_eq!(
            harness.read_selection(&mut client, Selection::Primary),
            Ok(None),
        );

        harness.stop(client);
    });
}

#[test]
fn a_read_nobody_is_waiting_for_stops_holding_its_pipe() {
    fixture::guarded("clipboard-abandoned", OUTPUT, |harness| {
        let mut client = ready(harness);

        // A client that owns the selection and never answers. waypuppet asks,
        // parks, and the caller gives up — at which point the transfer must be
        // torn down rather than holding a pipe and a loop registration for the
        // rest of the session.
        harness.own_selection(&mut client, Selection::Clipboard, "never sent");

        let (reply, receiver) = tokio::sync::oneshot::channel();
        harness
            .commands()
            .send(Command::ClipboardGet {
                selection: Selection::Clipboard,
                reply,
            })
            .expect("the compositor is listening");
        // Not pumped, so the client never gets as far as writing.
        harness.drive_until("the transfer to start", |state| state.transfers() == 1);

        drop(receiver);
        harness.drive_until("the transfer to be swept", |state| state.transfers() == 0);

        harness.stop(client);
    });
}

#[test]
fn a_selection_that_is_not_text_is_a_failure_rather_than_an_empty_answer() {
    fixture::guarded("clipboard-wrong-type", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.own_selection_as(
            &mut client,
            Selection::Clipboard,
            "image/png",
            "not really a png",
        );

        // There *is* a selection; it simply cannot be had as text. Answering
        // `None` would tell the caller the clipboard was empty, which is a
        // different and wrong thing.
        assert!(
            matches!(
                harness.read_selection(&mut client, Selection::Clipboard),
                Err(Failure::Clipboard(_))
            ),
            "a selection in the wrong type must not be reported as no selection",
        );

        harness.stop(client);
    });
}
