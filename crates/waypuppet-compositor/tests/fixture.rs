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
    io::{Read as _, Write as _},
    os::{
        fd::{AsFd as _, AsRawFd as _},
        unix::{fs::FileExt as _, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smithay::{
    backend::allocator::{
        Allocator as _, Buffer as _, Fourcc, Modifier,
        dmabuf::{AsDmabuf as _, Dmabuf},
        gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
    },
    input::keyboard::xkb,
};
use tokio::sync::oneshot;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard},
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
        },
        primary_selection::zv1::client::{
            zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
            zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
            zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
            zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
        },
    },
    xdg::shell::client::{
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::{self, XdgToplevel},
        xdg_wm_base::{self, XdgWmBase},
    },
};
use waypuppet_compositor::{Command, Compositor, Config, Frame, Rendering, State};
use waypuppet_proto::{Failure, Selection, Size};

/// The MIME type the fixture exchanges selections in.
///
/// The same one waypuppet offers and asks for. A test that disagreed with the
/// compositor here would be testing mime negotiation rather than the clipboard.
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";

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

/// One input event, as the client saw it.
///
/// Deliberately the client's view rather than the compositor's: keys are
/// resolved through the keymap the compositor actually sent over the wire, so a
/// reverse index that produced the wrong keycode fails here. Asserting against
/// waypuppet's own idea of the keymap would be circular.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A key, named by the keysym it resolved to under the current modifiers.
    Key {
        /// The keysym's xkb name, such as `a`, `A` or `Return`.
        name: String,
        /// Whether this was a press.
        pressed: bool,
    },
    /// The pointer entered the surface, at surface-local coordinates.
    Enter {
        /// Horizontal position within the surface.
        x: i32,
        /// Vertical position within the surface.
        y: i32,
    },
    /// The pointer moved, at surface-local coordinates.
    Motion {
        /// Horizontal position within the surface.
        x: i32,
        /// Vertical position within the surface.
        y: i32,
    },
    /// A pointer button, as a Linux input-event code.
    Button {
        /// The button code, such as `0x110` for the left button.
        code: u32,
        /// Whether this was a press.
        pressed: bool,
    },
    /// A discrete scroll, in `v120` units — 120 per wheel click.
    Scroll {
        /// Horizontal steps, positive rightwards.
        horizontal: i32,
        /// Vertical steps, positive downwards.
        vertical: i32,
    },
}

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
    guarded_on(name, output, Rendering::default(), body);
}

