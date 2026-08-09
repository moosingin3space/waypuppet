//! Phase 05's acceptance tests: every condition holds when it should, and
//! times out cleanly when it should not.
//!
//! Each variant is checked twice, and the two halves fail for different
//! reasons. **The true case** proves the registry is woken from wherever the
//! fact it names can change — which is the part that is easy to get subtly
//! wrong, because a wait that resolves one tick late still looks like it works.
//! **The false case** proves that not happening is reported as
//! [`Settled::TimedOut`] rather than as a hang, and it is only meaningful
//! because [`fixture::Harness::deliver`] ticks the compositor throughout: a
//! `None` from it is a statement about the condition, never about the loop.
//!
//! Three tests here are written to fail if a defence is removed rather than
//! only to pass while it is present, which is what the project asks of anything
//! whose failure mode is a wrong answer instead of a crash:
//!
//! - `a_condition_that_already_holds_is_answered_at_once` breaks if the check
//!   in `park_wait` goes, because nothing else will fire.
//! - `a_renamed_window_is_noticed_without_being_redrawn` breaks if the
//!   `title_changed` hook goes, because the client never commits again.
//! - `a_window_that_goes_away_fails_a_parked_frame_wait_at_once` asserts it
//!   fails *quickly* against a long limit, so it cannot pass by timing out.

mod fixture;

use std::time::Duration;

use fixture::control::Serving;
use waypuppet_compositor::{Command, Watched};
use waypuppet_proto::{Condition, Failure, Settled, Size};

const OUTPUT: Size = Size {
    width: 320,
    height: 240,
};

const PAINT: [u8; 4] = [0x30, 0x60, 0x90, 0xFF];

/// How long to give a condition before calling it unmet.
///
/// Short: everything under test here is answered the moment the compositor
/// learns of it, so a longer limit only buys slower failures.
const LIMIT: Duration = Duration::from_millis(500);

/// How long a condition that must *not* be met is given to prove it.
const NEVER: Duration = Duration::from_millis(300);

/// Bring a client up with a mapped, painted window.
fn mapped(harness: &mut fixture::Harness, title: &str) -> fixture::ClientThread {
    let mut client = harness.start_client(title);
    harness.configure(&mut client);
    harness.paint(&mut client, PAINT);
    harness.drive_until("the client's first frame", |state| {
        state.records.values().any(|record| record.mapped)
    });
    client
}

/// Park a wait without blocking on it, and hand back its receiver.
///
/// The verbs that settle can be driven with `deliver`, which waits. A `wait`
/// often has to be parked *before* the thing that satisfies it happens, which
/// is exactly the ordering that would deadlock if the test blocked here.
fn park(
    harness: &fixture::Harness,
    condition: Watched,
) -> tokio::sync::oneshot::Receiver<Result<(), Failure>> {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    harness
        .commands()
        .send(Command::Wait { condition, reply })
        .expect("the compositor is listening");
    receiver
}

/// Tick until a parked wait is answered, and hand back what it said.
///
/// # Panics
///
/// Panics if nothing answers within [`fixture::WAIT_LIMIT`].
fn answered(
    harness: &mut fixture::Harness,
    what: &str,
    receiver: &mut tokio::sync::oneshot::Receiver<Result<(), Failure>>,
) -> Result<(), Failure> {
    let mut answer = None;
    harness.drive_until(what, |_| match receiver.try_recv() {
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
        Ok(reply) => {
            answer = Some(reply);
            true
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            answer = Some(Err(Failure::ShuttingDown));
            true
        }
    });
    answer.expect("drive_until only returns once the answer is set")
}

// ----------------------------------------------------------------- windows --

#[test]
fn waiting_for_a_window_is_answered_when_one_maps() {
    fixture::guarded("wait-windows", OUTPUT, |harness| {
        // Parked before the client exists at all, which is the ordering a
        // caller gets from `run --detach` followed immediately by `wait`.
        let mut parked = park(harness, Watched::Windows(1));

        let mut client = harness.start_client("wait-windows");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);

        assert_eq!(
            answered(harness, "the window to map", &mut parked),
            Ok(()),
            "a window mapping is what this condition is about",
        );

        harness.stop(client);
    });
}

