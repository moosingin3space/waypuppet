//! Phase 01's acceptance test: a real Wayland client connects and maps a
//! window.
//!
//! This drives the compositor from the outside, with `wayland-client`, rather
//! than reaching into its state. That is deliberate — a toolkit only cares
//! about what it can observe over the protocol, so a test that asserts on the
//! same things fails for the same reasons a real client would.
//!
//! Compositor-side assertions (the commit counter, the mapped flag) arrive with
//! the control-socket bridge in plan 03. Until then there is no way to ask the
//! compositor a question from another thread, which is exactly what that bridge
//! is for.

use std::{
    io::Write as _,
    os::unix::{io::AsFd as _, net::UnixStream},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry::WlRegistry, wl_shm::WlShm,
        wl_shm_pool::WlShmPool, wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use waypuppet_compositor::{Compositor, Config};
use waypuppet_proto::Size;

const OUTPUT: Size = Size {
    width: 640,
    height: 480,
};

/// What the client learned, so assertions can run after the dispatch loop.
#[derive(Default)]
struct Client {
    /// The size the compositor configured us to, if it has.
    configured: Option<(i32, i32)>,
    /// Whether the configure was followed by an `xdg_surface.configure`.
    surface_configured: bool,
}

#[test]
fn a_client_connects_configures_and_commits() {
    let (socket_tx, socket_rx) = mpsc::channel();

    // The compositor is not `Send`, so it is built on the thread that runs it
    // and only its socket name comes back.
    let compositor = std::thread::spawn(move || {
        let config = Config {
            output: OUTPUT,
            ..Config::default()
        };
        let mut compositor = Compositor::new(&config).expect("compositor starts");
        socket_tx
            .send(compositor.socket_name().to_owned())
            .expect("test is still listening");
        compositor.run().expect("compositor runs");
    });

    let socket_name = socket_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("compositor reports its socket");

    // Connect the socket by hand rather than via `WAYLAND_DISPLAY`.
    // `connect_to_env` would mean setting an environment variable, which is
    // process-global mutation in a test binary that runs tests in parallel —
    // and, since Rust 2024, `unsafe`. Opening the path directly is neither.
    let socket_path = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set in the sandbox"),
    )
    .join(&socket_name);
    let stream = UnixStream::connect(&socket_path).expect("client connects");
    let connection = Connection::from_socket(stream).expect("client speaks wayland");
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).expect("registry");
    let handle = queue.handle();

    // Every global a toolkit checks for before it will start. A missing one is
    // the difference between "the app is slow" and "the app hangs forever".
    let compositor_global: WlCompositor = globals.bind(&handle, 1..=6, ()).expect("wl_compositor");
    let shm: WlShm = globals.bind(&handle, 1..=1, ()).expect("wl_shm");
    let wm_base: XdgWmBase = globals.bind(&handle, 1..=5, ()).expect("xdg_wm_base");
    globals.contents().with_list(|list| {
        for interface in ["wl_seat", "wl_output"] {
            assert!(
                list.iter().any(|global| global.interface == interface),
                "{interface} is not advertised",
            );
        }
    });

    let surface = compositor_global.create_surface(&handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, ());
    let _toplevel = xdg_surface.get_toplevel(&handle, ());
    surface.commit(); // the buffer-less commit that asks to be configured

    let mut client = Client::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.configured.is_none() && Instant::now() < deadline {
        queue
            .blocking_dispatch(&mut client)
            .expect("client dispatches");
    }

    let (width, height) = client.configured.expect("the compositor configured us");
    assert!(
        client.surface_configured,
        "xdg_toplevel.configure arrived without xdg_surface.configure",
    );
    assert_eq!(
        (width, height),
        (
            i32::try_from(OUTPUT.width).unwrap(),
            i32::try_from(OUTPUT.height).unwrap()
        ),
        "the window should be configured to fill the output",
    );

    // Now actually map: attach a buffer and commit. A protocol error here shows
    // up as a disconnect on the next roundtrip.
    let (pool, buffer) = shm_buffer(&shm, &handle, width, height);
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width, height);
    surface.commit();

    connection
        .roundtrip()
        .expect("the commit did not trigger a protocol error");

    drop(buffer);
    drop(pool);
    drop(connection);
    drop(compositor);
}

/// A single-colour ARGB buffer of the given size.
fn shm_buffer(
    shm: &WlShm,
    handle: &QueueHandle<Client>,
    width: i32,
    height: i32,
) -> (WlShmPool, WlBuffer) {
    let stride = width * 4;
    let len = usize::try_from(stride * height).expect("buffer fits in memory");

    let mut file = tempfile::tempfile().expect("temp file");
    file.write_all(&vec![0xFF; len]).expect("fill buffer");
    file.flush().expect("flush");

    let pool = shm.create_pool(
        file.as_fd(),
        i32::try_from(len).expect("pool fits in an i32"),
        handle,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        stride,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        handle,
        (),
    );
    (pool, buffer)
}

impl Dispatch<WlRegistry, GlobalListContents> for Client {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for Client {
    fn event(
        _state: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // Failing to pong is a protocol error, and a compositor that pings is
        // entitled to disconnect us for it.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for Client {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.surface_configured = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for Client {
    fn event(
        state: &mut Self,
        _toplevel: &XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event {
            state.configured = Some((width, height));
        }
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),* $(,)?) => {
        $(
            impl Dispatch<$proxy, ()> for Client {
                fn event(
                    _state: &mut Self,
                    _proxy: &$proxy,
                    _event: <$proxy as wayland_client::Proxy>::Event,
                    _data: &(),
                    _connection: &Connection,
                    _handle: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

ignore_events!(WlCompositor, WlSurface, WlShm, WlShmPool, WlBuffer);