/// The same, on a named renderer.
///
/// Exists so the software path is exercised on purpose rather than assumed to
/// work because the hardware path does. They are different drivers: llvmpipe
/// and a GPU disagree about rasterisation, and can disagree about which dmabuf
/// formats they will import.
///
/// # Panics
///
/// Propagates a panic from the body, so ordinary assertion failures behave
/// normally.
pub fn guarded_on<F>(name: &'static str, output: Size, rendering: Rendering, body: F)
where
    F: FnOnce(&mut Harness) + Send + 'static,
{
    let watch = Watch::default();
    let (done_tx, done_rx) = mpsc::channel();

    let worker = thread::spawn({
        let watch = watch.clone();
        move || {
            let _signal = Signal(done_tx);
            let mut harness = Harness::new(output, rendering, watch);
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

// ----------------------------------------------------------------- control --

/// A whole session — compositor, child process and control socket — for the
/// tests [`Harness`] deliberately cannot serve.
///
/// [`Harness`] owns a compositor and nothing else, which is right for almost
/// everything: the facts under test are protocol facts. `wait --exit` is the
/// exception. The child process belongs to the control thread, and a process
/// can exit without ever having connected, so there is no Wayland event to
/// observe and no way to test it without the real thing.
pub mod control {
    use std::{path::PathBuf, thread::JoinHandle, time::Duration};

    use tarpc::{client, context, tokio_serde::formats::Bincode};
    use waypuppet_compositor::{Config, Error, session};
    use waypuppet_proto::{Size, WaypuppetClient};

    /// Long enough for software rendering, short enough to fail while somebody
    /// is watching.
    pub const LIMIT: Duration = Duration::from_secs(20);

    /// Small: nothing here renders, and a session is started per test.
    const OUTPUT: Size = Size {
        width: 64,
        height: 48,
    };

    /// A session running on its own thread.
    ///
    /// Every call against it carries a deadline and the socket wait is bounded,
    /// so the only way to hang is [`Serving::join`] on a session that was never
    /// asked to stop. Call `kill` first, as a caller would.
    pub struct Serving {
        /// Where the control socket lives.
        pub socket: PathBuf,
        /// Held so the socket and capture directory outlive the session.
        _directory: tempfile::TempDir,
        thread: Option<JoinHandle<Result<Option<i32>, Error>>>,
    }

    impl Serving {
        /// Start a session, optionally hosting `argv`.
        ///
        /// # Panics
        ///
        /// Panics if the temporary directory or the session thread cannot be
        /// created.
        #[must_use]
        pub fn start(argv: &[&str]) -> Self {
            let directory = tempfile::tempdir().expect("temp dir");
            let socket = directory.path().join("control.sock");
            let captures = directory.path().to_path_buf();
            let argv: Vec<String> = argv.iter().map(|part| (*part).to_owned()).collect();

            let thread = std::thread::spawn({
                let socket = socket.clone();
                move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("tokio runtime");
                    runtime.block_on(session::serve(
                        &Config {
                            output: OUTPUT,
                            capture_dir: captures,
                            ..Config::default()
                        },
                        &socket,
                        &argv,
                        false,
                    ))
                }
            });

            Self {
                socket,
                _directory: directory,
                thread: Some(thread),
            }
        }

        /// Connect a typed client once the session is listening.
        ///
        /// The poll here is not the sleep-for-synchronisation the project bans:
        /// it checks the condition it is waiting for — that the socket accepts
        /// a connection — rather than guessing how long the condition takes.
        /// There is no in-process signal to use instead, because `serve` is
        /// what creates the socket.
        ///
        /// # Panics
        ///
        /// Panics if the session never starts listening within [`LIMIT`].
        pub async fn connect(&self) -> WaypuppetClient {
            let deadline = tokio::time::Instant::now() + LIMIT;
            while tokio::time::Instant::now() < deadline {
                if let Ok(transport) =
                    tarpc::serde_transport::unix::connect(&self.socket, Bincode::default).await
                {
                    return WaypuppetClient::new(client::Config::default(), transport).spawn();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the session never listened on {}", self.socket.display());
        }

        /// Wait for the session thread to finish.
        ///
        /// # Panics
        ///
        /// Panics if the thread panicked or the session did not shut down
        /// cleanly.
        pub fn join(mut self) {
            if let Some(thread) = self.thread.take() {
                thread
                    .join()
                    .expect("the session thread finishes")
                    .expect("the session shuts down cleanly");
            }
        }
    }

    /// A call context with a bounded deadline — as everything here has.
    #[must_use]
    pub fn deadline() -> context::Context {
        let mut context = context::current();
        context.deadline = std::time::Instant::now() + LIMIT;
        context
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
    pub fn new(output: Size, rendering: Rendering, watch: Watch) -> Self {
        let compositor = Compositor::new(&Config {
            output,
            rendering,
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

    /// Whether the compositor is rendering in software.
    ///
    /// What makes a `Rendering::Software` test mean something: without it the
    /// same body passes on a GPU, and the software path would be untested while
    /// looking covered.
    ///
    /// # Panics
    ///
    /// Panics if the session has no renderer at all.
    #[must_use]
    pub fn renders_in_software(&self) -> bool {
        self.compositor
            .state()
            .renders_in_software()
            .expect("the session has a renderer")
    }

    /// The bridge, for a test that wants to park a reply and keep going.
    #[must_use]
    pub fn commands(&self) -> waypuppet_compositor::Commands {
        self.compositor.commands()
    }

    /// How many iterations the compositor loop has completed.
    ///
    /// The evidence that a call which failed to settle was the client's doing
    /// and not a stalled loop.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.compositor.heartbeat().ticks()
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
        client.send(ClientCommand::Configure);
        match self.await_reply(client, "the client to be configured") {
            Reply::Configured(width, height) => (width, height),
            other => panic!("expected a configure, got {other:?}"),
        }
    }

    /// Have the client request a frame callback and wait for it.
    ///
    /// # Panics
    ///
    /// Panics after [`WAIT_LIMIT`] if no callback arrives, which is what a
    /// compositor that never paces its clients looks like from here.
    pub fn await_frame(&mut self, client: &mut ClientThread) {
        client.send(ClientCommand::AwaitFrame);
        self.await_reply(client, "a frame callback");
    }

    /// Have the client rename its window, without drawing anything.
    ///
    /// Without drawing is the point: `xdg_toplevel.set_title` is not double
    /// buffered, so a rename is observable the instant it is requested and
    /// there is no commit behind it to notice. A `wait --title` that only woke
    /// on frames would sit parked through this.
    ///
    /// # Panics
    ///
    /// Panics if the client does not answer within [`WAIT_LIMIT`].
    pub fn retitle(&mut self, client: &mut ClientThread, title: &str) {
        client.send(ClientCommand::Title(title.to_owned()));
        self.await_reply(client, "the client to rename its window");
    }

    /// Have the client fill its window with one colour and commit it.
    ///
    /// # Panics
    ///
    /// Panics if the paint does not complete within [`WAIT_LIMIT`].
    pub fn paint(&mut self, client: &mut ClientThread, rgba: [u8; 4]) {
        client.send(ClientCommand::Paint(rgba));
        self.await_reply(client, "the client to paint");
    }

    /// The same, but through `zwp_linux_dmabuf_v1` rather than `wl_shm`.
    ///
    /// `Err` means the *client* could not get as far as handing a buffer over:
    /// no render node it may open, no advertised format it can use, or a gbm
    /// buffer it cannot map. Those are facts about the machine.
    ///
    /// Everything after the buffer is handed over is waypuppet's problem, and
    /// panics rather than returning here — a compositor that rejects the import
    /// must not be reportable as "this machine cannot do dmabuf".
    ///
    /// # Panics
    ///
    /// Panics if the compositor refuses the buffer, or if the client does not
    /// answer within [`WAIT_LIMIT`].
    pub fn paint_dmabuf(&mut self, client: &mut ClientThread, rgba: [u8; 4]) -> Result<(), String> {
        client.send(ClientCommand::PaintDmabuf(rgba));
        match self.await_reply(client, "the client to paint through dmabuf") {
            Reply::Painted(result) => result,
            other => panic!("expected a dmabuf paint, got {other:?}"),
        }
    }

    /// Let the client answer whatever the compositor has asked of it.
    ///
    /// A selection the *client* owns is handed over during the client's own
    /// dispatch, and between commands the client thread is blocked waiting for
    /// the next one. So a compositor-side read has to be interleaved with this
    /// rather than simply awaited — which is the harness's usual bargain in the
    /// other direction: nothing blocks unless somebody is driving the far side.
    ///
    /// # Panics
    ///
    /// Panics if the client does not answer within [`WAIT_LIMIT`].
    pub fn pump(&mut self, client: &mut ClientThread) {
        client.send(ClientCommand::Pump);
        self.await_reply(client, "the client to answer the compositor");
    }

    /// Have the client take ownership of a selection.
    ///
    /// # Panics
    ///
    /// Panics if the client does not answer within [`WAIT_LIMIT`].
    pub fn own_selection(&mut self, client: &mut ClientThread, selection: Selection, text: &str) {
        self.own_selection_as(client, selection, TEXT_MIME, text);
    }

    /// The same, under a mime type of the caller's choosing.
    ///
    /// Exists for the case where the client offers something waypuppet cannot
    /// read, which has to be told apart from an empty selection.
    ///
    /// # Panics
    ///
    /// Panics if the client does not answer within [`WAIT_LIMIT`].
    pub fn own_selection_as(
        &mut self,
        client: &mut ClientThread,
        selection: Selection,
        mime: &str,
        text: &str,
    ) {
        client.send(ClientCommand::OwnSelection(
            selection,
            mime.to_owned(),
            text.to_owned(),
        ));
        self.await_reply(client, "the client to take the selection");
    }

    /// Ask the client what it can see in a selection.
    ///
    /// This is the client's view over the protocol, not waypuppet's own idea of
    /// what it offered — which is the point. A selection nobody was actually
    /// told about would pass the second check and fail this one.
    ///
    /// # Panics
    ///
    /// Panics if the client does not answer within [`WAIT_LIMIT`].
    pub fn client_sees(
        &mut self,
        client: &mut ClientThread,
        selection: Selection,
    ) -> Option<String> {
        client.send(ClientCommand::ReadSelection(selection));
        match self.await_reply(client, "the client to read the selection") {
            Reply::Selection(text) => text,
            other => panic!("expected a selection, got {other:?}"),
        }
    }

    /// Read a selection through the compositor, pumping the client meanwhile.
    ///
    /// # Panics
    ///
    /// Panics if nothing answers within [`WAIT_LIMIT`], or if the compositor
    /// has gone away.
    pub fn read_selection(
        &mut self,
        client: &mut ClientThread,
        selection: Selection,
    ) -> Result<Option<String>, Failure> {
        // Cloned rather than borrowed from `self`: the loop below hands the
        // whole harness to `pump`, which a live borrow of `self.watch` would
        // rule out. `Watch` is a handle, so a clone is the same watch.
        let watch = self.watch.clone();
        let _guard = watch.begin("a selection to be read");
        let (reply, mut receiver) = oneshot::channel();
        self.compositor
            .commands()
            .send(Command::ClipboardGet { selection, reply })
            .expect("the compositor is listening");

        let deadline = Instant::now() + WAIT_LIMIT;
        while Instant::now() < deadline {
            match receiver.try_recv() {
                Ok(answer) => return answer,
                Err(oneshot::error::TryRecvError::Closed) => return Err(Failure::ShuttingDown),
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Both sides get a turn: the compositor to send the request
                    // and drain the pipe, the client to notice and write.
                    self.compositor.tick(TICK).expect("the compositor ticks");
                    self.pump(client);
                }
            }
        }
        panic!("gave up after {WAIT_LIMIT:?} waiting for a selection to be read");
    }

    /// Hand back the input the client has received since the last call.
    ///
    /// # Panics
    ///
    /// Panics if the client does not answer within [`WAIT_LIMIT`].
    pub fn input(&mut self, client: &mut ClientThread) -> Vec<Input> {
        client.send(ClientCommand::Drain);
        match self.await_reply(client, "the client to report its input") {
            Reply::Input(input) => input,
            other => panic!("expected input, got {other:?}"),
        }
    }

    /// Queue a bridge command and tick until it is answered.
    ///
    /// `None` means `limit` passed with nothing sent back, which for a settling
    /// verb is the interesting case: the events were delivered and the client
    /// did not draw. The compositor is ticking throughout, so a `None` here is a
    /// statement about the client rather than about the loop.
    ///
    /// # Panics
    ///
    /// Panics if the compositor thread has gone away, or if a tick fails.
    pub fn deliver(
        &mut self,
        what: &str,
        limit: Duration,
        build: impl FnOnce(oneshot::Sender<Result<(), Failure>>) -> Command,
    ) -> Option<Result<(), Failure>> {
        let _guard = self.watch.begin(what);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        self.compositor
            .commands()
            .send(build(reply_tx))
            .expect("the compositor is listening");

        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            match reply_rx.try_recv() {
                Ok(reply) => return Some(reply),
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.compositor.tick(TICK).expect("the compositor ticks");
                }
                // The compositor dropped the sender without answering, which is
                // the shutdown path rather than a timeout.
                Err(oneshot::error::TryRecvError::Closed) => {
                    return Some(Err(Failure::ShuttingDown));
                }
            }
        }
        None
    }

    /// Shut the client down, ticking until its thread has finished.
    ///
    /// # Panics
    ///
    /// Panics if the client does not stop within [`WAIT_LIMIT`].
    pub fn stop(&mut self, mut client: ClientThread) {
        client.send(ClientCommand::Stop);
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
    commands: Sender<ClientCommand>,
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
    fn send(&self, command: ClientCommand) {
        // A closed channel means the thread has died, and `await_reply` turns
        // that into a useful panic, so there is nothing to say here.
        let _ = self.commands.send(command);
    }
}

/// What the harness can ask a client to do.
enum ClientCommand {
    /// Block until configured, and report the size.
    Configure,
    /// Rename the window, with no commit behind it.
    Title(String),
    /// Fill the window and commit, through `wl_shm`.
    Paint([u8; 4]),
    /// The same, through `zwp_linux_dmabuf_v1`.
    PaintDmabuf([u8; 4]),
    /// Report the input received since the last drain.
    Drain,
    /// Request a frame callback and block until the compositor answers it.
    AwaitFrame,
    /// Dispatch whatever the compositor has sent, and answer it.
    Pump,
    /// Take ownership of a selection, offering this text under this mime type.
    OwnSelection(Selection, String, String),
    /// Read whatever the compositor is offering as a selection.
    ReadSelection(Selection),
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
    /// Everything received since the last drain.
    Input(Vec<Input>),
    /// A dmabuf paint was attempted, and either worked or could not be set up.
    Painted(Result<(), String>),
    /// What the client could read from a selection, if anything.
    Selection(Option<String>),
    /// The last command finished.
    Done,
}

/// The client thread's body: connect, then serve commands until told to stop.
fn client_thread(
    socket: &Path,
    title: &str,
    commands: &Receiver<ClientCommand>,
    replies: &Sender<Reply>,
) {
    let mut client = Client::connect(socket, title);
    if replies.send(Reply::Connected).is_err() {
        return;
    }

    let mut size = (0, 0);
    while let Ok(command) = commands.recv() {
        let reply = match command {
            ClientCommand::Configure => {
                size = client.wait_for_configure();
                Reply::Configured(size.0, size.1)
            }
            ClientCommand::Title(title) => {
                client.set_title(&title);
                Reply::Done
            }
            ClientCommand::Paint(rgba) => {
                client.paint(size, rgba);
                Reply::Done
            }
            ClientCommand::PaintDmabuf(rgba) => Reply::Painted(client.paint_dmabuf(size, rgba)),
            ClientCommand::Drain => Reply::Input(client.drain()),
            ClientCommand::AwaitFrame => {
                client.await_frame();
                Reply::Done
            }
            ClientCommand::Pump => {
                client.pump();
                Reply::Done
            }
            ClientCommand::OwnSelection(selection, mime, text) => {
                client.own_selection(selection, &mime, &text);
                Reply::Done
            }
            ClientCommand::ReadSelection(selection) => {
                Reply::Selection(client.read_selection(selection))
            }
            ClientCommand::Stop => {
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
    toplevel: XdgToplevel,
    /// Held so the compositor keeps sending to them.
    _keyboard: WlKeyboard,
    _pointer: WlPointer,
    /// The selection managers, and this client's devices on them.
    data_manager: WlDataDeviceManager,
    primary_manager: ZwpPrimarySelectionDeviceManagerV1,
    data_device: WlDataDevice,
    primary_device: ZwpPrimarySelectionDeviceV1,
    /// Held so a selection this client owns survives the request that set it.
    /// Dropping a source withdraws the selection.
    sources: Vec<Source>,
    /// The dmabuf global, bound at version 3 so the format and modifier events
    /// arrive — which is also how this fixture learns what the compositor will
    /// accept, rather than guessing. `None` if it was never advertised.
    dmabuf: Option<ZwpLinuxDmabufV1>,
    /// Held so the buffer outlives the commit that referenced it.
    buffer: Option<(WlShmPool, WlBuffer)>,
    /// The same, for a dmabuf paint. The gbm buffer has to outlive the commit
    /// too, or the compositor is compositing freed memory.
    imported: Option<(Held, WlBuffer)>,
    events: Events,
}

/// Whatever produced the dmabuf, held for as long as the compositor might read
/// it — dropping the allocator or the memfd underneath a committed buffer means
/// compositing freed memory.
enum Held {
    /// Allocated on a render node with gbm.
    Gbm {
        _allocator: GbmAllocator<std::fs::File>,
        _buffer: GbmBuffer,
        dmabuf: Dmabuf,
    },
    /// Made from a sealed memfd, for machines with no render node at all.
    Udmabuf {
        fd: std::os::fd::OwnedFd,
        stride: u32,
    },
}

impl Held {
    /// The fourcc and raw modifier to declare for this buffer.
    fn format(&self) -> (u32, u64) {
        match self {
            Self::Gbm { dmabuf, .. } => {
                let format = dmabuf.format();
                (format.code as u32, u64::from(format.modifier))
            }
            // A memfd is linear by construction; there is no tiling to describe.
            Self::Udmabuf { .. } => (Fourcc::Argb8888 as u32, u64::from(Modifier::Linear)),
        }
    }

    /// Each plane as (fd, offset, stride). Both paths are single-plane here.
    fn planes(&self) -> Vec<(std::os::fd::BorrowedFd<'_>, u32, u32)> {
        match self {
            Self::Gbm { dmabuf, .. } => dmabuf
                .handles()
                .zip(dmabuf.offsets())
                .zip(dmabuf.strides())
                .map(|((fd, offset), stride)| (fd, offset, stride))
                .collect(),
            Self::Udmabuf { fd, stride } => vec![(fd.as_fd(), 0, *stride)],
        }
    }
}

/// A selection this client owns, kept alive for as long as it owns it.
enum Source {
    /// A `wl_data_source` backing the clipboard.
    Clipboard(WlDataSource),
    /// A `zwp_primary_selection_source_v1` backing the primary selection.
    Primary(ZwpPrimarySelectionSourceV1),
}

/// What the client has learned from the compositor.
#[derive(Default)]
struct Events {
    /// The size the compositor configured us to, if it has.
    configured: Option<(i32, i32)>,
    /// Whether an `xdg_surface.configure` arrived and was acked.
    surface_configured: bool,
    /// Built from the keymap the compositor sent, and updated from every
    /// `modifiers` event — which is how a real toolkit turns a keycode into a
    /// character, and therefore the only honest way to check one.
    xkb: Option<xkb::State>,
    /// Input received since the last drain.
    input: Vec<Input>,
    /// The dmabuf format and modifier pairs the compositor advertised.
    dmabuf_formats: Vec<(u32, Modifier)>,
    /// How many `wl_surface.frame` callbacks have fired.
    frames: u32,
    /// The selections the compositor is currently offering, if any.
    clipboard_offer: Option<WlDataOffer>,
    primary_offer: Option<ZwpPrimarySelectionOfferV1>,
    /// What this client hands over when asked for a selection it owns.
    offered: String,
    /// The most recent serial from an input event.
    ///
    /// `set_selection` has to cite one: a client may only take the clipboard in
    /// response to something the user did, and inventing a serial is how a
    /// compositor learns to distrust you.
    serial: u32,
}

impl Events {
    /// Compile the keymap the compositor handed over.
    ///
    /// Read positionally rather than by seeking: the fd may be a dup of one the
    /// compositor keeps, so moving its offset would be moving somebody else's.
    /// mmap is the usual answer here and is the one thing this workspace will
    /// not write, since it needs `unsafe`.
    fn adopt_keymap(&mut self, format: KeymapFormat, fd: std::os::fd::OwnedFd, size: u32) {
        assert_eq!(
            format,
            KeymapFormat::XkbV1,
            "the compositor must send an xkb keymap",
        );
        let file = std::fs::File::from(fd);
        let mut bytes = vec![0_u8; size as usize];
        file.read_exact_at(&mut bytes, 0).expect("the keymap reads");
        // The keymap is NUL-terminated on the wire.
        let text = String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_owned();

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the compositor's keymap compiles");
        self.xkb = Some(xkb::State::new(&keymap));
    }

    /// Choose a modifier the compositor advertised for `format`.
    ///
    /// `Linear` first, because this fixture writes the pixels from the CPU and
    /// a tiled buffer would come out scrambled. `Invalid` — "whatever the
    /// driver picks" — is the fallback for a compositor that only advertises
    /// the implicit modifier.
    fn pick_modifier(&self, format: Fourcc) -> Option<Modifier> {
        let code = format as u32;
        let offered = |wanted: Modifier| {
            self.dmabuf_formats
                .iter()
                .any(|&(advertised, modifier)| advertised == code && modifier == wanted)
        };
        [Modifier::Linear, Modifier::Invalid]
            .into_iter()
            .find(|&modifier| offered(modifier))
    }

    /// Name the keysym a keycode produces under the current modifiers.
    fn name(&self, evdev: u32) -> String {
        let state = self.xkb.as_ref().expect("a keymap arrived before any key");
        // The wire carries evdev codes; xkb numbers the same keys eight higher.
        xkb::keysym_get_name(state.key_get_one_sym(xkb::Keycode::new(evdev + 8)))
    }
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
        // Version 8 or better, so discrete scrolls arrive as `v120` steps
        // rather than only as accumulated values.
        let seat: WlSeat = globals.bind(&handle, 8..=9, ()).expect("wl_seat");
        // Version 3 exactly: it is the last version that announces formats up
        // front, and this fixture wants to be told what the compositor accepts
        // rather than assume. A real toolkit would use version 4 feedback.
        //
        // Optional, because a session whose EGL is unusable still serves the
        // protocol and still runs most of these tests. Only `paint_dmabuf`
        // needs it, and it says so when it is missing.
        let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&handle, 3..=3, ()).ok();

        // A missing global is the difference between "the app is slow" and "the
        // app hangs forever with no error", so check rather than assume.
        globals.contents().with_list(|list| {
            // `wl_data_device_manager` is here because GTK will not create a
            // seat without one, and refuses silently: no seat means no
            // keyboard, and input goes nowhere with nothing in the log to say
            // why. Nothing in waypuppet reads a selection yet.
            for interface in ["wl_seat", "wl_output", "wl_data_device_manager"] {
                assert!(
                    list.iter().any(|global| global.interface == interface),
                    "{interface} is not advertised",
                );
            }
        });

        // Version 3 for `wl_data_device_manager`, which is what a modern
        // toolkit binds; the selection requests used here exist at every
        // version, so this is about being ordinary rather than about need.
        let data_manager: WlDataDeviceManager = globals
            .bind(&handle, 3..=3, ())
            .expect("wl_data_device_manager");
        let primary_manager: ZwpPrimarySelectionDeviceManagerV1 = globals
            .bind(&handle, 1..=1, ())
            .expect("zwp_primary_selection_device_manager_v1");

        // waypuppet's seat always has both, so there is no need to wait for the
        // capabilities event before asking.
        let keyboard = seat.get_keyboard(&handle, ());
        let pointer = seat.get_pointer(&handle, ());
        let data_device = data_manager.get_data_device(&seat, &handle, ());
        let primary_device = primary_manager.get_device(&seat, &handle, ());

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
            _keyboard: keyboard,
            _pointer: pointer,
            data_manager,
            primary_manager,
            data_device,
            primary_device,
            sources: Vec::new(),
            dmabuf,
            buffer: None,
            imported: None,
            events: Events::default(),
        }
    }

    /// Ask to be told when this frame is presented, and wait for the answer.
    ///
    /// This is how every real toolkit paces itself: a callback is requested
    /// before a commit, and the next frame is not drawn until it fires. Waiting
    /// on it here is safe for the usual harness reason — the compositor is
    /// being ticked by whoever is waiting on this client.
    fn await_frame(&mut self) {
        self.events.frames = 0;
        self.surface.frame(&self.handle, ());
        self.surface.commit();
        while self.events.frames == 0 {
            self.queue
                .blocking_dispatch(&mut self.events)
                .expect("the connection is healthy");
        }
    }

    /// Rename the window and make sure the compositor has read the request.
    ///
    /// The roundtrip is what makes the caller's next assertion meaningful: it
    /// returns only once the compositor has processed everything sent before
    /// it, so "the title has not taken effect yet" is not an available excuse.
    fn set_title(&mut self, title: &str) {
        self.toplevel.set_title(title.to_owned());
        self.connection
            .roundtrip()
            .expect("the connection is healthy");
    }

    /// Answer whatever the compositor has sent since the last turn.
    ///
    /// A roundtrip rather than a bare dispatch, because the interesting case is
    /// a `send` event asking this client to write a selection it owns — and
    /// that write happens inside the dispatch, so it has to actually run.
    fn pump(&mut self) {
        self.connection
            .roundtrip()
            .expect("the connection is healthy");
        self.queue
            .dispatch_pending(&mut self.events)
            .expect("the connection is healthy");
    }

    /// Take ownership of a selection, offering `text` under `mime`.
    fn own_selection(&mut self, selection: Selection, mime: &str, text: &str) {
        self.events.offered.clear();
        self.events.offered.push_str(text);
        let serial = self.events.serial;

        let source = match selection {
            Selection::Clipboard => {
                let source = self.data_manager.create_data_source(&self.handle, ());
                source.offer(mime.to_owned());
                self.data_device.set_selection(Some(&source), serial);
                Source::Clipboard(source)
            }
            Selection::Primary => {
                let source = self.primary_manager.create_source(&self.handle, ());
                source.offer(mime.to_owned());
                self.primary_device.set_selection(Some(&source), serial);
                Source::Primary(source)
            }
        };
        self.sources.push(source);

        self.connection
            .roundtrip()
            .expect("the compositor accepted the selection");
    }

    /// Read whatever the compositor is offering as a selection.
    ///
    /// Blocks on the compositor filling a pipe, which is safe for the usual
    /// harness reason: whoever is waiting on this client is ticking the
    /// compositor on its behalf.
    fn read_selection(&mut self, selection: Selection) -> Option<String> {
        // The offer arrives as an event, so make sure everything sent so far
        // has been read before concluding there is nothing on offer.
        self.pump();

        let (reader, writer) = std::io::pipe().expect("a pipe");
        match selection {
            Selection::Clipboard => {
                self.events
                    .clipboard_offer
                    .as_ref()?
                    .receive(TEXT_MIME.to_owned(), writer.as_fd());
            }
            Selection::Primary => {
                self.events
                    .primary_offer
                    .as_ref()?
                    .receive(TEXT_MIME.to_owned(), writer.as_fd());
            }
        }
        self.connection.flush().expect("the request goes out");

        // Ours is the last copy of the write end besides the compositor's, and
        // the read below ends at EOF — which cannot arrive while this is open.
        drop(writer);

        let mut text = String::new();
        let mut reader = reader;
        reader
            .read_to_string(&mut text)
            .expect("the compositor writes the selection");
        Some(text)
    }

    /// Dispatch whatever has arrived and hand back the input in it.
    fn drain(&mut self) -> Vec<Input> {
        // A roundtrip rather than a bare dispatch: it guarantees everything the
        // compositor had already sent has been read, so an empty result means
        // "nothing was sent" rather than "nothing had arrived yet".
        self.connection
            .roundtrip()
            .expect("the connection is healthy");
        self.queue
            .dispatch_pending(&mut self.events)
            .expect("the connection is healthy");
        std::mem::take(&mut self.events.input)
    }

    /// Dispatch until the compositor configures us, and return the size.
    ///
    /// Waits for the *next* configure, not the last one seen: a resize is a
    /// second configure, and a version that returned the cached one would hand
    /// back the old size and make a resize test pass without resizing.
    fn wait_for_configure(&mut self) -> (i32, i32) {
        self.events.configured = None;
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

    /// Fill the window with one colour and commit it, through a dmabuf.
    ///
    /// This is the path a real toolkit takes: allocate on a render node with
    /// gbm, export the buffer object as a dmabuf, hand the file descriptors to
    /// the compositor over `zwp_linux_dmabuf_v1`, and attach what comes back.
    /// `wl_shm` proves none of it.
    fn paint_dmabuf(&mut self, (width, height): (i32, i32), rgba: [u8; 4]) -> Result<(), String> {
        let global = self
            .dmabuf
            .clone()
            .ok_or_else(|| "the compositor does not advertise zwp_linux_dmabuf_v1".to_owned())?;

        // The compositor told us what it accepts when we bound the global; a
        // format it did not name would be rejected, and rightly.
        let modifier = self
            .events
            .pick_modifier(Fourcc::Argb8888)
            .ok_or_else(|| "the compositor advertises no Argb8888 dmabuf format".to_owned())?;

        // gbm where there is a render node, udmabuf where there is not. Both
        // produce a linear buffer full of the same colour; only the allocator
        // differs, so the import under test is identical either way.
        let held = match allocate(width, height, modifier) {
            Ok((allocator, mut buffer)) => {
                fill(&mut buffer, rgba)?;
                let dmabuf = buffer.export().map_err(|error| {
                    format!("the buffer cannot be exported as a dmabuf: {error}")
                })?;
                Held::Gbm {
                    _allocator: allocator,
                    _buffer: buffer,
                    dmabuf,
                }
            }
            Err(gbm) => {
                let (fd, stride) = udmabuf(width, height, rgba)
                    .map_err(|udmabuf| format!("no gbm ({gbm}); no udmabuf ({udmabuf})"))?;
                Held::Udmabuf { fd, stride }
            }
        };

        let params = global.create_params(&self.handle, ());
        let (format_code, raw) = held.format();
        for (index, (fd, offset, stride)) in held.planes().into_iter().enumerate() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a plane index above u32::MAX is not a thing"
            )]
            params.add(
                fd,
                index as u32,
                offset,
                stride,
                (raw >> 32) as u32,
                (raw & 0xFFFF_FFFF) as u32,
            );
        }

        // `create_immed` rather than `create`: the reply is the buffer, so
        // there is nothing to dispatch for, and a compositor that refuses the
        // import says so as a protocol error rather than an event this fixture
        // would have to wait on.
        let wl_buffer = params.create_immed(
            width,
            height,
            format_code,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &self.handle,
            (),
        );

        self.surface.attach(Some(&wl_buffer), 0, 0);
        self.surface.damage(0, 0, width, height);
        self.surface.commit();
        self.imported = Some((held, wl_buffer));

        // Deliberately a panic, not an `Err`. Past this point the client has
        // done everything asked of it, so a protocol error here is the
        // compositor refusing the import — and returning it alongside the
        // "this machine cannot allocate" cases would let a real defect be
        // reported as an environment limitation and skipped.
        self.connection
            .roundtrip()
            .expect("the compositor accepted the dmabuf it advertised a format for");
        Ok(())
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

/// Create a dmabuf out of plain memory, with no GPU involved.
///
/// `udmabuf(7)` turns a sealed `memfd` into a dma-buf, which is the only way to
/// produce one on a machine with no render node — a hosted CI runner, for
/// instance. The buffer is linear by construction, so the pixels go in with an
/// ordinary write.
///
/// Returns the dmabuf fd and its stride, or a reason it could not be done.
fn udmabuf(width: i32, height: i32, rgba: [u8; 4]) -> Result<(std::os::fd::OwnedFd, u32), String> {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    // `_IOW('u', 0x42, struct udmabuf_create)`, where the struct is
    // { u32 memfd; u32 flags; u64 offset; u64 size; } — 24 bytes.
    const UDMABUF_CREATE: libc::c_ulong = 0x4018_7542;
    const UDMABUF_FLAGS_CLOEXEC: u32 = 0x01;
    // udmabuf refuses a memfd that could still shrink under it.
    const F_SEAL_SHRINK: libc::c_int = 0x0002;

    #[repr(C)]
    struct Create {
        memfd: u32,
        flags: u32,
        offset: u64,
        size: u64,
    }

    let stride = u32::try_from(width).map_err(|_| "negative width".to_owned())? * 4;
    let bytes =
        u64::from(stride) * u64::try_from(height).map_err(|_| "negative height".to_owned())?;
    // udmabuf allocates in whole pages.
    let page = 4096;
    let size = bytes.div_ceil(page) * page;

    // `Argb8888` is 0xAARRGGBB in a little-endian word, so the bytes run B, G, R, A.
    let [r, g, b, a] = rgba;
    let pixel = [b, g, r, a];
    let filled: Vec<u8> = pixel
        .iter()
        .copied()
        .cycle()
        .take(usize::try_from(bytes).map_err(|_| "buffer too large".to_owned())?)
        .collect();

    // Every call here is an unsafe libc entry point with no safe wrapper in the
    // dependency tree. Each one is checked, the file descriptors are wrapped in
    // `OwnedFd` as soon as they exist, and `Create` is a plain repr(C) struct
    // passed by pointer to an ioctl that only reads it.
    #[expect(
        unsafe_code,
        reason = "memfd_create, fcntl(F_ADD_SEALS) and the udmabuf ioctl have no safe wrapper here"
    )]
    let dmabuf = unsafe {
        let memfd = libc::memfd_create(
            c"waypuppet-dmabuf".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        );
        if memfd < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        let memfd = OwnedFd::from_raw_fd(memfd);

        let length = libc::off_t::try_from(size).map_err(|_| "buffer too large".to_owned())?;
        if libc::ftruncate(memfd.as_raw_fd(), length) < 0 {
            return Err(format!("ftruncate: {}", std::io::Error::last_os_error()));
        }
        let written = libc::pwrite(
            memfd.as_raw_fd(),
            filled.as_ptr().cast::<libc::c_void>(),
            filled.len(),
            0,
        );
        if written < 0 || usize::try_from(written).unwrap_or(0) != filled.len() {
            return Err(format!("pwrite: {}", std::io::Error::last_os_error()));
        }
        if libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, F_SEAL_SHRINK) < 0 {
            return Err(format!("F_ADD_SEALS: {}", std::io::Error::last_os_error()));
        }

        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/udmabuf")
            .map_err(|error| format!("/dev/udmabuf: {error}"))?;

        let create = Create {
            memfd: u32::try_from(memfd.as_raw_fd()).map_err(|_| "bad fd".to_owned())?,
            flags: UDMABUF_FLAGS_CLOEXEC,
            offset: 0,
            size,
        };
        let fd = libc::ioctl(device.as_raw_fd(), UDMABUF_CREATE, &raw const create);
        if fd < 0 {
            return Err(format!(
                "UDMABUF_CREATE: {}",
                std::io::Error::last_os_error()
            ));
        }
        OwnedFd::from_raw_fd(fd)
    };

    Ok((dmabuf, stride))
}

/// Allocate a linear gbm buffer on the first render node we may open.
///
/// A render node rather than a card node: card nodes want privileges a
/// container usually does not have, and a client has no business holding them.
fn allocate(
    width: i32,
    height: i32,
    modifier: Modifier,
) -> Result<(GbmAllocator<std::fs::File>, GbmBuffer), String> {
    let mut tried = Vec::new();
    let nodes = std::fs::read_dir("/dev/dri")
        .map_err(|error| format!("/dev/dri is not readable: {error}"))?;

    for entry in nodes.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        {
            continue;
        }

        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| error.to_string())
            .and_then(|file| GbmDevice::new(file).map_err(|error| error.to_string()));
        let device = match opened {
            Ok(device) => device,
            Err(error) => {
                tried.push(format!("{}: {error}", path.display()));
                continue;
            }
        };

        // `LINEAR` so the buffer can be mapped and written from the CPU, which
        // is the only way this fixture can put a known colour in it. A real
        // toolkit would render into it on the GPU and not care.
        let mut allocator =
            GbmAllocator::new(device, GbmBufferFlags::RENDERING | GbmBufferFlags::LINEAR);
        match allocator.create_buffer(
            width.unsigned_abs(),
            height.unsigned_abs(),
            Fourcc::Argb8888,
            &[modifier],
        ) {
            Ok(buffer) => return Ok((allocator, buffer)),
            Err(error) => tried.push(format!("{}: {error}", path.display())),
        }
    }

    Err(if tried.is_empty() {
        "no render node in /dev/dri".to_owned()
    } else {
        tried.join("; ")
    })
}

/// Fill a mapped gbm buffer with one colour.
fn fill(buffer: &mut GbmBuffer, rgba: [u8; 4]) -> Result<(), String> {
    // `Argb8888` is 0xAARRGGBB in a little-endian word, so the bytes run
    // B, G, R, A — the same trap as the `wl_shm` path below.
    let [r, g, b, a] = rgba;
    let pixel = [b, g, r, a];
    let (width, height) = (buffer.width(), buffer.height());

    buffer
        .map_mut(0, 0, width, height, |mapping| {
            let stride = mapping.stride() as usize;
            let row = width as usize * 4;
            let pixels = mapping.buffer_mut();
            for y in 0..height as usize {
                let start = y * stride;
                for chunk in pixels[start..start + row].as_chunks_mut::<4>().0 {
                    chunk.copy_from_slice(&pixel);
                }
            }
        })
        .map_err(|error| format!("the gbm buffer cannot be mapped for writing: {error}"))
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

impl Dispatch<WlCallback, ()> for Events {
    fn event(
        state: &mut Self,
        _callback: &WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.frames += 1;
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Events {
    fn event(
        state: &mut Self,
        _dmabuf: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // The version 3 `modifier` event supersedes the version 1 `format`
        // event; both are recorded, because a compositor that only sent the
        // older one still means what it says.
        match event {
            zwp_linux_dmabuf_v1::Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                let raw = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
                state.dmabuf_formats.push((format, Modifier::from(raw)));
            }
            zwp_linux_dmabuf_v1::Event::Format { format } => {
                state.dmabuf_formats.push((format, Modifier::Invalid));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Events {
    fn event(
        _state: &mut Self,
        _params: &ZwpLinuxBufferParamsV1,
        _event: zwp_linux_buffer_params_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // `created` and `failed` only arrive for the deferred `create` request.
        // This fixture uses `create_immed`, where a refusal is a protocol error
        // rather than an event.
    }
}

impl Dispatch<WlSeat, ()> for Events {
    fn event(
        _state: &mut Self,
        _seat: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // Capabilities and the seat's name are not interesting here: waypuppet
        // always offers a keyboard and a pointer, and the client asked for both
        // up front.
    }
}

impl Dispatch<WlKeyboard, ()> for Events {
    fn event(
        state: &mut Self,
        _keyboard: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                state.adopt_keymap(
                    format.into_result().expect("a known keymap format"),
                    fd,
                    size,
                );
            }
            // Modifiers arrive before the key they apply to, which is what
            // makes `shift+a` resolve to `A` below rather than to `a`.
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = state.xkb.as_mut() {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            // Any serial from a real input event will do to cite when taking a
            // selection; the enter that comes with focus is the earliest one.
            wl_keyboard::Event::Enter { serial, .. } => state.serial = serial,
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                serial,
                ..
            } => {
                state.serial = serial;
                let pressed =
                    key_state.into_result().expect("a known key state") == KeyState::Pressed;
                let name = state.name(key);
                state.input.push(Input::Key { name, pressed });
            }
            _ => {}
        }
    }
}

/// Round a surface-local coordinate to a whole pixel.
///
/// The wire carries fixed-point; waypuppet only ever aims at whole pixels, so
/// anything else here would be a bug rather than a rounding question.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the clamp above is what makes the cast exact"
)]
fn whole(value: f64) -> i32 {
    // Saturating rather than `as`: a coordinate outside i32 is a bug worth
    // seeing as a wildly wrong assertion rather than as a wrapped one.
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

impl Dispatch<WlPointer, ()> for Events {
    fn event(
        state: &mut Self,
        _pointer: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => state.input.push(Input::Enter {
                x: whole(surface_x),
                y: whole(surface_y),
            }),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.input.push(Input::Motion {
                x: whole(surface_x),
                y: whole(surface_y),
            }),
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => state.input.push(Input::Button {
                code: button,
                pressed: button_state.into_result().expect("a known button state")
                    == ButtonState::Pressed,
            }),
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let axis = axis.into_result().expect("a known axis");
                state.input.push(Input::Scroll {
                    horizontal: i32::from(axis == wl_pointer::Axis::HorizontalScroll) * value120,
                    vertical: i32::from(axis == wl_pointer::Axis::VerticalScroll) * value120,
                });
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------- selections --

impl Dispatch<WlDataDevice, ()> for Events {
    fn event(
        state: &mut Self,
        _device: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // `DataOffer` introduces the object; `Selection` says which one is the
        // clipboard, and a `None` means the selection was withdrawn. Only the
        // second is worth recording — an offer nobody named is a drag, and
        // there is no drag in a session with one client.
        if let wl_data_device::Event::Selection { id } = event {
            state.clipboard_offer = id;
        }
    }

    // The offer arrives as a brand new object, so the queue needs to be told
    // what to attach to it.
    wayland_client::event_created_child!(Events, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for Events {
    fn event(
        _state: &mut Self,
        _offer: &WlDataOffer,
        _event: wl_data_offer::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // The `offer` events name the available mime types. This fixture asks
        // for one type and finds out whether it was there by whether the read
        // produces anything, so there is nothing to record.
    }
}

impl Dispatch<WlDataSource, ()> for Events {
    fn event(
        state: &mut Self,
        _source: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { mime_type: _, fd } = event {
            // Writing then dropping is the whole protocol: the close is the
            // EOF the reader is waiting for. A client that forgot the drop
            // would hang the compositor's read, which is exactly why that read
            // is on the event loop rather than inline.
            let mut file = std::fs::File::from(fd);
            let _ = file.write_all(state.offered.as_bytes());
        }
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for Events {
    fn event(
        state: &mut Self,
        _device: &ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_device_v1::Event::Selection { id } = event {
            state.primary_offer = id;
        }
    }

    wayland_client::event_created_child!(Events, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE
            => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for Events {
    fn event(
        _state: &mut Self,
        _offer: &ZwpPrimarySelectionOfferV1,
        _event: zwp_primary_selection_offer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, ()> for Events {
    fn event(
        state: &mut Self,
        _source: &ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_source_v1::Event::Send { mime_type: _, fd } = event {
            let mut file = std::fs::File::from(fd);
            let _ = file.write_all(state.offered.as_bytes());
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

ignore_events!(
    WlCompositor,
    WlSurface,
    WlShm,
    WlShmPool,
    WlBuffer,
    // Neither manager sends anything; they exist to make devices and sources.
    WlDataDeviceManager,
    ZwpPrimarySelectionDeviceManagerV1,
);