#[test]
fn a_condition_that_already_holds_is_answered_at_once() {
    fixture::guarded("wait-already-true", OUTPUT, |harness| {
        let client = mapped(harness, "wait-already-true");

        // The window mapped before the request arrived — the other side of the
        // race above, and the one that decides whether `wait` is dependable or
        // a coin flip. Nothing further will happen to this window, so only the
        // check at park time can answer this.
        let outcome = harness.deliver("a window that is already there", LIMIT, |reply| {
            Command::Wait {
                condition: Watched::Windows(1),
                reply,
            }
        });

        assert_eq!(
            outcome,
            Some(Ok(())),
            "a condition that is already true must not wait for it to change again",
        );

        harness.stop(client);
    });
}

#[test]
fn waiting_for_more_windows_than_exist_times_out_cleanly() {
    fixture::guarded("wait-windows-timeout", OUTPUT, |harness| {
        let client = mapped(harness, "wait-windows-timeout");
        let before = harness.ticks();

        let outcome = harness.deliver("a second window that never comes", NEVER, |reply| {
            Command::Wait {
                condition: Watched::Windows(2),
                reply,
            }
        });

        assert_eq!(
            outcome, None,
            "one window is not two, and the wait must not claim otherwise",
        );
        // Without this the assertion above would also pass against a compositor
        // that had stopped entirely, which is the failure `wait` most needs to
        // be distinguishable from.
        assert!(
            harness.ticks() > before,
            "the loop should have been running the whole time",
        );

        harness.stop(client);
    });
}

#[test]
fn waiting_for_a_window_is_reliable_however_the_race_falls() {
    fixture::guarded("wait-windows-race", OUTPUT, |harness| {
        // The plan asks for this under repetition, because the failure it is
        // guarding against is order-dependent rather than rare: park-then-map
        // is answered by the commit hook, map-then-park by the check in
        // `park_wait`, and a version missing either one passes half the time.
        // Alternating is a stronger test than randomising — both orderings get
        // the same number of goes.
        for round in 0..200 {
            let park_first = round % 2 == 0;

            let mut parked = park_first.then(|| park(harness, Watched::Windows(1)));
            let mut client = harness.start_client("wait-windows-race");
            harness.configure(&mut client);
            harness.paint(&mut client, PAINT);
            harness.drive_until("the window to map", |state| {
                state.records.values().any(|record| record.mapped)
            });

            let outcome = match parked.as_mut() {
                Some(receiver) => answered(harness, "the parked wait", receiver),
                None => harness
                    .deliver("a wait after the fact", LIMIT, |reply| Command::Wait {
                        condition: Watched::Windows(1),
                        reply,
                    })
                    .expect("a window is already mapped, so this cannot time out"),
            };
            assert_eq!(outcome, Ok(()), "round {round} (park first: {park_first})");

            // Back to no windows, so the next round races from the same place.
            harness.stop(client);
            harness.drive_until("the window to go away", |state| state.records.is_empty());
        }
    });
}

// ------------------------------------------------------------------- title --

#[test]
fn waiting_for_a_title_is_answered_when_a_window_matches() {
    fixture::guarded("wait-title", OUTPUT, |harness| {
        let mut parked = park(harness, Watched::Title("^wait-t.tle$".to_owned()));

        let mut client = harness.start_client("wait-title");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);

        assert_eq!(
            answered(harness, "a matching title", &mut parked),
            Ok(()),
            "the pattern should have matched the window that mapped",
        );

        harness.stop(client);
    });
}

#[test]
fn a_renamed_window_is_noticed_without_being_redrawn() {
    fixture::guarded("wait-title-renamed", OUTPUT, |harness| {
        let mut client = mapped(harness, "before");
        let mut parked = park(harness, Watched::Title("^after$".to_owned()));

        // A rename and nothing else: `xdg_toplevel.set_title` is not double
        // buffered, so there is no commit behind this to notice it. A registry
        // woken only by frames would leave this parked forever.
        harness.retitle(&mut client, "after");

        assert_eq!(
            answered(harness, "the renamed window", &mut parked),
            Ok(()),
            "a title is an event in its own right, not something a frame carries",
        );

        harness.stop(client);
    });
}

