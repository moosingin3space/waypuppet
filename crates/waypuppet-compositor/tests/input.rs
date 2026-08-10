//! Phase 04's acceptance tests: input arrives, and mutating verbs settle.
//!
//! Two distinct claims are checked here, and they fail for different reasons.
//!
//! **The events are right.** The fixture resolves every key through the keymap
//! the compositor sent over the wire, so a reverse index that picked the wrong
//! keycode, or forgot the shift a symbol needs, shows up as the wrong keysym
//! name. Asserting against waypuppet's own `Keymap` would agree with itself.
//!
//! **The reply waits for the frame.** A settling verb must return only once the
//! client has drawn, and must fail rather than hang when it does not. The
//! interesting case is the failure: a client that draws nothing has to be told
//! apart from a compositor that stopped, which is the distinction the whole
//! bridge is built around.

mod fixture;

use std::time::Duration;

use fixture::Input;
use waypuppet_compositor::Command;
use waypuppet_proto::{Button, Failure, Point, Size, Target};

const OUTPUT: Size = Size {
    width: 320,
    height: 240,
};

const PAINT: [u8; 4] = [0x20, 0xC0, 0x40, 0xFF];

/// How long to give a settling verb before calling it unsettled.
///
/// Short: the fixture client draws immediately when it is going to draw at all,
/// so anything longer is only time spent waiting for a test to fail.
const SETTLE: Duration = Duration::from_millis(500);

/// Bring a client up, painted and focused, ready to receive input.
fn ready(harness: &mut fixture::Harness) -> fixture::ClientThread {
    let mut client = harness.start_client("input-fixture");
    harness.configure(&mut client);
    harness.paint(&mut client, PAINT);
    harness.drive_until("the client's first frame", |state| {
        state.records.values().any(|record| record.mapped)
    });
    // The keymap and the keyboard's enter arrive with focus, which the
    // compositor grants on that first frame. Draining here leaves the assertions
    // below looking only at what the test itself caused.
    harness.input(&mut client);
    client
}

#[test]
fn chords_arrive_as_the_right_keysyms_in_order() {
    fixture::guarded("input-keys", OUTPUT, |harness| {
        let mut client = ready(harness);

        // No paint in response, so this will not settle — the events are still
        // delivered, which is what is under test here.
        harness.deliver("a chord to settle", SETTLE, |reply| Command::Key {
            chords: vec![
                "a".to_owned(),
                "ctrl+shift+t".to_owned(),
                "Return".to_owned(),
            ],
            focus: None,
            reply,
        });

        let input = harness.input(&mut client);
        let keys: Vec<_> = input
            .iter()
            .filter_map(|event| match event {
                Input::Key { name, pressed } => Some((name.as_str(), *pressed)),
                _ => None,
            })
            .collect();

        assert_eq!(
            keys,
            vec![
                ("a", true),
                ("a", false),
                // Modifiers nest around the key, outermost first, and `t`
                // resolves to `T` because shift is held when it arrives.
                ("Control_L", true),
                ("Shift_L", true),
                ("T", true),
                ("T", false),
                ("Shift_L", false),
                ("Control_L", false),
                ("Return", true),
                ("Return", false),
            ],
            "the client should see exactly the chords that were asked for, in order",
        );

        harness.stop(client);
    });
}

#[test]
fn typed_text_arrives_a_character_at_a_time() {
    fixture::guarded("input-type", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.deliver("typing to settle", SETTLE, |reply| Command::Write {
            text: "Hi!".to_owned(),
            focus: None,
            reply,
        });

        let pressed: Vec<_> = harness
            .input(&mut client)
            .into_iter()
            .filter_map(|event| match event {
                Input::Key {
                    name,
                    pressed: true,
                } => Some(name),
                _ => None,
            })
            .collect();

        assert_eq!(
            pressed,
            vec!["Shift_L", "H", "i", "Shift_L", "exclam"],
            "each character should be typed with exactly the modifiers it needs",
        );

        harness.stop(client);
    });
}

