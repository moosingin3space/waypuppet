//! The accessibility bus, and the registry waypuppet becomes on it.
//!
//! A capture says what a window looks like. It does not say what is in it, and
//! coordinates harvested off a screenshot are the most brittle thing a GUI test
//! can contain. AT-SPI is the semantic view — roles, names, bounds — and
//! toolkits built on [AccessKit] publish it without being asked.
//!
//! # Why waypuppet has to provide the whole thing
//!
//! AT-SPI needs two things that a headless container has neither of: a D-Bus
//! session, and an accessibility *registry* for applications to register
//! against. Using the developer's own session bus is the wrong answer even
//! where there is one — it breaks isolation, it fails in CI, and it announces
//! the application under test to the user's real desktop.
//!
//! So waypuppet provides both. The bus comes from [`Provider`]. The registry is
//! waypuppet itself: it owns the two names an application looks for, and the
//! handshake hands over everything the tree needs.
//!
//! # What an application actually looks up
//!
//! Established by reading `accesskit_unix` and then watching it happen — see
//! plan 07. In order:
//!
//! 1. `org.a11y.Status.IsEnabled`, a property on `org.a11y.Bus` at
//!    `/org/a11y/bus`. **This is the gate.** Until it reads true, nothing else
//!    is attempted and no tree is ever published.
//! 2. The accessibility bus address — from `AT_SPI_BUS_ADDRESS` when that is
//!    set, and only otherwise from `org.a11y.Bus.GetAddress`. waypuppet sets
//!    the variable, so one bus does both jobs; there is nothing here for a
//!    second bus to isolate.
//! 3. `org.a11y.atspi.Socket.Embed` on `org.a11y.atspi.Registry`, passing the
//!    application's own bus name and root object path. That argument is the
//!    whole prize: the tree is reachable from it.
//!
//! With no session bus at all, `accesskit_unix` gives up quietly — no panic, no
//! log. That is why an application runs perfectly well under a waypuppet that
//! cannot start a bus, and simply has no tree.
//!
//! # The reply that must be a unique name
//!
//! [`Socket::embed`] returns waypuppet's *unique* connection name, never the
//! well-known one. `atspi` decodes that field with `UniqueName::try_from`, so a
//! well-known name fails to deserialise — and the failure is swallowed as
//! recoverable, leaving an application that embedded successfully, published
//! nothing, and logged nothing. It is the one mistake here with no symptom, so
//! `tests/tree.rs` asserts on the tree rather than on the handshake.
//!
//! [AccessKit]: https://accesskit.dev/

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use atspi::{
    CoordType,
    proxy::{accessible::AccessibleProxy, component::ComponentProxy},
};
use waypuppet_proto::{Failure, Node, Point, Rect, Size};
use zbus::{
    Connection, connection, interface,
    zvariant::{ObjectPath, OwnedObjectPath},
};

/// Where an application registered itself: a bus name and a root object path.
type Plug = Arc<Mutex<Option<(String, OwnedObjectPath)>>>;

/// How deep a tree walk will go before deciding it is looping.
///
/// AT-SPI is a graph the application controls, and nothing in the protocol
/// forbids a cycle. A bounded walk turns "waypuppet hung" into a shallow tree,
/// which is the better failure by a wide margin.
const MAX_DEPTH: usize = 64;

/// How long any single D-Bus call may take.
///
/// The application is being asked to introspect itself while it may also be
/// mid-render. Generous, but never unbounded — the standing rule.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Lock, tolerating poisoning.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the session's D-Bus comes from.
///
/// Preference order is deliberate: **a real daemon where there is one, and the
/// embedded broker only where there is not.** `dbus-daemon` is the reference
/// implementation, it is present in most images that can run a GUI application,
/// and using it means waypuppet is not also on the hook for being a correct
/// message bus. `busd` is alpha by its own account, so it is the fallback
/// rather than the default — but it is what makes this work at all inside the
/// Flatpak SDK, which ships no bus of any kind.
enum Provider {
    /// A `dbus-daemon` found on `PATH`, running as a child process.
    Daemon(tokio::process::Child),
    /// `busd`, running in this process.
    ///
    /// The task is aborted on drop, which is what stops the bus.
    Embedded(tokio::task::JoinHandle<()>),
}

impl Drop for Provider {
    fn drop(&mut self) {
        match self {
            // The daemon is ours and nobody else's; a session that has finished
            // has no use for it.
            Self::Daemon(child) => {
                let _ = child.start_kill();
            }
            Self::Embedded(task) => task.abort(),
        }
    }
}

