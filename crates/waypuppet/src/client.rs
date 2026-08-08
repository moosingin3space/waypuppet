//! Talking to a session's control socket.
//!
//! Every call sets a deadline. There is no unbounded wait anywhere in the CLI,
//! because a verb that hangs is worse than one that fails: the failure names
//! what went wrong, and a session that has stalled says so rather than looking
//! slow.

use std::{path::Path, time::Duration};

use tarpc::{client, context, tokio_serde::formats::Bincode};
use waypuppet_proto::WaypuppetClient;

/// The default deadline for a call.
///
/// Software rendering is several times slower than a GPU, so this is tuned for
/// the slow case rather than the fast one.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// Connect to a session's control socket.
///
/// # Errors
///
/// Returns a message naming the socket if nothing is listening on it, which is
/// almost always "no session by that name".
pub async fn connect(socket: &Path) -> Result<WaypuppetClient, String> {
    let transport = tarpc::serde_transport::unix::connect(socket, Bincode::default)
        .await
        .map_err(|error| format!("no session at {}: {error}", socket.display()))?;
    Ok(WaypuppetClient::new(client::Config::default(), transport).spawn())
}

/// A call context carrying `deadline`.
#[must_use]
pub fn deadline(deadline: Duration) -> context::Context {
    let mut context = context::current();
    context.deadline = std::time::Instant::now() + deadline;
    context
}

/// Wait until a socket accepts a connection, or give up.
///
/// Used after starting a detached session: the process is up before the socket
/// is, and polling for it beats sleeping a guessed interval.
///
/// # Errors
///
/// Returns a message if the socket never came up within `limit`.
pub async fn wait_for_socket(socket: &Path, limit: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(format!(
        "the session did not start listening on {} within {limit:?}",
        socket.display()
    ))
}
