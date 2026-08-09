//! Phase 07's acceptance test: an application's tree comes back.
//!
//! **These assert on the tree, never on the handshake**, and that is the whole
//! design of the file. The spike that preceded this phase found the one mistake
//! in the area that has no symptom: if the `Embed` reply carries a well-known
//! bus name instead of a unique one, `atspi` fails to decode it, the failure is
//! swallowed as recoverable, and the application registers successfully and
//! then publishes nothing — silently, on both sides. A test that checked "did
//! Embed arrive?" would have passed against that. So every test here reads a
//! node out the far end.
//!
//! The application is a fake rather than a toolkit, and that is deliberate.
//! `gtk3-widget-factory` is only in the CI image, while these have to run in
//! the Flatpak SDK too — where there is no D-Bus daemon at all and the embedded
//! bus is the one under test. Driving a real toolkit is
//! [`ci/toolkit-smoke.sh`](../../../ci/toolkit-smoke.sh)'s job.

use std::time::Duration;

use waypuppet_compositor::a11y::Accessibility;
use waypuppet_proto::Failure;
use zbus::{
    Connection, connection, interface,
    zvariant::{ObjectPath, OwnedObjectPath},
};

/// Where an accessible object lives, on the wire.
type ObjectRef = (String, OwnedObjectPath);

/// The AT-SPI role numbers this test uses. They are a stable enum; these are
/// the three that make a plausible little window.
const ROLE_APPLICATION: u32 = 75;
const ROLE_FRAME: u32 = 23;
const ROLE_BUTTON: u32 = 43;

/// Long enough for a bus handshake on a loaded machine.
const LIMIT: Duration = Duration::from_secs(20);

/// One node of a fake application's tree.
struct Accessible {
    role: u32,
    name: String,
    children: Vec<ObjectRef>,
}

#[interface(name = "org.a11y.atspi.Accessible")]
impl Accessible {
    #[zbus(property)]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[zbus(property)]
    fn child_count(&self) -> i32 {
        i32::try_from(self.children.len()).unwrap_or(i32::MAX)
    }

    fn get_role(&self) -> u32 {
        self.role
    }

    fn get_children(&self) -> Vec<ObjectRef> {
        self.children.clone()
    }
}

/// The `Component` half, which is where bounds come from.
struct Component((i32, i32, i32, i32));

#[interface(name = "org.a11y.atspi.Component")]
impl Component {
    fn get_extents(&self, coord_type: u32) -> (i32, i32, i32, i32) {
        // One set of extents whatever was asked for: this fixture never moves,
        // so screen and window coordinates are the same thing.
        let _ = coord_type;
        self.0
    }
}

/// Build and register an application whose root has one framed button.
async fn register(address: &str, button: &str) -> Connection {
    let root = ObjectPath::from_static_str_unchecked("/org/a11y/atspi/accessible/root");
    let frame = ObjectPath::from_static_str_unchecked("/org/a11y/atspi/accessible/1");
    let child = ObjectPath::from_static_str_unchecked("/org/a11y/atspi/accessible/2");

    // Built without names first, because the object references inside the tree
    // have to carry *this* connection's unique name, which does not exist until
    // it has one.
    let connection = connection::Builder::address(address)
        .expect("waypuppet's bus address parses")
        .build()
        .await
        .expect("the fake application connects to waypuppet's bus");
    let me = connection
        .unique_name()
        .expect("a bus connection has a unique name")
        .to_string();

    let server = connection.object_server();
    server
        .at(
            &root,
            Accessible {
                role: ROLE_APPLICATION,
                name: "fake-app".to_owned(),
                children: vec![(me.clone(), frame.clone().into())],
            },
        )
        .await
        .expect("the root is served");
    server
        .at(
            &frame,
            Accessible {
                role: ROLE_FRAME,
                name: "fake window".to_owned(),
                children: vec![(me.clone(), child.clone().into())],
            },
        )
        .await
        .expect("the frame is served");
    server
        .at(&frame, Component((0, 0, 320, 240)))
        .await
        .expect("the frame has extents");
    server
        .at(
            &child,
            Accessible {
                role: ROLE_BUTTON,
                name: button.to_owned(),
                children: Vec::new(),
            },
        )
        .await
        .expect("the button is served");
    server
        .at(&child, Component((10, 20, 100, 30)))
        .await
        .expect("the button has extents");

    // The handshake, exactly as `accesskit_unix` performs it — including the
    // reply type. `ObjectRefOwned` is what decodes the name with
    // `UniqueName::try_from`, so deserialising into it here is the only thing
    // that makes this test able to catch a well-known name coming back. A
    // looser `(String, OwnedObjectPath)` accepts one happily, and the test
    // would then pass against the exact bug this file exists to prevent.
    let _: atspi::ObjectRefOwned = connection
        .call_method(
            Some("org.a11y.atspi.Registry"),
            "/org/a11y/atspi/accessible/root",
            Some("org.a11y.atspi.Socket"),
            "Embed",
            &(me.as_str(), root.as_ref()),
        )
        .await
        .expect("the registry answers Embed")
        .body()
        .deserialize()
        .expect("the reply decodes — a well-known name here would not");

    // Handed back rather than dropped: the tree lives on this connection, and
    // closing it takes the application off the bus.
    connection
}