/// The accessibility bus and waypuppet's registry on it.
pub struct Accessibility {
    /// What to put in the child's `DBUS_SESSION_BUS_ADDRESS`.
    address: String,
    /// Where the application registered, once it has.
    plug: Plug,
    /// waypuppet's own connection, which the tree is read over.
    connection: Connection,
    /// Held so the bus outlives the session.
    _provider: Provider,
}

impl std::fmt::Debug for Accessibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accessibility")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Accessibility {
    /// Start a bus and become its accessibility registry.
    ///
    /// # Errors
    ///
    /// Returns a message if no bus could be started or the names could not be
    /// claimed. A session whose accessibility is unusable still serves the
    /// whole protocol — it just says so at startup rather than at the first
    /// `tree`, which is the same bargain [`crate::Renderer`] makes.
    pub async fn start(runtime_dir: &Path) -> Result<Self, String> {
        let (address, provider) = start_bus(runtime_dir).await?;

        let plug: Plug = Arc::default();
        let connection = connection::Builder::address(address.as_str())
            .map_err(|error| format!("{address}: {error}"))?
            .name("org.a11y.Bus")
            .map_err(|error| error.to_string())?
            .name("org.a11y.atspi.Registry")
            .map_err(|error| error.to_string())?
            .serve_at("/org/a11y/bus", Status)
            .map_err(|error| error.to_string())?
            .serve_at("/org/a11y/bus", Bus(address.clone()))
            .map_err(|error| error.to_string())?
            .serve_at("/org/a11y/atspi/accessible/root", Socket(Arc::clone(&plug)))
            .map_err(|error| error.to_string())?
            .build()
            .await
            .map_err(|error| format!("cannot claim the accessibility names: {error}"))?;

        tracing::info!(%address, "accessibility bus up");
        Ok(Self {
            address,
            plug,
            connection,
            _provider: provider,
        })
    }

    /// What the child's `DBUS_SESSION_BUS_ADDRESS` should be.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Whether an application has registered itself yet.
    #[must_use]
    pub fn registered(&self) -> bool {
        lock(&self.plug).is_some()
    }

    /// Read the application's accessibility tree.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoTree`] when nothing has registered, which is the
    /// ordinary answer for a toolkit that does not publish one — and a
    /// different thing from a walk that broke part way, which is
    /// [`Failure::Tree`].
    pub async fn tree(&self) -> Result<Node, Failure> {
        let Some((name, path)) = lock(&self.plug).clone() else {
            return Err(Failure::NoTree);
        };
        walk(&self.connection, &name, path.as_ref(), 0)
            .await
            .map_err(|error| Failure::Tree(error.to_string()))
    }
}

/// Start a bus, preferring a real daemon over the embedded one.
async fn start_bus(runtime_dir: &Path) -> Result<(String, Provider), String> {
    match spawn_daemon(runtime_dir).await {
        Ok(started) => return Ok(started),
        Err(why) => {
            tracing::debug!(%why, "no dbus-daemon; falling back to the embedded bus");
        }
    }
    embedded_bus(runtime_dir).await
}

