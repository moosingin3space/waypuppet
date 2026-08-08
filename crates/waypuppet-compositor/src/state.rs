//! Compositor state and the protocol handlers hung off it.
//!
//! Smithay's delegation wants one type to attach every handler impl to, so
//! [`State`] is a plain struct holding all of it. Splitting it up fights the
//! framework; keep it flat.

use std::collections::HashMap;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{Space, Window},
    input::{Seat, SeatHandler, SeatState, keyboard::XkbConfig},
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{
        Client, DisplayHandle,
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface},
    },
    utils::{Logical, Serial, Size as SmithaySize, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states,
        },
        output::{OutputHandler, OutputManagerState},
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
    },
};
use waypuppet_proto::WindowId;

use crate::Config;

/// What a client gets attached to it when it connects.
#[derive(Debug, Default)]
pub struct ClientState {
    /// Per-client compositor bookkeeping, required by smithay.
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {
        tracing::debug!("client connected");
    }

    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        tracing::debug!(?reason, "client disconnected");
    }
}

/// What waypuppet tracks about a window beyond what `Space` already knows.
///
/// The commit counter is the load-bearing part: `wait --frames` and
/// settle-on-frame are both claims about this number, so it is maintained at
/// the point of truth rather than inferred later.
#[derive(Debug)]
pub struct Record {
    /// The identifier handed out to clients of the control protocol.
    pub id: WindowId,
    /// How many buffers this surface has committed.
    pub frames: u64,
    /// Whether a buffer has ever been committed, which is what "mapped"
    /// means to everything downstream.
    pub mapped: bool,
}

/// Everything the compositor owns.
#[expect(
    missing_debug_implementations,
    reason = "smithay's state types are not uniformly Debug"
)]
pub struct State {
    /// Handle to the wayland display, for talking to clients.
    pub display_handle: DisplayHandle,
    /// Set to `false` to stop the event loop.
    pub running: bool,

    /// The globals.
    pub compositor_state: CompositorState,
    /// Shared-memory buffers, for clients that render on the CPU.
    pub shm_state: ShmState,
    /// `xdg_shell`, which is how a client gets a window at all.
    pub xdg_shell_state: XdgShellState,
    /// `wl_output` plus `xdg_output`.
    pub output_manager_state: OutputManagerState,
    /// Seat bookkeeping. Input injection goes through the seat below.
    pub seat_state: SeatState<Self>,

    /// The single seat. Synthetic input is delivered here, which is the same
    /// path a real keyboard would take.
    pub seat: Seat<Self>,
    /// The single output. Fixed for the session's lifetime, because a capture
    /// is only comparable against another capture from the same output.
    pub output: Output,
    /// Where windows are.
    pub space: Space<Window>,
    /// Per-window bookkeeping, keyed by the toplevel's surface.
    pub records: HashMap<WlSurface, Record>,

    /// The output's logical size, which is also every window's size.
    output_size: SmithaySize<i32, Logical>,
    /// Source of the next [`WindowId`].
    next_window_id: u32,
}

impl State {
    /// Build the compositor state and register every global.
    #[must_use]
    pub fn new(display_handle: DisplayHandle, config: &Config) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();

        let mut seat = seat_state.new_wl_seat(&display_handle, "waypuppet");
        // Repeat rate and delay are the client's business once it has the
        // keymap — waypuppet never synthesises repeats — but the numbers still
        // have to be plausible, because clients read them.
        let _ = seat.add_keyboard(XkbConfig::default(), 200, 25);
        seat.add_pointer();

        let output = Output::new(
            "waypuppet-0".to_owned(),
            PhysicalProperties {
                // Reported in millimetres. Zero means "unknown", which is
                // honest for an output that does not exist.
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "waypuppet".to_owned(),
                model: "headless".to_owned(),
            },
        );
        output.create_global::<Self>(&display_handle);

        let output_size: SmithaySize<i32, Logical> = (
            i32::try_from(config.output.width).unwrap_or(i32::MAX),
            i32::try_from(config.output.height).unwrap_or(i32::MAX),
        )
            .into();
        let mode = OutputMode {
            // The mode is in physical pixels; `output_size` stays logical,
            // because that is the space window geometry and pointer input live
            // in. Conflating the two is the classic source of off-by-scale bugs.
            size: output_size.to_physical_precise_round(config.scale),
            // 60Hz in mHz. Nothing paces off this — frames are drawn on
            // demand — but clients read it and some divide by it.
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(config.scale)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        Self {
            display_handle,
            running: true,
            compositor_state,
            shm_state,
            xdg_shell_state,
            output_manager_state,
            seat_state,
            seat,
            output,
            space,
            records: HashMap::new(),
            output_size,
            next_window_id: 1,
        }
    }

    /// Find the window whose toplevel owns `surface`.
    #[must_use]
    pub fn window_for(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == surface)
            })
            .cloned()
    }

    /// Hand out the next window identifier.
    fn allocate_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        id
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Smithay tracks buffer damage and scale for us, but only if told about
        // every commit before anything else looks at the surface.
        on_commit_buffer_handler::<Self>(surface);

        // Subsurface commits arrive here too; the bookkeeping below is about
        // toplevels, so walk up to the root first.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(window) = self.window_for(&root) {
            window.on_commit();

            // The initial configure has to wait for the client's first,
            // buffer-less commit: that is the point in the xdg_shell handshake
            // where the client is ready to be told how big it is. Sending it
            // earlier is a protocol error, and forgetting to send it at all is
            // the classic reason a client hangs at startup with no diagnostic.
            let configured = with_states(&root, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .and_then(|data| data.lock().ok().map(|data| data.initial_configure_sent))
                    .unwrap_or(false)
            });

            if !configured {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_configure();
                }
                return;
            }
        }

        if let Some(record) = self.records.get_mut(&root) {
            record.frames += 1;
            if !record.mapped {
                record.mapped = true;
                tracing::info!(id = record.id.0, "window mapped");
            }
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.allocate_id();
        let wl_surface = surface.wl_surface().clone();

        // One window, filling the output. Placement becomes a real question
        // only if multiple windows ever land.
        let size = self.output_size;
        surface.with_pending_state(|state| {
            state.size = Some(size);
        });

        let window = Window::new_wayland_window(surface);
        self.space.map_element(window, (0, 0), true);

        self.records.insert(
            wl_surface,
            Record {
                id,
                frames: 0,
                mapped: false,
            },
        );
        tracing::debug!(id = id.0, "toplevel created");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(record) = self.records.remove(surface.wl_surface()) {
            tracing::debug!(id = record.id.0, "toplevel destroyed");
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // Popups render as part of their parent's tree. Nothing to place until
        // there is a reason to treat them separately.
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // waypuppet is the only input source, so there is nothing to grab
        // input away from.
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        // Popups are not placed independently yet, but the token must be
        // acknowledged or the client waits forever for a reply.
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_xdg_shell!(State);
