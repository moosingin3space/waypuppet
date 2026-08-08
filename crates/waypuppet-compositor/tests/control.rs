//! Phase 03's acceptance test: the control socket answers.
//!
//! Exercises the whole control path — tarpc over a Unix socket, the bridge to
//! the compositor thread, and back — rather than calling into the compositor
//! directly. The bridge is the part that can deadlock, so a test that does not
//! cross it would not be testing the interesting half.

use std::{path::Path, time::Duration};

use tarpc::{client, context, tokio_serde::formats::Bincode};
use waypuppet_compositor::{Config, session};
use waypuppet_proto::{ClientState, Size, Target, WaypuppetClient};

const OUTPUT: Size = Size {
    width: 200,
    height: 150,
};

/// Long enough for software rendering, short enough to fail while somebody is
/// watching.
const LIMIT: Duration = Duration::from_secs(20);

#[test]
fn the_control_socket_answers_and_shuts_down() {
    let directory = tempfile::tempdir().expect("temp dir");
    let socket = directory.path().join("control.sock");
    let captures = directory.path().to_path_buf();

    let server = std::thread::spawn({
        let socket = socket.clone();
        move || {
            let runtime = runtime();
            runtime.block_on(session::serve(
                &Config {
                    output: OUTPUT,
                    capture_dir: captures,
                    ..Config::default()
                },
                &socket,
                &[],
                false,
            ))
        }
    });

    runtime().block_on(async {
        wait_for(&socket).await;
        let session = connect(&socket).await;

        let status = session
            .status(deadline())
            .await
            .expect("the call reached the session")
            .expect("status succeeds");
        assert_eq!(status.output, OUTPUT);
        assert!(status.windows.is_empty(), "nothing has connected yet");
        assert_eq!(
            status.client,
            ClientState::Running,
            "a session with no child of its own is not waiting for one",
        );

        // Capture through the bridge: the compositor renders on its own thread
        // and the PNG is written here, on the control thread.
        let capture = session
            .shot(deadline(), Target::Output, None)
            .await
            .expect("the call reached the session")
            .expect("shot succeeds");
        assert_eq!(capture.size, OUTPUT);
        assert!(
            capture.path.metadata().is_ok_and(|meta| meta.len() > 0),
            "the capture should exist on disk at the path it reported",
        );

        // Logs are answerable even with no child; the cursor should not move.
        let logs = session
            .logs(deadline(), waypuppet_proto::Cursor::default())
            .await
            .expect("the call reached the session")
            .expect("logs succeed");
        assert!(logs.stdout.is_empty() && logs.stderr.is_empty());

        session
            .kill(deadline())
            .await
            .expect("the call reached the session")
            .expect("kill succeeds");
    });

    server
        .join()
        .expect("the session thread finishes")
        .expect("the session shuts down cleanly");

    assert!(
        !socket.exists(),
        "the socket should be removed on shutdown, so it cannot be mistaken for a live session",
    );
}

/// A current-thread runtime, which is all either side needs.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// A call context with a bounded deadline — as everything here has.
fn deadline() -> context::Context {
    let mut context = context::current();
    context.deadline = std::time::Instant::now() + LIMIT;
    context
}

/// Block until the session is listening, or fail with something readable.
async fn wait_for(socket: &Path) {
    let deadline = tokio::time::Instant::now() + LIMIT;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the session never listened on {}", socket.display());
}

/// Connect a typed client to the session.
async fn connect(socket: &Path) -> WaypuppetClient {
    let transport = tarpc::serde_transport::unix::connect(socket, Bincode::default)
        .await
        .expect("the session accepts connections");
    WaypuppetClient::new(client::Config::default(), transport).spawn()
}