/// Run `dbus-daemon`, if this image has one.
///
/// A configuration file of waypuppet's own rather than `--session`: the stock
/// session configuration is a separate package from the daemon and is missing
/// from plenty of images that have the binary. Writing three lines is cheaper
/// than depending on somebody else's file being there, and lets the policy say
/// what is true here — one client, everything allowed.
async fn spawn_daemon(runtime_dir: &Path) -> Result<(String, Provider), String> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let config = runtime_dir.join("waypuppet-bus.conf");
    let socket = runtime_dir.join("waypuppet-bus.sock");
    // Removed rather than reused: a stale socket from a crashed session would
    // make the daemon fail to bind, and it has no more claim on the path than a
    // stale control socket does.
    let _ = std::fs::remove_file(&socket);

    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
            socket.display()
        ),
    )
    .map_err(|error| format!("{}: {error}", config.display()))?;

    let mut child = tokio::process::Command::new("dbus-daemon")
        .arg("--nofork")
        .arg("--print-address")
        .arg(format!("--config-file={}", config.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("dbus-daemon: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "dbus-daemon produced no stdout".to_owned())?;

    // The daemon prints its address once it is listening, so this doubles as
    // the readiness signal — no polling, and no sleeping either.
    let mut lines = BufReader::new(stdout).lines();
    let address = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .map_err(|_| "dbus-daemon never printed an address".to_owned())?
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "dbus-daemon exited without printing an address".to_owned())?;

    tracing::debug!(%address, "using dbus-daemon");
    Ok((address.trim().to_owned(), Provider::Daemon(child)))
}

/// Run `busd` in this process.
///
/// The fallback, and the reason the Flatpak SDK can do this at all: it ships no
/// bus of any kind. Alpha by its own account, which is exactly why it is second
/// in line rather than first.
async fn embedded_bus(runtime_dir: &Path) -> Result<(String, Provider), String> {
    let socket = runtime_dir.join("waypuppet-a11y.sock");
    let _ = std::fs::remove_file(&socket);
    let wanted = format!("unix:path={}", socket.display());

    let mut bus = busd::bus::Bus::for_address(Some(&wanted))
        .await
        .map_err(|error| format!("cannot start the embedded bus: {error}"))?;
    // Asked for and given may differ, and the client has to be told the truth.
    let address = bus.address().to_string();

    let task = tokio::spawn(async move {
        if let Err(error) = bus.run().await {
            tracing::warn!(%error, "the embedded bus stopped");
        }
    });

    tracing::debug!(%address, "using the embedded bus");
    Ok((address, Provider::Embedded(task)))
}

// ---------------------------------------------------------------- the tree --

/// Read one node and everything under it.
async fn walk(
    connection: &Connection,
    name: &str,
    path: ObjectPath<'_>,
    depth: usize,
) -> Result<Node, zbus::Error> {
    let accessible = AccessibleProxy::builder(connection)
        .destination(name.to_owned())?
        .path(path.to_owned())?
        .build()
        .await?;

    let role = tokio::time::timeout(CALL_TIMEOUT, accessible.get_role())
        .await
        .map_err(|_| zbus::Error::InputOutput(timed_out("GetRole").into()))??;

    // A name is optional and often absent; failing the whole walk over one
    // would throw away the tree for the sake of a label.
    let label = tokio::time::timeout(CALL_TIMEOUT, accessible.name())
        .await
        .ok()
        .and_then(Result::ok)
        .filter(|label| !label.is_empty());

    let bounds = extents(connection, name, path.as_ref()).await;

    let mut children = Vec::new();
    if depth < MAX_DEPTH {
        let listed = tokio::time::timeout(CALL_TIMEOUT, accessible.get_children())
            .await
            .map_err(|_| zbus::Error::InputOutput(timed_out("GetChildren").into()))??;

        for child in listed {
            // A reference with no name is the protocol's null object, which
            // AT-SPI uses for "there is nothing here" — skipped rather than
            // walked.
            let Some(child_name) = child.name() else {
                continue;
            };
            // A child that cannot be read is dropped rather than fatal: an
            // application is entitled to tear one down mid-walk, and half a
            // tree beats none.
            if let Ok(node) = Box::pin(walk(
                connection,
                child_name.as_str(),
                child.path().as_ref(),
                depth + 1,
            ))
            .await
            {
                children.push(node);
            }
        }
    }

    Ok(Node {
        role: role.name().to_owned(),
        name: label,
        bounds,
        children,
    })
}

/// Where a node sits, if it will say.
///
/// `Component` is optional — plenty of nodes are not on screen at all — so this
/// answers `None` rather than failing.
async fn extents(connection: &Connection, name: &str, path: ObjectPath<'_>) -> Option<Rect> {
    let component = ComponentProxy::builder(connection)
        .destination(name.to_owned())
        .ok()?
        .path(path.to_owned())
        .ok()?
        .build()
        .await
        .ok()?;

    // Window coordinates, not screen: waypuppet's output *is* the screen, and
    // window-relative is what `click --at` takes.
    let (x, y, width, height) =
        tokio::time::timeout(CALL_TIMEOUT, component.get_extents(CoordType::Window))
            .await
            .ok()?
            .ok()?;

    // A widget that exists but is not laid out — a menu that has never been
    // opened, most commonly — answers with `i32::MIN` corners and a nominal
    // size. GTK does this for every collapsed menu item. Reporting that as a
    // rectangle would be worse than reporting nothing: `click --at` would take
    // it, and it points nowhere.
    if x == i32::MIN || y == i32::MIN || width <= 0 || height <= 0 {
        return None;
    }

    Some(Rect {
        origin: Point { x, y },
        size: Size {
            width: width.unsigned_abs(),
            height: height.unsigned_abs(),
        },
    })
}

/// A timeout, as an `io::Error` so it can travel as a `zbus::Error`.
fn timed_out(call: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{call} did not answer within {CALL_TIMEOUT:?}"),
    )
}

// ------------------------------------------------------------ the registry --

/// `org.a11y.Status`, the gate.
///
/// An application reads `IsEnabled` and stops there if it is false. waypuppet
/// exists to look at applications, so it is true — there is no user here whose
/// preference it could be expressing.
struct Status;