/// Start waypuppet's accessibility side on a directory of its own.
async fn start() -> (tempfile::TempDir, Accessibility) {
    let directory = tempfile::tempdir().expect("temp dir");
    let a11y = Accessibility::start(directory.path())
        .await
        .expect("a bus starts — by daemon where there is one, embedded where not");
    (directory, a11y)
}

#[tokio::test]
async fn an_application_that_registers_has_its_tree_read_back() {
    let (_directory, a11y) = start().await;
    assert!(
        !a11y.registered(),
        "nothing has registered before anything connects",
    );

    let _app = tokio::time::timeout(LIMIT, register(a11y.address(), "Reconnect"))
        .await
        .expect("the application registers within the limit");

    let root = tokio::time::timeout(LIMIT, a11y.tree())
        .await
        .expect("the tree is read within the limit")
        .expect("a registered application has a tree");

    // Roles come back named, not numbered: `button` is what a caller writes a
    // selector against, and 43 is not.
    assert_eq!(root.role, "application");
    assert_eq!(root.name.as_deref(), Some("fake-app"));

    let frame = root.children.first().expect("the root has a child");
    assert_eq!(frame.role, "frame");
    assert_eq!(frame.name.as_deref(), Some("fake window"));

    let button = frame.children.first().expect("the frame has a child");
    assert_eq!(button.role, "button");
    assert_eq!(button.name.as_deref(), Some("Reconnect"));

    // Bounds in the same coordinates `click --at` takes, which is what makes
    // the tree an alternative to reading coordinates off a screenshot rather
    // than a second thing to correlate against one.
    let bounds = button.bounds.expect("the button knows where it is");
    assert_eq!(bounds.origin.x, 10);
    assert_eq!(bounds.origin.y, 20);
    assert_eq!(bounds.size.width, 100);
    assert_eq!(bounds.size.height, 30);
}

#[tokio::test]
async fn a_session_nothing_registered_with_says_so_rather_than_failing() {
    let (_directory, a11y) = start().await;

    // An ordinary state of affairs: plenty of toolkits publish no tree at all,
    // and a caller needs to tell that from a walk that broke halfway.
    let outcome = tokio::time::timeout(LIMIT, a11y.tree())
        .await
        .expect("the answer comes back promptly");

    assert!(
        matches!(outcome, Err(Failure::NoTree)),
        "an unregistered session is `NoTree`, not a `Tree` failure: {outcome:?}",
    );
}

#[tokio::test]
async fn a_node_that_is_nowhere_reports_no_bounds_rather_than_nonsense() {
    let (_directory, a11y) = start().await;

    let connection = connection::Builder::address(a11y.address())
        .expect("the address parses")
        .build()
        .await
        .expect("the fake application connects");
    let me = connection.unique_name().expect("a unique name").to_string();
    let root = ObjectPath::from_static_str_unchecked("/org/a11y/atspi/accessible/root");

    connection
        .object_server()
        .at(
            &root,
            Accessible {
                role: ROLE_APPLICATION,
                name: "unplaced".to_owned(),
                children: Vec::new(),
            },
        )
        .await
        .expect("the root is served");
    // What GTK reports for anything not laid out — a collapsed menu, most
    // often. It is `i32::MIN` corners with a nominal size, and passing it
    // through would hand `click --at` a point that goes nowhere.
    connection
        .object_server()
        .at(&root, Component((i32::MIN, i32::MIN, 1, 1)))
        .await
        .expect("the root has extents");

    let _: atspi::ObjectRefOwned = connection
        .call_method(
            Some("org.a11y.atspi.Registry"),
            "/org/a11y/atspi/accessible/root",
            Some("org.a11y.atspi.Socket"),
            "Embed",
            &(me.as_str(), root.as_ref()),
        )
        .await
        .expect("the registry answers Embed")
        .body()
        .deserialize()
        .expect("the reply decodes as a unique name, as a real client requires");

    let node = tokio::time::timeout(LIMIT, a11y.tree())
        .await
        .expect("the tree is read")
        .expect("there is a tree");

    assert_eq!(node.role, "application");
    assert_eq!(
        node.bounds, None,
        "a node that is nowhere must report nowhere, not a rectangle at i32::MIN",
    );

    drop(connection);
}