#[test]
fn a_character_the_layout_cannot_type_fails_before_anything_is_sent() {
    fixture::guarded("input-unmappable", OUTPUT, |harness| {
        let mut client = ready(harness);

        let outcome = harness.deliver("an unmappable string", SETTLE, |reply| Command::Write {
            // The `a` is typeable and the ideograph is not. If the failure came
            // halfway through, the client would have received the `a`.
            text: "a漢".to_owned(),
            focus: None,
            reply,
        });

        assert!(
            matches!(outcome, Some(Err(Failure::UnmappableChord { .. }))),
            "an unproducible character must be refused, not approximated: {outcome:?}",
        );
        assert_eq!(
            harness.input(&mut client),
            Vec::new(),
            "nothing should have been typed: half a string is worse than none",
        );

        harness.stop(client);
    });
}

#[test]
fn focusing_first_clicks_the_point_before_typing() {
    // `--focus-on` resolves a selector to a point and this is what happens to
    // it. Keyboard focus *within* a window is the toolkit's own state, not the
    // seat's, so the lever is a click — and the plan left "is a click actually
    // the right lever" open until it could be watched against a real client.
    //
    // Order is the whole assertion. A click delivered after the keys would still
    // produce every event this test looks for and would focus the widget just in
    // time to miss the text, which is a bug no count of events can see.
    fixture::guarded("input-focus-on", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.deliver("typing with a focus point", SETTLE, |reply| {
            Command::Write {
                text: "hi".to_owned(),
                focus: Some(Point { x: 40, y: 30 }),
                reply,
            }
        });

        let input = harness.input(&mut client);
        let first_key = input
            .iter()
            .position(|event| matches!(event, Input::Key { .. }))
            .expect("the text should have been typed");
        let last_button = input
            .iter()
            .rposition(|event| matches!(event, Input::Button { code: 0x110, .. }))
            .expect("focusing should have clicked");
        assert!(
            last_button < first_key,
            "the focus click has to land before the text, not after it: {input:?}",
        );
        assert!(
            input
                .iter()
                .take(last_button)
                .any(|event| matches!(event, Input::Motion { x: 40, y: 30 })),
            "the focus click should have moved the pointer there first: {input:?}",
        );

        harness.stop(client);
    });
}

#[test]
fn a_chord_that_does_not_parse_leaves_the_focus_alone() {
    // The all-or-nothing rule, extended to the click `--focus-on` adds. A
    // rejected chord must not move the application's focus on its way out:
    // half-performed is worse than not performed, and a caller who mistyped a
    // chord would otherwise have to undo a click they never asked for.
    fixture::guarded("input-focus-rejected", OUTPUT, |harness| {
        let mut client = ready(harness);

        let reply = harness.deliver("an unmappable chord", SETTLE, |reply| Command::Key {
            chords: vec!["ctrl+nonesuch".to_owned()],
            focus: Some(Point { x: 40, y: 30 }),
            reply,
        });
        assert!(
            matches!(reply, Some(Err(Failure::UnmappableChord { .. }))),
            "{reply:?}",
        );

        let input = harness.input(&mut client);
        assert!(
            !input
                .iter()
                .any(|event| matches!(event, Input::Button { .. } | Input::Key { .. })),
            "nothing should have been delivered at all: {input:?}",
        );

        harness.stop(client);
    });
}

#[test]
fn a_click_arrives_where_it_was_aimed() {
    fixture::guarded("input-click", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.deliver("a click to settle", SETTLE, |reply| Command::Click {
            at: Some(Point { x: 40, y: 30 }),
            button: Button::Left,
            target: Target::Focused,
            reply,
        });

        let input = harness.input(&mut client);
        // The pointer is already inside the window — it was parked at the
        // centre when the window mapped — so aiming somewhere else is a motion
        // rather than another enter.
        assert!(
            input
                .iter()
                .any(|event| matches!(event, Input::Motion { x: 40, y: 30 })),
            "the pointer should have moved to the requested point: {input:?}",
        );
        assert_eq!(
            input
                .iter()
                .filter(|event| matches!(event, Input::Button { code: 0x110, .. }))
                .count(),
            2,
            "a click is a press and a release of the left button: {input:?}",
        );

        harness.stop(client);
    });
}