// Every method here is dispatched by zbus on the served value, so it has to be
// a method and has to take the arguments the interface declares — including the
// setters, which exist because the property is writable in the protocol and
// nothing here has anywhere to write it.
#[expect(clippy::unused_self, reason = "the shape is the interface's, not ours")]
#[interface(name = "org.a11y.Status")]
impl Status {
    #[zbus(property)]
    fn is_enabled(&self) -> bool {
        true
    }

    /// Ignored. waypuppet exists to look at applications, so accessibility is
    /// on; there is no user here whose preference this could be recording.
    #[zbus(property)]
    fn set_is_enabled(&self, enabled: bool) {
        let _ = enabled;
    }

    /// No screen reader is listening, and saying otherwise invites a toolkit to
    /// do extra work for an audience that does not exist.
    #[zbus(property)]
    fn screen_reader_enabled(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn set_screen_reader_enabled(&self, enabled: bool) {
        let _ = enabled;
    }
}

/// `org.a11y.Bus`, which says where the accessibility bus is.
///
/// The same bus. A real desktop runs a second one to keep accessibility traffic
/// off the session bus; here there is one client and nothing to separate it
/// from. waypuppet also sets `AT_SPI_BUS_ADDRESS`, so a well-behaved
/// application never asks this at all — it is here for the ones that do.
struct Bus(String);

#[interface(name = "org.a11y.Bus")]
impl Bus {
    fn get_address(&self) -> String {
        self.0.clone()
    }
}

/// `org.a11y.atspi.Socket`, the handshake.
struct Socket(Plug);

#[expect(
    clippy::needless_pass_by_value,
    reason = "zbus deserialises the argument, so it arrives owned"
)]
#[interface(name = "org.a11y.atspi.Socket")]
impl Socket {
    /// Register an application, and remember where it lives.
    ///
    /// The return value **must** carry waypuppet's unique connection name. See
    /// this module's header: a well-known name here fails to decode on the
    /// application's side, and the failure is silent on both.
    fn embed(
        &self,
        plug: (String, OwnedObjectPath),
        #[zbus(connection)] connection: &Connection,
    ) -> (String, OwnedObjectPath) {
        tracing::info!(name = %plug.0, path = %plug.1.as_str(), "an application registered");
        *lock(&self.0) = Some(plug);

        let unique = connection
            .unique_name()
            .map_or_else(String::new, ToString::to_string);
        (unique, root_path())
    }

    /// Deregister an application.
    ///
    /// Optional in the protocol — the registry is expected to notice a
    /// disconnect — but answering it keeps a tidy client tidy.
    fn unembed(&self, plug: (String, OwnedObjectPath)) {
        let mut registered = lock(&self.0);
        if registered.as_ref().is_some_and(|(name, _)| name == &plug.0) {
            tracing::info!(name = %plug.0, "an application deregistered");
            *registered = None;
        }
    }
}

/// The registry's own root, which is also where an application's root lives.
fn root_path() -> OwnedObjectPath {
    ObjectPath::from_static_str_unchecked("/org/a11y/atspi/accessible/root").into()
}

/// Where waypuppet's private bus sockets live.
///
/// # Errors
///
/// Returns a message if `XDG_RUNTIME_DIR` is not set, which means there is
/// nowhere private to put one.
pub fn runtime_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?;
    let directory = PathBuf::from(base).join("waypuppet");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    //! One test, and it is not about D-Bus.
    //!
    //! `waypuppet-select` carries its own copy of the AT-SPI role vocabulary,
    //! because the CLI has to reject an unknown role without linking `atspi`.
    //! A copy is a thing that drifts, and this one drifts *silently*: a role
    //! added upstream would simply become unselectable, reported as "not an
    //! AT-SPI role" to a caller looking straight at it in `waypuppet tree`.
    //!
    //! This is the only crate that has both, so this is where the two are held
    //! against each other.

    use atspi::Role;

    #[test]
    fn every_atspi_role_can_be_written_as_a_selector() {
        // `atspi` numbers its roles from zero and its own tests walk to 129.
        // Going one past the end is deliberate: if upstream adds a role, the
        // conversion starts succeeding and this fails until the table is
        // updated, which is the whole point.
        for value in 0..=130u32 {
            let Ok(role) = Role::try_from(value) else {
                continue;
            };
            let name = role.name();
            assert_eq!(
                waypuppet_select::roles::canonical(name),
                Some(name),
                "atspi role {value} ({name:?}) is missing from waypuppet-select's vocabulary, \
                 so a selector naming it would be refused"
            );
        }
    }
}
