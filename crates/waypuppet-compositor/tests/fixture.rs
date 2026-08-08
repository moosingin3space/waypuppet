//! A test harness that cannot deadlock against itself, and a minimal Wayland
//! client for it to drive.
//!
//! Everything the compositor claims is observable over the protocol, so the
//! tests assert over the protocol too — a fixture that fails for the same
//! reasons a real toolkit would is worth more than one that reaches into
//! compositor state.
//!
//! # Why the harness owns both halves
//!
//! A client blocking on the compositor is only safe while somebody is ticking
//! the compositor. The first version of this file left that to a comment, and
//! the first test written against it hung: the client kept calling `roundtrip`
//! to stay healthy, the test stopped ticking once it had its capture, and the
//! `join` at the end waited on a thread that was waiting on a loop nobody was
//! running.
//!
//! So the compositor and the client are not two things a test coordinates.
//! They are one [`Harness`], and **every method on it that waits also ticks**.
//! There is no way to reach a blocking client operation except through a call
//! that is already driving the compositor, which makes the mistake
//! unrepresentable rather than merely documented.
//!
//! # The two time limits
//!
//! [`Harness`] fails its own waits after [`WAIT_LIMIT`] with a message naming
//! what it was waiting for. [`guarded`] is the backstop for anything the
//! harness does not mediate, and aborts after [`WATCHDOG_LIMIT`]. The inner
//! limit is much the shorter of the two, so the precise failure wins the race
//! against the blunt one.

#![allow(dead_code, reason = "each test binary uses a different part of this")]