#[test]
fn waiting_for_a_title_nothing_has_times_out_cleanly() {
    fixture::guarded("wait-title-timeout", OUTPUT, |harness| {
        let client = mapped(harness, "wait-title-timeout");
        let before = harness.ticks();

        let outcome = harness.deliver("a title nothing has", NEVER, |reply| Command::Wait {
            condition: Watched::Title("^nothing here$".to_owned()),
            reply,
        });

        assert_eq!(outcome, None, "no window has that title");
        assert!(
            harness.ticks() > before,
            "the loop should have kept running"
        );

        harness.stop(client);
    });
}

#[test]
fn a_title_that_matches_an_unmapped_window_does_not_count() {
    fixture::guarded("wait-title-unmapped", OUTPUT, |harness| {
        // Connected and titled, but it has never committed a buffer. A title on
        // a window nobody can see is not what a caller waiting for one means,
        // and answering here would hand back a session with nothing on screen.
        let mut client = harness.start_client("wait-title-unmapped");
        harness.configure(&mut client);
        harness.drive_until("the toplevel to be recorded", |state| {
            !state.records.is_empty()
        });

        let outcome = harness.deliver("a title on an unmapped window", NEVER, |reply| {
            Command::Wait {
                condition: Watched::Title("^wait-title-unmapped$".to_owned()),
                reply,
            }
        });
        assert_eq!(
            outcome, None,
            "a window with no content must not satisfy a title wait",
        );

        // And it does once the window has drawn, so the assertion above is
        // about being mapped rather than about the pattern being wrong.
        harness.paint(&mut client, PAINT);
        let outcome = harness.deliver("the same title once mapped", LIMIT, |reply| Command::Wait {
            condition: Watched::Title("^wait-title-unmapped$".to_owned()),
            reply,
        });
        assert_eq!(outcome, Some(Ok(())), "the same pattern, now with content");

        harness.stop(client);
    });
}

#[test]
fn a_pattern_that_does_not_compile_is_refused_at_once() {
    fixture::guarded("wait-title-bad-pattern", OUTPUT, |harness| {
        let client = mapped(harness, "wait-title-bad-pattern");

        let outcome = harness.deliver("an unclosed group", LIMIT, |reply| Command::Wait {
            condition: Watched::Title("(unclosed".to_owned()),
            reply,
        });

        assert!(
            matches!(outcome, Some(Err(Failure::BadPattern { .. }))),
            "a pattern that will not compile must be reported, not silently \
             matched against nothing until the timeout: {outcome:?}",
        );

        harness.stop(client);
    });
}

// ------------------------------------------------------------------ frames --

#[test]
fn waiting_for_frames_counts_from_the_request() {
    fixture::guarded("wait-frames", OUTPUT, |harness| {
        let mut client = mapped(harness, "wait-frames");

        let mut parked = park(harness, Watched::Frames(2));

        // One frame is not two. Driving the loop here is what makes this
        // meaningful: the compositor had every chance to answer early.
        harness.paint(&mut client, PAINT);
        harness.drive_until("the first frame to land", |state| state.waiting() == 1);
        assert_eq!(
            parked.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "one frame must not answer a wait for two",
        );

        harness.paint(&mut client, PAINT);
        assert_eq!(
            answered(harness, "the second frame", &mut parked),
            Ok(()),
            "two frames after the request is what was asked for",
        );

        harness.stop(client);
    });
}

#[test]
fn waiting_for_frames_nobody_draws_times_out_cleanly() {
    fixture::guarded("wait-frames-timeout", OUTPUT, |harness| {
        // The client is up and mapped; it simply never draws again. That is a
        // fact about the application, and the honest answer is a timeout.
        let client = mapped(harness, "wait-frames-timeout");
        let before = harness.ticks();

        let outcome = harness.deliver("a frame that never comes", NEVER, |reply| Command::Wait {
            condition: Watched::Frames(1),
            reply,
        });

        assert_eq!(outcome, None, "nothing drew, so nothing should be claimed");
        assert!(
            harness.ticks() > before,
            "the loop should have kept running"
        );

        harness.stop(client);
    });
}

