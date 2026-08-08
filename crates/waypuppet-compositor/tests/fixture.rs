//! A minimal Wayland client, for driving the compositor from the outside.
//!
//! Everything the compositor claims is observable over the protocol, so the
//! tests assert over the protocol too — a fixture that fails for the same
//! reasons a real toolkit would is worth more than one that reaches into
//! compositor state.
//!
//! This is `wl_shm` only, deliberately. A software buffer with a known colour
//! in it is the simplest thing that proves the whole capture path, and it works
//! identically on llvmpipe and on a GPU.

#![allow(dead_code, reason = "each test binary uses a different part of this")]

use std::{
    io::Write as _,
    os::unix::{io::AsFd as _, net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_registry::WlRegistry,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

/// What the client has learned from the compositor.
#[derive(Default)]
pub struct Events {
    /// The size the compositor configured us to, if it has.
    pub configured: Option<(i32, i32)>,
    /// Whether an `xdg_surface.configure` arrived and was acked.
    pub surface_configured: bool,
}

/// A connected client with a toplevel.
pub struct Client {
    connection: Connection,
    queue: EventQueue<Events>,
    handle: QueueHandle<Events>,
    shm: WlShm,
    surface: WlSurface,
    toplevel: XdgToplevel,
    /// Held so the buffer outlives the commit that referenced it.
    buffer: Option<(WlShmPool, WlBuffer)>,
    /// What the compositor has told us.
    pub events: Events,
}

impl Client {
    /// Connect, bind the globals a toolkit needs, and create a toplevel.
    ///
    /// Connects the socket path directly rather than through
    /// `WAYLAND_DISPLAY`: setting an environment variable is process-global
    /// mutation in a binary that runs tests in parallel, and since Rust 2024 it
    /// is `unsafe` as well.
    ///
    /// # Panics
    ///
    /// Panics if the compositor is missing anything a client needs, which is
    /// the failure the tests exist to catch.
    pub fn connect(socket: &Path, title: &str) -> Self {
        let stream = UnixStream::connect(socket).expect("the compositor is listening");
        let connection = Connection::from_socket(stream).expect("the compositor speaks wayland");
        let (globals, queue) = registry_queue_init::<Events>(&connection).expect("registry");
        let handle = queue.handle();

        let compositor: WlCompositor = globals.bind(&handle, 1..=6, ()).expect("wl_compositor");
        let shm: WlShm = globals.bind(&handle, 1..=1, ()).expect("wl_shm");
        let wm_base: XdgWmBase = globals.bind(&handle, 1..=5, ()).expect("xdg_wm_base");

        // A missing global is the difference between "the app is slow" and "the
        // app hangs forever with no error", so check rather than assume.
        globals.contents().with_list(|list| {
            for interface in ["wl_seat", "wl_output"] {
                assert!(
                    list.iter().any(|global| global.interface == interface),
                    "{interface} is not advertised",
                );
            }
        });

        let surface = compositor.create_surface(&handle, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, ());
        let toplevel = xdg_surface.get_toplevel(&handle, ());
        toplevel.set_title(title.to_owned());

        // The buffer-less commit that asks to be configured. Skipping it means
        // never being configured, which means never being allowed to draw.
        surface.commit();

        Self {
            connection,
            queue,
            handle,
            shm,
            surface,
            toplevel,
            buffer: None,
            events: Events::default(),
        }
    }

    /// Dispatch until the compositor has configured us, and return the size.
    ///
    /// # Panics
    ///
    /// Panics if no configure arrives within ten seconds.
    pub fn wait_for_configure(&mut self) -> (i32, i32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.events.configured.is_none() && Instant::now() < deadline {
            self.queue
                .blocking_dispatch(&mut self.events)
                .expect("client dispatches");
        }
        self.events
            .configured
            .expect("the compositor configured the toplevel")
    }

    /// Fill the window with one colour and commit it.
    ///
    /// # Panics
    ///
    /// Panics if the commit provokes a protocol error.
    pub fn paint(&mut self, (width, height): (i32, i32), rgba: [u8; 4]) {
        let (pool, buffer) = shm_buffer(&self.shm, &self.handle, width, height, rgba);
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, width, height);
        self.surface.commit();
        self.buffer = Some((pool, buffer));

        self.connection
            .roundtrip()
            .expect("the commit did not provoke a protocol error");
    }

    /// Answer anything outstanding, and block until the compositor replies.
    ///
    /// **Only call this while something is driving the compositor.** A
    /// roundtrip waits for the compositor to answer a sync, so a client that
    /// keeps pumping after the test has stopped ticking will wait forever — and
    /// then the test's `join` waits for the client. Nothing in this fixture
    /// deadlocks by itself; the pair does.
    ///
    /// # Panics
    ///
    /// Panics if the connection has broken.
    pub fn pump(&mut self) {
        self.queue
            .roundtrip(&mut self.events)
            .expect("the connection is healthy");
    }

    /// The toplevel, for tests that want to change it.
    #[must_use]
    pub fn toplevel(&self) -> &XdgToplevel {
        &self.toplevel
    }
}

/// A pool and buffer of `width` × `height`, filled with `rgba`.
fn shm_buffer(
    shm: &WlShm,
    handle: &QueueHandle<Events>,
    width: i32,
    height: i32,
    rgba: [u8; 4],
) -> (WlShmPool, WlBuffer) {
    let stride = width * 4;
    let len = usize::try_from(stride * height).expect("the buffer fits in memory");

    // `Argb8888` is 0xAARRGGBB in a little-endian word, so the bytes in memory
    // run B, G, R, A. Writing them in RGBA order is the classic way to get an
    // image where red and blue are swapped.
    let [r, g, b, a] = rgba;
    let pixel = [b, g, r, a];

    let mut file = tempfile::tempfile().expect("temp file");
    let mut contents = Vec::with_capacity(len);
    for _ in 0..len / 4 {
        contents.extend_from_slice(&pixel);
    }
    file.write_all(&contents).expect("fill the buffer");
    file.flush().expect("flush");

    let pool = shm.create_pool(
        file.as_fd(),
        i32::try_from(len).expect("the pool fits in an i32"),
        handle,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        stride,
        wl_shm::Format::Argb8888,
        handle,
        (),
    );
    (pool, buffer)
}

impl Dispatch<WlRegistry, GlobalListContents> for Events {
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

impl Dispatch<XdgWmBase, ()> for Events {
    fn event(
        _state: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // Not ponging is a protocol error, and a compositor that pings is
        // entitled to disconnect us over it.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for Events {
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

impl Dispatch<XdgToplevel, ()> for Events {
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
            impl Dispatch<$proxy, ()> for Events {
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
