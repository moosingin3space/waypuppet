//! Defence 3, tested against a compositor that is really stuck.
//!
//! The unit tests in `bridge.rs` drive [`Heartbeat`] directly, which proves the
//! branch logic but not the assumption underneath it: that a blocked calloop
//! loop actually stops beating. This wedges a real compositor thread with
//! `Command::Stall` and checks that a call made during the wedge is blamed on
//! waypuppet rather than on the application.
//!
//! Worth the trouble because the failure mode is silent. A `diagnose` that
//! always answered `TimedOut` would look completely fine — every call would
//! still fail correctly, just with the wrong culprit named, and no assertion
//! anywhere else would notice.
//!
//! The mirror case — a *healthy* loop whose call times out anyway — is not
//! here, and deliberately. Every command a real compositor can answer, it
//! answers in microseconds, so there is no way to make one time out without
//! also blocking the loop, which is the other case. An integration test written
//! for it passed against a deliberately inverted `diagnose`, which makes it
//! worse than no test: it looks like coverage. That branch is covered properly
//! by `a_healthy_loop_is_reported_as_a_plain_timeout` in `bridge.rs`, where the
//! heartbeat can be beaten without a compositor attached to it.
//!
//! [`Heartbeat`]: waypuppet_compositor::Heartbeat

use std::{sync::mpsc, thread, time::Duration};

use waypuppet_compositor::{Command, Commands, Compositor, Config};
use waypuppet_proto::{Failure, Size};

const OUTPUT: Size = Size {
    width: 160,
    height: 120,
};

/// How long to wedge the loop for. Comfortably longer than [`DEADLINE`], so the
/// call under test expires while the loop is still stuck.
const STALL: Duration = Duration::from_millis(1500);

/// The deadline for the call made during the stall.
const DEADLINE: Duration = Duration::from_millis(200);

#[test]
fn a_call_during_a_stall_blames_the_compositor() {
    let (commands, compositor) = start();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async {
        // Prove the loop is healthy first, so a failure below cannot be blamed
        // on the session never having worked.
        commands
            .ask(Duration::from_secs(10), Command::Status)
            .await
            .expect("a healthy session answers");

        // Wedge it. Fire and forget: there is no reply, and asking for one
        // would just be waiting on the stall we asked for.
        commands
            .send(Command::Stall(STALL))
            .expect("the compositor is listening");

        // Give the loop a moment to pick the command up and go under.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let failure = commands
            .ask(DEADLINE, Command::Status)
            .await
            .expect_err("the loop is blocked, so nothing can answer");

        match failure {
            Failure::LoopStalled { millis } => {
                assert!(
                    millis > 0,
                    "a stall of zero milliseconds is not a useful diagnosis",
                );
            }
            other => panic!(
                "a wedged loop must be reported as stalled, not as {other:?} — \
                 otherwise the caller is told to debug their application",
            ),
        }

        // And it recovers: once the stall ends the same call succeeds, so this
        // reports a stuck loop rather than a dead one.
        tokio::time::sleep(STALL).await;
        commands
            .ask(Duration::from_secs(10), Command::Status)
            .await
            .expect("the session answers again once the stall is over");

        let _ = commands.send(Command::Stop);
    });

    compositor.join().expect("the compositor thread finishes");
}

/// Run a compositor on its own thread and hand back its command channel.
fn start() -> (Commands, thread::JoinHandle<()>) {
    let (ready_tx, ready_rx) = mpsc::channel();

    let compositor = thread::spawn(move || {
        let mut compositor = Compositor::new(&Config {
            output: OUTPUT,
            ..Config::default()
        })
        .expect("compositor starts");
        ready_tx
            .send(compositor.commands())
            .expect("the test is still listening");
        compositor.run().expect("the compositor runs");
    });

    let commands = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the compositor reports ready");
    (commands, compositor)
}