#[test]
fn waiting_for_frames_with_no_window_is_refused_rather_than_parked() {
    fixture::guarded("wait-frames-no-window", OUTPUT, |harness| {
        // Nothing has ever connected, so there is no window whose frames could
        // be counted. Parking here would be failure mode B: waiting for
        // something that can never happen, and reporting it as an ordinary
        // timeout when the deadline came.
        let outcome = harness.deliver("frames from nothing", LIMIT, |reply| Command::Wait {
            condition: Watched::Frames(1),
            reply,
        });

        assert_eq!(
            outcome,
            Some(Err(Failure::NoSuchWindow)),
            "a frame count with no window to count must fail immediately",
        );
    });
}

#[test]
fn a_window_that_goes_away_fails_a_parked_frame_wait_at_once() {
    fixture::guarded("wait-frames-abandoned", OUTPUT, |harness| {
        let client = mapped(harness, "wait-frames-abandoned");

        let mut parked = park(harness, Watched::Frames(5));
        harness.drive_until("the wait to be parked", |state| state.waiting() == 1);

        // The client leaves without drawing again. Those five frames have
        // become impossible, so the call must fail now rather than on its
        // deadline — and explicitly, because a dropped sender arrives as
        // `ShuttingDown` and would blame the session for a window that closed.
        harness.stop(client);

        assert_eq!(
            answered(harness, "the abandoned wait", &mut parked),
            Err(Failure::ClientGone),
            "a departed window must fail the waits counting its frames",
        );
    });
}

#[test]
fn a_window_going_away_leaves_other_conditions_parked() {
    fixture::guarded("wait-frames-abandon-scope", OUTPUT, |harness| {
        let client = mapped(harness, "first");

        // A title no window here has — so it stays parked — but which another
        // window could perfectly well arrive with. Unlike a frame count, this
        // names no surface, so a window leaving does not make it impossible and
        // failing it alongside the frame waits would answer a question nobody
        // asked.
        let mut parked = park(harness, Watched::Title("^second$".to_owned()));
        harness.drive_until("the wait to be parked", |state| state.waiting() == 1);

        harness.stop(client);
        harness.drive_until("the window to go away", |state| state.records.is_empty());
        assert_eq!(
            parked.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "one window closing does not make another one impossible",
        );

        let mut client = harness.start_client("second");
        harness.configure(&mut client);
        harness.paint(&mut client, PAINT);
        assert_eq!(
            answered(harness, "the next window", &mut parked),
            Ok(()),
            "the wait should still have been there for the window that came",
        );

        harness.stop(client);
    });
}

// -------------------------------------------------------------------- idle --

#[test]
fn waiting_for_idle_is_answered_once_nothing_is_drawing() {
    fixture::guarded("wait-idle", OUTPUT, |harness| {
        let client = mapped(harness, "wait-idle");

        // Nothing draws unless this test tells it to, so the session goes quiet
        // immediately. Note what this needs that no other condition does: going
        // quiet is the *absence* of commits, so only the loop can notice it.
        let outcome = harness.deliver("the session to go quiet", LIMIT, |reply| Command::Wait {
            condition: Watched::Idle(Duration::from_millis(50)),
            reply,
        });

        assert_eq!(
            outcome,
            Some(Ok(())),
            "a client that is not drawing is idle, and only the loop can say so",
        );

        harness.stop(client);
    });
}