use std::{
    io::Write as _,
    os::unix::{io::AsFd as _, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
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
use waypuppet_compositor::{Compositor, Config, Frame, State};
use waypuppet_proto::Size;

/// How long any single harness wait may take before it fails with a diagnosis.
pub const WAIT_LIMIT: Duration = Duration::from_secs(10);

/// How long a whole guarded test may take before the watchdog gives up.
///
/// Generous relative to [`WAIT_LIMIT`] on purpose: it should only ever fire for
/// a wait the harness does not mediate, and software rendering is several times
/// slower than a GPU.
pub const WATCHDOG_LIMIT: Duration = Duration::from_mins(1);

/// How long each tick may block waiting for events.
const TICK: Duration = Duration::from_millis(5);

// ---------------------------------------------------------------- watchdog --

/// What a thread is currently blocked on, for the watchdog to report.
///
/// Cheap enough to update on every wait, and it turns "the test hung" into "the
/// test hung waiting for the client to be configured" — most of the diagnostic
/// value of a real wait-for graph at none of the cost.
#[derive(Clone, Default)]
pub struct Watch(Arc<Mutex<Option<(String, Instant)>>>);

impl Watch {
    /// Record what is being waited on. The guard clears it when dropped.
    fn begin(&self, label: &str) -> WatchGuard<'_> {
        *self.lock() = Some((label.to_owned(), Instant::now()));
        WatchGuard(self)
    }

    /// Describe the outstanding wait, if there is one.
    fn report(&self) -> String {
        self.lock().as_ref().map_or_else(
            || "not inside a harness wait".to_owned(),
            |(label, since)| {
                format!(
                    "waiting on {label} for {:.1}s",
                    since.elapsed().as_secs_f32()
                )
            },
        )
    }

    /// Lock, tolerating poisoning — a panicking test is exactly when this is
    /// most worth reading.
    fn lock(&self) -> MutexGuard<'_, Option<(String, Instant)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Clears the [`Watch`] when the wait it describes finishes.
struct WatchGuard<'a>(&'a Watch);

impl Drop for WatchGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

/// Signals completion even if the body panics.
struct Signal(Sender<()>);

impl Drop for Signal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Run a test body under a watchdog, with a harness built for it.
///
/// The body runs on its own thread — [`Compositor`] is not `Send`, so it has to
/// be built where it is used — while this thread watches the clock. If the body
/// has not finished within [`WATCHDOG_LIMIT`], the process aborts with a
/// message naming the test and what it was waiting on.
///
/// Aborting takes the whole test binary with it, losing the results of anything
/// running alongside. That is the honest trade: a hung thread cannot be killed
/// in Rust, so the choice is between one loud failure and a job that runs until
/// CI times out having printed nothing at all.
///
/// # Panics
///
/// Propagates a panic from the body, so ordinary assertion failures behave
/// normally.
pub fn guarded<F>(name: &'static str, output: Size, body: F)
where
    F: FnOnce(&mut Harness) + Send + 'static,
{
    let watch = Watch::default();
    let (done_tx, done_rx) = mpsc::channel();

    let worker = thread::spawn({
        let watch = watch.clone();
        move || {
            let _signal = Signal(done_tx);
            let mut harness = Harness::new(output, watch);
            body(&mut harness);
        }
    });

    if done_rx.recv_timeout(WATCHDOG_LIMIT).is_err() {
        eprintln!(
            "\n{name}: no progress for {WATCHDOG_LIMIT:?} — {}",
            watch.report()
        );
        eprintln!("{name}: aborting; a hung thread cannot be unwound");
        std::process::abort();
    }

    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

// ----------------------------------------------------------------- harness --

/// A compositor and the clients driven against it.
///
/// Every method here that waits also ticks the compositor, which is the whole
/// point — see the module docs.
pub struct Harness {
    compositor: Compositor,
    socket: PathBuf,
    watch: Watch,
}

impl Harness {
    /// Start a compositor with the given output size.
    ///
    /// # Panics
    ///
    /// Panics if the compositor cannot start, or if `XDG_RUNTIME_DIR` is unset.
    #[must_use]
    pub fn new(output: Size, watch: Watch) -> Self {
        let compositor = Compositor::new(&Config {
            output,
            ..Config::default()
        })
        .expect("compositor starts");

        let socket = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set in the sandbox"),
        )
        .join(compositor.socket_name());

        Self {
            compositor,
            socket,
            watch,
        }
    }

    /// What the compositor currently knows.
    #[must_use]
    pub fn state(&self) -> &State {
        self.compositor.state()
    }

    /// Composite and read back the current frame.
    ///
    /// # Panics
    ///
    /// Panics if rendering or readback fails.
    pub fn capture(&mut self) -> Frame {
        self.compositor.capture().expect("capture succeeds")
    }

    /// Tick until `condition` holds of the compositor's state.
    ///
    /// # Panics
    ///
    /// Panics after [`WAIT_LIMIT`], naming what was being waited for.
    pub fn drive_until(&mut self, what: &str, mut condition: impl FnMut(&State) -> bool) {
        let _guard = self.watch.begin(what);
        let deadline = Instant::now() + WAIT_LIMIT;
        while Instant::now() < deadline {
            if condition(self.compositor.state()) {
                return;
            }
            self.compositor.tick(TICK).expect("the compositor ticks");
        }
        panic!("gave up after {WAIT_LIMIT:?} waiting for {what}");
    }

    /// Connect a client on its own thread, ticking until it is up.
    ///
    /// # Panics
    ///
    /// Panics if the client cannot connect within [`WAIT_LIMIT`].
    pub fn start_client(&mut self, title: &str) -> ClientThread {
        let (command_tx, command_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();

        let join = thread::spawn({
            let socket = self.socket.clone();
            let title = title.to_owned();
            move || client_thread(&socket, &title, &command_rx, &reply_tx)
        });

        let mut client = ClientThread {
            commands: command_tx,
            replies: reply_rx,
            join: Some(join),
            surface_configured: false,
        };
        self.await_reply(&mut client, "the client to connect");
        client
    }

    /// Wait for the compositor to configure the client, and return the size.
    ///
    /// # Panics
    ///
    /// Panics if no configure arrives within [`WAIT_LIMIT`].
    pub fn configure(&mut self, client: &mut ClientThread) -> (i32, i32) {
        client.send(Command::Configure);
        match self.await_reply(client, "the client to be configured") {
            Reply::Configured(width, height) => (width, height),
            other => panic!("expected a configure, got {other:?}"),
        }
    }

    /// Have the client fill its window with one colour and commit it.
    ///
    /// # Panics
    ///
    /// Panics if the paint does not complete within [`WAIT_LIMIT`].
    pub fn paint(&mut self, client: &mut ClientThread, rgba: [u8; 4]) {
        client.send(Command::Paint(rgba));
        self.await_reply(client, "the client to paint");
    }

    /// Shut the client down, ticking until its thread has finished.
    ///
    /// # Panics
    ///
    /// Panics if the client does not stop within [`WAIT_LIMIT`].
    pub fn stop(&mut self, mut client: ClientThread) {
        client.send(Command::Stop);
        self.await_reply(&mut client, "the client to stop");
        if let Some(join) = client.join.take() {
            join.join().expect("the client thread finishes cleanly");
        }
    }

    /// Tick until the client answers.
    ///
    /// This is the method the whole design turns on: the client may block on
    /// the compositor as much as it likes, because the compositor is running
    /// right here for as long as the client is thinking.
    ///
    /// # Panics
    ///
    /// Panics if the client dies, or does not answer within [`WAIT_LIMIT`].
    fn await_reply(&mut self, client: &mut ClientThread, what: &str) -> Reply {
        let _guard = self.watch.begin(what);
        let deadline = Instant::now() + WAIT_LIMIT;
        while Instant::now() < deadline {
            match client.replies.try_recv() {
                Ok(reply) => {
                    if matches!(reply, Reply::Configured(..)) {
                        client.surface_configured = true;
                    }
                    return reply;
                }
                Err(TryRecvError::Empty) => {
                    self.compositor.tick(TICK).expect("the compositor ticks");
                }
                // The sender is gone, so the client thread has died. Waiting
                // longer cannot help; joining reports why it died.
                Err(TryRecvError::Disconnected) => match client.join.take() {
                    Some(join) => match join.join() {
                        Err(panic) => std::panic::resume_unwind(panic),
                        Ok(()) => panic!("the client thread ended without answering"),
                    },
                    None => panic!("the client thread is gone"),
                },
            }
        }
        panic!("gave up after {WAIT_LIMIT:?} waiting for {what}");
    }
}

/// A client running on its own thread.
///
/// Deliberately has no blocking methods of its own — everything goes through
/// [`Harness`], which ticks while it waits.
pub struct ClientThread {
    commands: Sender<Command>,
    replies: Receiver<Reply>,
    join: Option<JoinHandle<()>>,
    /// Set once a configure has been seen and acked.
    surface_configured: bool,
}

impl ClientThread {
    /// Whether the client's `xdg_surface` was configured before its toplevel.
    #[must_use]
    pub fn surface_was_configured(&self) -> bool {
        self.surface_configured
    }

    /// Queue a command for the client thread.
    fn send(&self, command: Command) {
        // A closed channel means the thread has died, and `await_reply` turns
        // that into a useful panic, so there is nothing to say here.
        let _ = self.commands.send(command);
    }
}

/// What the harness can ask a client to do.
enum Command {
    /// Block until configured, and report the size.
    Configure,
    /// Fill the window and commit.
    Paint([u8; 4]),
    /// Disconnect and finish.
    Stop,
}

/// What a client reports back.
#[derive(Debug)]
enum Reply {
    /// Connected and bound to every global.
    Connected,
    /// Configured to this size.
    Configured(i32, i32),
    /// The last command finished.
    Done,
}

/// The client thread's body: connect, then serve commands until told to stop.
fn client_thread(
    socket: &Path,
    title: &str,
    commands: &Receiver<Command>,
    replies: &Sender<Reply>,
) {
    let mut client = Client::connect(socket, title);
    if replies.send(Reply::Connected).is_err() {
        return;
    }

    let mut size = (0, 0);
    while let Ok(command) = commands.recv() {
        let reply = match command {
            Command::Configure => {
                size = client.wait_for_configure();
                Reply::Configured(size.0, size.1)
            }
            Command::Paint(rgba) => {
                client.paint(size, rgba);
                Reply::Done
            }
            Command::Stop => {
                let _ = replies.send(Reply::Done);
                return;
            }
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

// ------------------------------------------------------------------ client --

/// A connected client with a toplevel.
///
/// Private to this module: its methods block, and blocking is only safe on the
/// client thread, where the harness is ticking on its behalf.
struct Client {
    connection: Connection,
    queue: EventQueue<Events>,
    handle: QueueHandle<Events>,
    shm: WlShm,
    surface: WlSurface,
    _toplevel: XdgToplevel,
    /// Held so the buffer outlives the commit that referenced it.
    buffer: Option<(WlShmPool, WlBuffer)>,
    events: Events,
}

/// What the client has learned from the compositor.
#[derive(Default)]
struct Events {
    /// The size the compositor configured us to, if it has.
    configured: Option<(i32, i32)>,
    /// Whether an `xdg_surface.configure` arrived and was acked.
    surface_configured: bool,
}

impl Client {
    /// Connect, bind the globals a toolkit needs, and create a toplevel.
    ///
    /// Connects the socket path directly rather than through
    /// `WAYLAND_DISPLAY`: setting an environment variable is process-global
    /// mutation in a binary that runs tests in parallel, and since Rust 2024 it
    /// is `unsafe` as well.
    fn connect(socket: &Path, title: &str) -> Self {
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
            _toplevel: toplevel,
            buffer: None,
            events: Events::default(),
        }
    }

    /// Dispatch until the compositor has configured us, and return the size.
    fn wait_for_configure(&mut self) -> (i32, i32) {
        while self.events.configured.is_none() {
            self.queue
                .blocking_dispatch(&mut self.events)
                .expect("the connection is healthy");
        }
        assert!(
            self.events.surface_configured,
            "xdg_toplevel.configure arrived without xdg_surface.configure",
        );
        self.events
            .configured
            .expect("the loop only exits once this is set")
    }

    /// Fill the window with one colour and commit it.
    fn paint(&mut self, (width, height): (i32, i32), rgba: [u8; 4]) {
        let (pool, buffer) = shm_buffer(&self.shm, &self.handle, width, height, rgba);
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, width, height);
        self.surface.commit();
        self.buffer = Some((pool, buffer));

        self.connection
            .roundtrip()
            .expect("the commit did not provoke a protocol error");
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