#[test]
fn a_scroll_arrives_as_discrete_steps() {
    fixture::guarded("input-scroll", OUTPUT, |harness| {
        let mut client = ready(harness);

        harness.deliver("a scroll to settle", SETTLE, |reply| Command::Scroll {
            dx: 0,
            dy: 2,
            reply,
        });

        let input = harness.input(&mut client);
        assert!(
            input.contains(&Input::Scroll {
                horizontal: 0,
                vertical: 240,
            }),
            "two wheel clicks down should arrive as 240 v120 steps: {input:?}",
        );

        harness.stop(client);
    });
}

#[test]
fn a_verb_returns_once_the_client_has_drawn() {
    fixture::guarded("input-settles", OUTPUT, |harness| {
        let mut client = ready(harness);

        // Deliver first, with no paint behind it, and confirm it does not
        // answer: without that, the success below could just be a reply that
        // never waited for anything.
        let unanswered = harness.deliver("a key with no repaint", SETTLE, |reply| Command::Key {
            chords: vec!["a".to_owned()],
            focus: None,
            reply,
        });
        assert!(
            unanswered.is_none(),
            "a client that drew nothing must not settle the call: {unanswered:?}",
        );

        // The compositor kept ticking throughout, so nothing above was a stall.
        let ticks = harness.ticks();
        assert!(
            ticks > 0,
            "the loop should have been running the whole time"
        );

        // Now park a reply and paint, which is the frame it is waiting for.
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        harness
            .commands()
            .send(Command::Key {
                chords: vec!["b".to_owned()],
                focus: None,
                reply: reply_tx,
            })
            .expect("the compositor is listening");
        harness.paint(&mut client, PAINT);

        // `try_recv` takes the value, so the reply has to be kept as it is
        // seen rather than looked at twice.
        let mut settled = None;
        harness.drive_until("the parked reply to be answered", |_| {
            match reply_rx.try_recv() {
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
                other => {
                    settled = Some(other);
                    true
                }
            }
        });

        assert!(
            matches!(settled, Some(Ok(Ok(())))),
            "a client that drew should settle the call: {settled:?}",
        );

        harness.stop(client);
    });
}

#[test]
fn a_window_that_goes_away_fails_its_parked_replies_at_once() {
    fixture::guarded("input-abandoned", OUTPUT, |harness| {
        let client = ready(harness);

        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        harness
            .commands()
            .send(Command::Key {
                chords: vec!["a".to_owned()],
                focus: None,
                reply: reply_tx,
            })
            .expect("the compositor is listening");
        harness.drive_until("the reply to be parked", |state| state.settling() == 1);

        // The client leaves without ever drawing again. The frame the reply is
        // waiting for has become impossible, so it must fail now rather than on
        // a deadline — that is the whole point of tying the sender's lifetime to
        // the condition.
        harness.stop(client);
        harness.drive_until("the parked reply to be released", |state| {
            state.settling() == 0
        });

        assert!(
            matches!(reply_rx.try_recv(), Ok(Err(Failure::ClientGone))),
            "a departed window must fail its parked replies, not leave them waiting",
        );
    });
}

#[test]
fn input_with_no_window_is_refused_rather_than_parked() {
    fixture::guarded("input-no-window", OUTPUT, |harness| {
        // Nothing has ever connected, so there is nothing to settle on. Parking
        // here would be failure mode B: waiting for something that can never
        // happen.
        let outcome = harness.deliver("a key with no window", SETTLE, |reply| Command::Key {
            chords: vec!["a".to_owned()],
            focus: None,
            reply,
        });

        assert!(
            matches!(outcome, Some(Err(Failure::NoSuchWindow))),
            "input with no window must fail immediately: {outcome:?}",
        );
    });
}