#[test]
fn waiting_for_more_quiet_than_there_is_time_for_times_out_cleanly() {
    fixture::guarded("wait-idle-timeout", OUTPUT, |harness| {
        let client = mapped(harness, "wait-idle-timeout");
        let before = harness.ticks();

        // Asking for far more quiet than the wait itself allows. This is the
        // shape of the trap the condition carries in practice — an application
        // that repaints on a timer is never idle for long enough — with the
        // client's behaviour replaced by arithmetic so the test is exact.
        let outcome = harness.deliver("thirty seconds of quiet", NEVER, |reply| Command::Wait {
            condition: Watched::Idle(Duration::from_secs(30)),
            reply,
        });

        assert_eq!(
            outcome, None,
            "there has not been thirty seconds of anything"
        );
        assert!(
            harness.ticks() > before,
            "the loop should have kept running"
        );

        harness.stop(client);
    });
}

// -------------------------------------------------------------------- exit --

// `Exit` is the one condition the compositor cannot see: a process may exit
// without ever having connected, so there is no Wayland event behind it. These
// go through a whole session, which is also the only end-to-end coverage of
// `wait` over the wire.

#[test]
fn waiting_for_exit_is_answered_when_the_child_goes() {
    let serving = Serving::start(&["/bin/true"]);
    let settled = runtime().block_on(async {
        let session = serving.connect().await;
        let settled = session
            .wait(
                fixture::control::deadline(),
                Condition::Exit,
                fixture::control::LIMIT,
            )
            .await
            .expect("the call reached the session")
            .expect("waiting for an exit is not itself an error");
        session
            .kill(fixture::control::deadline())
            .await
            .expect("the call reached the session")
            .expect("kill succeeds");
        settled
    });

    assert_eq!(
        settled,
        Settled::Met,
        "a child that has exited satisfies the condition, whether it exited \
         before or after the request arrived",
    );
    serving.join();
}

#[test]
fn waiting_for_exit_times_out_while_the_child_runs() {
    let serving = Serving::start(&["/bin/sleep", "60"]);
    let settled = runtime().block_on(async {
        let session = serving.connect().await;
        let settled = session
            .wait(
                fixture::control::deadline(),
                Condition::Exit,
                Duration::from_millis(300),
            )
            .await
            .expect("the call reached the session")
            .expect("a running child is not an error");
        session
            .kill(fixture::control::deadline())
            .await
            .expect("the call reached the session")
            .expect("kill succeeds");
        settled
    });

    assert_eq!(
        settled,
        Settled::TimedOut,
        "the child is still running, and saying so is the whole point of \
         `Settled` being a value rather than an error",
    );
    serving.join();
}

#[test]
fn waiting_for_the_exit_of_nothing_is_refused_rather_than_parked() {
    let serving = Serving::start(&[]);
    let failure = runtime().block_on(async {
        let session = serving.connect().await;
        let failure = session
            .wait(
                fixture::control::deadline(),
                Condition::Exit,
                fixture::control::LIMIT,
            )
            .await
            .expect("the call reached the session")
            .expect_err("nothing can exit here");
        session
            .kill(fixture::control::deadline())
            .await
            .expect("the call reached the session")
            .expect("kill succeeds");
        failure
    });

    assert_eq!(
        failure,
        Failure::NoClient,
        "this session never spawned anything, so the condition can never hold \
         and must not sit on its timeout pretending otherwise — nor claim a \
         client died, which is what `ClientGone` would have said",
    );
    serving.join();
}

#[test]
fn a_compositor_condition_answers_over_the_wire_too() {
    // The three tests above route to the control thread. This one routes to the
    // compositor and back over the same socket, which is the only place the
    // split in `Watching` is exercised end to end.
    let serving = Serving::start(&[]);
    let settled = runtime().block_on(async {
        let session = serving.connect().await;
        let settled = session
            .wait(
                fixture::control::deadline(),
                Condition::Windows(1),
                Duration::from_millis(300),
            )
            .await
            .expect("the call reached the session")
            .expect("an unmet condition is not an error");
        session
            .kill(fixture::control::deadline())
            .await
            .expect("the call reached the session")
            .expect("kill succeeds");
        settled
    });

    assert_eq!(
        settled,
        Settled::TimedOut,
        "nothing ever connected, so no window mapped",
    );
    serving.join();
}

/// A current-thread runtime, which is all a test client needs.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}
