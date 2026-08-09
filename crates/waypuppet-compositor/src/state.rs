//! Compositor state and the protocol handlers hung off it.
//!
//! Smithay's delegation wants one type to attach every handler impl to, so
//! [`State`] is a plain struct holding all of it. Splitting it up fights the
//! framework; keep it flat.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        input::{Axis, AxisSource, ButtonState, KeyState},
        renderer::utils::on_commit_buffer_handler,
    },
    delegate_compositor, delegate_dmabuf, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{Space, Window},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{
        Client, DisplayHandle,
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface},
    },
    utils::{
        Clock, Logical, Monotonic, Point as SmithayPoint, SERIAL_COUNTER, Serial,
        Size as SmithaySize, Transform,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states,
        },
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::{OutputHandler, OutputManagerState},
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tokio::sync::oneshot;
use waypuppet_proto::{Button, Failure, Point, Rect, Target, WindowId};

use crate::{
    Config, Frame, Renderer,
    bridge::Snapshot,
    input::{Keymap, Stroke},
};

/// How far a client scrolls per wheel click, in surface-local units.
///
/// The wire carries both this and a `v120` step count; toolkits differ over
/// which they read, so both have to be right. 15 is the conventional detent.
const WHEEL_STEP: f64 = 15.0;

/// One wheel click, in the `v120` units `wl_pointer` uses for discrete scrolls.
const WHEEL_V120: i32 = 120;

/// Advertise `zwp_linux_dmabuf_v1` for what `renderer` can import.
///
/// Not optional in practice: anything Skia-backed — which is the point of the
/// project — renders through EGL and has nowhere to put its buffers without
/// this. `wl_shm` stays supported for simpler clients.
///
/// Returns `None` when the renderer can import no dmabuf formats at all.
/// Advertising an empty global is worse than staying quiet: a toolkit that sees
/// the global assumes it can use it, and finding nothing usable is a harder
/// failure to read than never having been offered it.
fn dmabuf_global(display: &DisplayHandle, renderer: &mut Renderer) -> Option<DmabufState> {
    let formats = renderer.dmabuf_formats();
    if formats.iter().next().is_none() {
        tracing::warn!("the renderer imports no dmabuf formats; not offering the global");
        return None;
    }

    // The older path, and independent of the global below: some client stacks
    // look for `wl_drm` before they will consider hardware buffers at all.
    // Failing is survivable, so it is logged rather than propagated.
    if let Err(error) = renderer.bind_wl_display(display) {
        tracing::debug!(%error, "EGL will not answer for wl_drm buffers");
    }

    let mut state = DmabufState::new();
    // Version 4 feedback names the device a client should allocate on, which is
    // the answer to "which GPU?" that a bare format list cannot give. A software
    // renderer has no node to name, so it gets the version 3 global instead —
    // the formats are the same, only the advice is missing.
    let global = match renderer.node() {
        Some(node) => {
            let feedback = DmabufFeedbackBuilder::new(node.dev_id(), formats).build();
            match feedback {
                Ok(feedback) => {
                    state.create_global_with_default_feedback::<State>(display, &feedback)
                }
                Err(error) => {
                    tracing::debug!(%error, "no dmabuf feedback; falling back to version 3");
                    state.create_global::<State>(display, renderer.dmabuf_formats())
                }
            }
        }
        None => state.create_global::<State>(display, formats),
    };
    // The handle is only needed to disable or destroy the global, and this one
    // lives as long as the session does.
    let _: DmabufGlobal = global;

    Some(state)
}

/// What a client gets attached to it when it connects.
#[derive(Debug)]
pub struct ClientState {
    /// Per-client compositor bookkeeping, required by smithay.
    pub compositor_state: CompositorClientState,
    /// Shared with [`State`], so the control protocol can tell a process that
    /// has been spawned from one that has actually connected.
    connections: Arc<AtomicUsize>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {
        self.connections.fetch_add(1, Ordering::Relaxed);
        tracing::debug!("client connected");
    }

    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        self.connections.fetch_sub(1, Ordering::Relaxed);
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

/// A reply held back until the client draws.
///
/// This is the parked-sender pattern the bridge is built around: the sender
/// lives here only while the frame it is waiting for is still possible, so a
/// window that goes away, or a compositor thread that dies, fails the call
/// immediately rather than on its deadline.
struct Settling {
    /// The surface whose next commit answers this.
    surface: WlSurface,
    /// The commit count at the moment the events were delivered. The reply is
    /// sent on the first commit *after* this, which is what makes a client that
    /// repaints continuously still settle correctly: the question is never "has
    /// it gone quiet" but "has it drawn since".
    after: u64,
    /// The call waiting on the frame.
    reply: oneshot::Sender<Result<(), Failure>>,
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

    /// Keysym to keycode, so a chord can be turned into wire events.
    keymap: Keymap,
    /// Replies waiting on a client frame.
    settling: Vec<Settling>,

    /// Monotonic time source for frame callbacks.
    clock: Clock<Monotonic>,
    /// The renderer, or why there is not one.
    ///
    /// Built at startup rather than on first capture, because the
    /// `zwp_linux_dmabuf_v1` global advertises *this renderer's* formats and
    /// has to exist before the client connects. A session whose EGL is unusable
    /// still serves the protocol; it just says so immediately instead of at the
    /// first `shot`.
    renderer: Result<Renderer, String>,
    /// `zwp_linux_dmabuf_v1`, when there is a renderer to back it.
    dmabuf: Option<DmabufState>,
    /// The output's logical size and scale, as the control protocol reports
    /// them.
    reported: (waypuppet_proto::Size, f64),
    /// How many Wayland clients are currently connected.
    connections: Arc<AtomicUsize>,
    /// The output's logical size, which is also every window's size.
    output_size: SmithaySize<i32, Logical>,
    /// Source of the next [`WindowId`].
    next_window_id: u32,
}

impl State {
    /// Build the compositor state and register every global.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Compositor`] if xkb cannot compile
    /// [`crate::input::LAYOUT`], which means the xkb data files are missing.
    /// Failing here rather than at the first `key` call is deliberate: a
    /// session that cannot type is broken, and should say so while somebody is
    /// still looking at the startup output.
    pub fn new(display_handle: DisplayHandle, config: &Config) -> Result<Self, crate::Error> {
        let keymap = Keymap::new()?;
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();

        let mut seat = seat_state.new_wl_seat(&display_handle, "waypuppet");
        // The seat's keymap and `keymap` above have to be the same layout, or
        // the keycodes computed here would mean something else on the client's
        // copy — hence one `xkb_config` for both.
        //
        // Repeat rate and delay are the client's business once it has the
        // keymap — waypuppet never synthesises repeats — but the numbers still
        // have to be plausible, because clients read them.
        let _ = seat.add_keyboard(crate::input::xkb_config(), 200, 25);
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

        // Brought up here rather than on first capture: the dmabuf global
        // advertises this renderer's formats, and a global that appears after
        // the client has already looked at the registry may as well not exist.
        let mut renderer = Renderer::new(&output, config.render_node.as_deref())
            .map_err(|error| error.to_string());
        if let Err(why) = &renderer {
            tracing::warn!(
                %why,
                "no rendering device: captures will fail and dmabuf will not be offered",
            );
        }

        let dmabuf = renderer
            .as_mut()
            .ok()
            .and_then(|renderer| dmabuf_global(&display_handle, renderer));

        Ok(Self {
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
            keymap,
            settling: Vec::new(),
            clock: Clock::new(),
            renderer,
            dmabuf,
            reported: (config.output, config.scale),
            connections: Arc::new(AtomicUsize::new(0)),
            output_size,
            next_window_id: 1,
        })
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

    /// Per-client data for a newly accepted connection.
    #[must_use]
    pub fn new_client_state(&self) -> ClientState {
        ClientState {
            compositor_state: CompositorClientState::default(),
            connections: Arc::clone(&self.connections),
        }
    }

    /// Composite the current state and read it back.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::NoRenderDevice`] if EGL could not be brought up
    /// at startup, or [`crate::Error::Render`] if compositing or readback
    /// fails.
    pub fn capture(&mut self) -> Result<Frame, crate::Error> {
        // Destructured so the renderer can be borrowed mutably while the space
        // and output are borrowed immutably — they are disjoint fields, but
        // only a destructuring borrow tells the compiler that.
        let Self {
            renderer,
            output,
            space,
            ..
        } = self;

        // The startup failure is repeated verbatim rather than retried: if EGL
        // was unusable then, nothing since has changed it, and a fresh attempt
        // would only produce the same message more slowly.
        let renderer = renderer
            .as_mut()
            .map_err(|why| crate::Error::NoRenderDevice(why.clone()))?;

        let frame = renderer.capture(output, space)?;
        self.send_frames();
        Ok(frame)
    }

    /// Describe every window, for the control protocol.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let (output, scale) = self.reported;
        let mut windows: Vec<_> = self
            .space
            .elements()
            .filter_map(|window| {
                let toplevel = window.toplevel()?;
                let surface = toplevel.wl_surface();
                let record = self.records.get(surface)?;
                let geometry = self.space.element_geometry(window)?;
                let (title, app_id) = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|data| data.lock().ok())
                        .map_or((None, None), |data| {
                            (data.title.clone(), data.app_id.clone())
                        })
                });

                Some(waypuppet_proto::Window {
                    id: record.id,
                    title,
                    app_id,
                    geometry: Rect {
                        origin: Point {
                            x: geometry.loc.x,
                            y: geometry.loc.y,
                        },
                        size: waypuppet_proto::Size {
                            width: geometry.size.w.unsigned_abs(),
                            height: geometry.size.h.unsigned_abs(),
                        },
                    },
                    mapped: record.mapped,
                    frames: record.frames,
                })
            })
            .collect();
        // Stable order, so `ps` does not shuffle between calls.
        windows.sort_by_key(|window| window.id);

        Snapshot {
            output,
            scale,
            windows,
            connections: self.connections.load(Ordering::Relaxed),
        }
    }

    /// Tell every mapped surface that its last frame was presented.
    ///
    /// Clients throttle drawing on these callbacks, so a compositor that never
    /// sends them gets exactly one frame out of an animated client and then
    /// nothing.
    pub fn send_frames(&self) {
        let now = self.clock.now();
        for window in self.space.elements() {
            window.send_frame(&self.output, Duration::from(now), None, |_, _| {
                Some(self.output.clone())
            });
        }
    }

    // ------------------------------------------------------------- input --

    /// Deliver key chords, and name the surface whose next frame settles them.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoSuchWindow`] if nothing is focused, or
    /// [`Failure::UnmappableChord`] if a chord does not parse. Every chord is
    /// resolved before any is sent, so a typo in the third one does not leave
    /// the first two already delivered.
    pub fn deliver_key(&mut self, chords: &[String]) -> Result<WlSurface, Failure> {
        // Parsed before the window is looked up, so a typo is reported as a
        // typo rather than as whatever else happens to be wrong.
        let strokes = chords
            .iter()
            .map(|chord| self.keymap.chord(chord))
            .collect::<Result<Vec<_>, _>>()?;
        let surface = self.focused()?;
        for stroke in &strokes {
            self.send_stroke(stroke);
        }
        Ok(surface)
    }

    /// Type `text` as individual key events.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoSuchWindow`] if nothing is focused, or
    /// [`Failure::UnmappableChord`] naming the first character
    /// [`crate::input::LAYOUT`] cannot produce. Nothing is typed in that case:
    /// half a string is a worse answer than none.
    pub fn deliver_text(&mut self, text: &str) -> Result<WlSurface, Failure> {
        let strokes = text
            .chars()
            .map(|ch| self.keymap.character(ch))
            .collect::<Result<Vec<_>, _>>()?;
        let surface = self.focused()?;
        for stroke in &strokes {
            self.send_stroke(stroke);
        }
        Ok(surface)
    }

    /// Move the pointer to `to`, in `target`'s coordinates.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoSuchWindow`] if `target` names nothing.
    pub fn deliver_pointer(&mut self, to: Point, target: Target) -> Result<WlSurface, Failure> {
        let surface = self.settle_surface(target)?;
        let location = self.locate(to, target)?;
        self.move_pointer(location);
        Ok(surface)
    }

    /// Press and release a button, moving there first when `at` is given.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoSuchWindow`] if `target` names nothing.
    pub fn deliver_click(
        &mut self,
        at: Option<Point>,
        button: Button,
        target: Target,
    ) -> Result<WlSurface, Failure> {
        let surface = self.settle_surface(target)?;
        if let Some(at) = at {
            let location = self.locate(at, target)?;
            self.move_pointer(location);
        }

        let Some(pointer) = self.seat.get_pointer() else {
            return Ok(surface);
        };
        let time = self.now();
        // Linux input-event codes, which is what `wl_pointer.button` carries.
        let code = match button {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
        };

        for state in [ButtonState::Pressed, ButtonState::Released] {
            pointer.button(
                self,
                &ButtonEvent {
                    // From the seat's counter, never invented: a client that
                    // echoes a serial back at us has to be talking about an
                    // event we actually sent.
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                    button: code,
                    state,
                },
            );
            pointer.frame(self);
        }
        Ok(surface)
    }

    /// Scroll by whole wheel clicks.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoSuchWindow`] if nothing is focused.
    pub fn deliver_scroll(&mut self, dx: i32, dy: i32) -> Result<WlSurface, Failure> {
        let surface = self.settle_surface(Target::Focused)?;
        let Some(pointer) = self.seat.get_pointer() else {
            return Ok(surface);
        };

        let mut frame = AxisFrame::new(self.now()).source(AxisSource::Wheel);
        if dx != 0 {
            frame = frame
                .value(Axis::Horizontal, f64::from(dx) * WHEEL_STEP)
                .v120(Axis::Horizontal, dx * WHEEL_V120);
        }
        if dy != 0 {
            frame = frame
                .value(Axis::Vertical, f64::from(dy) * WHEEL_STEP)
                .v120(Axis::Vertical, dy * WHEEL_V120);
        }

        pointer.axis(self, frame);
        pointer.frame(self);
        Ok(surface)
    }

    /// Reconfigure a window to a new size, as a real compositor would.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::NoSuchWindow`] if `target` names nothing.
    pub fn deliver_resize(
        &mut self,
        size: waypuppet_proto::Size,
        target: Target,
    ) -> Result<WlSurface, Failure> {
        let window = self.target_window(target).ok_or(Failure::NoSuchWindow)?;
        let toplevel = window.toplevel().ok_or(Failure::NoSuchWindow)?;
        let wanted: SmithaySize<i32, Logical> = (
            i32::try_from(size.width).unwrap_or(i32::MAX),
            i32::try_from(size.height).unwrap_or(i32::MAX),
        )
            .into();

        // A configure is a request, not an assignment — the client acks it and
        // then draws at whatever size it chose. Settling on the next frame is
        // what makes the reply mean "the new size is on screen".
        toplevel.with_pending_state(|state| {
            state.size = Some(wanted);
        });
        toplevel.send_configure();
        Ok(toplevel.wl_surface().clone())
    }

    /// Give a freshly mapped window the keyboard and the pointer.
    ///
    /// Focus is granted on the first frame rather than at `new_toplevel`, so it
    /// arrives with content: a client told it has keyboard focus before it has
    /// drawn is entitled to be confused.
    ///
    /// The pointer is parked at the window's centre at the same time. A real
    /// desktop always has the pointer *somewhere*, and without a starting
    /// position `scroll` and a `click` with no coordinates would have no
    /// surface to go to — they would silently do nothing. The centre is
    /// arbitrary but fixed, which is what determinism requires.
    fn focus_on(&mut self, surface: &WlSurface) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(surface.clone()), SERIAL_COUNTER.next_serial());
        }

        let centre = self
            .window_for(surface)
            .and_then(|window| self.space.element_geometry(&window))
            .map(|geometry| {
                SmithayPoint::from((
                    f64::from(geometry.loc.x) + f64::from(geometry.size.w) / 2.0,
                    f64::from(geometry.loc.y) + f64::from(geometry.size.h) / 2.0,
                ))
            });
        if let Some(centre) = centre {
            self.move_pointer(centre);
        }
    }

    /// Send one keystroke: modifiers down, key down, key up, modifiers up.
    fn send_stroke(&mut self, stroke: &Stroke) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let time = self.now();

        let press = |state: &mut Self, keycode, key_state| {
            keyboard.input::<(), _>(
                state,
                keycode,
                key_state,
                SERIAL_COUNTER.next_serial(),
                time,
                // waypuppet binds no compositor-level shortcuts, so everything
                // it is asked to send is meant for the client.
                |_, _, _| FilterResult::Forward,
            );
        };

        for &modifier in &stroke.modifiers {
            press(self, modifier, KeyState::Pressed);
        }
        press(self, stroke.key, KeyState::Pressed);
        press(self, stroke.key, KeyState::Released);
        for &modifier in stroke.modifiers.iter().rev() {
            press(self, modifier, KeyState::Released);
        }
    }

    /// Put the pointer at `location` and tell whoever is under it.
    fn move_pointer(&mut self, location: SmithayPoint<f64, Logical>) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // Which surface is under the pointer is the compositor's own answer,
        // not the caller's: a click asked for at a window-relative point still
        // lands on whatever is actually drawn there.
        let under = self.space.element_under(location).and_then(|(window, at)| {
            window
                .toplevel()
                .map(|toplevel| (toplevel.wl_surface().clone(), at.to_f64()))
        });

        let event = MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: self.now(),
        };
        pointer.motion(self, under, &event);
        pointer.frame(self);
    }

    /// Where `to` lands in the global compositor space.
    fn locate(&self, to: Point, target: Target) -> Result<SmithayPoint<f64, Logical>, Failure> {
        let origin = match target {
            // Output coordinates are the global space, since the one output
            // sits at the origin.
            Target::Output => SmithayPoint::from((0, 0)),
            Target::Focused | Target::Window(_) => {
                let window = self.target_window(target).ok_or(Failure::NoSuchWindow)?;
                self.space
                    .element_geometry(&window)
                    .ok_or(Failure::NoSuchWindow)?
                    .loc
            }
        };
        Ok(SmithayPoint::from((
            f64::from(origin.x + to.x),
            f64::from(origin.y + to.y),
        )))
    }

    /// The window a request applies to.
    fn target_window(&self, target: Target) -> Option<Window> {
        match target {
            Target::Window(id) => {
                let (surface, _) = self.records.iter().find(|(_, record)| record.id == id)?;
                self.window_for(surface)
            }
            // Output is not a window, but a call still has to settle on one,
            // and with a single window on the desktop that is the same window.
            Target::Focused | Target::Output => {
                let focused = self
                    .seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.current_focus());
                match focused {
                    Some(surface) => self.window_for(&surface),
                    // Nothing focused yet is the normal state right up until
                    // the first frame, so fall back to the window that is
                    // about to get focus rather than failing the call.
                    None => self.first_mapped(),
                }
            }
        }
    }

    /// The lowest-numbered mapped window, for when nothing has focus yet.
    fn first_mapped(&self) -> Option<Window> {
        let mut mapped: Vec<_> = self
            .records
            .iter()
            .filter(|(_, record)| record.mapped)
            .collect();
        mapped.sort_by_key(|(_, record)| record.id);
        let (surface, _) = mapped.first()?;
        self.window_for(surface)
    }

    /// The focused surface, for keyboard input.
    fn focused(&self) -> Result<WlSurface, Failure> {
        let window = self
            .target_window(Target::Focused)
            .ok_or(Failure::NoSuchWindow)?;
        Ok(window
            .toplevel()
            .ok_or(Failure::NoSuchWindow)?
            .wl_surface()
            .clone())
    }

    /// The surface whose next frame settles a call against `target`.
    fn settle_surface(&self, target: Target) -> Result<WlSurface, Failure> {
        let window = self.target_window(target).ok_or(Failure::NoSuchWindow)?;
        Ok(window
            .toplevel()
            .ok_or(Failure::NoSuchWindow)?
            .wl_surface()
            .clone())
    }

    /// Milliseconds since the compositor started, for event timestamps.
    fn now(&self) -> u32 {
        // Wrapping is fine and expected: `wl_pointer` and `wl_keyboard`
        // timestamps have an undefined base and are only ever compared.
        u32::try_from(Duration::from(self.clock.now()).as_millis() % u128::from(u32::MAX))
            .unwrap_or(0)
    }

    // ----------------------------------------------------------- settling --

    /// Hold `reply` until `surface` commits another frame.
    ///
    /// The sender lives in [`State::settling`] and nowhere else, so every way
    /// the frame can stop being possible — the window going away, the
    /// compositor thread dying, the caller giving up — releases it.
    pub fn settle_on(&mut self, surface: WlSurface, reply: oneshot::Sender<Result<(), Failure>>) {
        let Some(record) = self.records.get(&surface) else {
            // The window went away between delivering the events and parking
            // the reply. Nothing will ever commit, so say so now.
            let _ = reply.send(Err(Failure::ClientGone));
            return;
        };
        self.settling.push(Settling {
            after: record.frames,
            surface,
            reply,
        });
    }

    /// Answer everything waiting on `surface` having drawn.
    fn wake_settled(&mut self, surface: &WlSurface, frames: u64) {
        let mut waiting = Vec::with_capacity(self.settling.len());
        for entry in self.settling.drain(..) {
            if &entry.surface == surface && frames > entry.after {
                let _ = entry.reply.send(Ok(()));
            } else {
                waiting.push(entry);
            }
        }
        self.settling = waiting;
    }

    /// Fail everything waiting on a window that has gone away.
    fn abandon_settled(&mut self, surface: &WlSurface) {
        let mut waiting = Vec::with_capacity(self.settling.len());
        for entry in self.settling.drain(..) {
            if &entry.surface == surface {
                let _ = entry.reply.send(Err(Failure::ClientGone));
            } else {
                waiting.push(entry);
            }
        }
        self.settling = waiting;
    }

    /// Drop replies nobody is listening for any more.
    ///
    /// A caller that hit its settle timeout dropped its receiver, which closes
    /// the sender here. Without this sweep those entries would accumulate for
    /// the life of the session — a slow leak rather than a hang, but a leak.
    pub fn prune_settling(&mut self) {
        self.settling.retain(|entry| !entry.reply.is_closed());
    }

    /// How many replies are parked, for tests.
    #[must_use]
    pub fn settling(&self) -> usize {
        self.settling.len()
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

        let Some(record) = self.records.get_mut(&root) else {
            return;
        };
        record.frames += 1;
        let frames = record.frames;
        let newly_mapped = !record.mapped;
        if newly_mapped {
            record.mapped = true;
            tracing::info!(id = record.id.0, "window mapped");
        }

        if newly_mapped {
            self.focus_on(&root);
        }
        self.wake_settled(&root, frames);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf
            .as_mut()
            .expect("the global is only created when there is a state to back it")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // The client is asking whether this buffer is usable, and the protocol
        // lets us answer honestly. Importing it now rather than discovering the
        // problem mid-render turns "the window is blank" into a protocol-level
        // failure the client can act on.
        let imported = self
            .renderer
            .as_mut()
            .map_err(|why| why.clone())
            .and_then(|renderer| renderer.import_dmabuf(&dmabuf).map_err(|e| e.to_string()));

        match imported {
            Ok(()) => {
                // Only fails if the client died between asking and being
                // answered, which needs nothing from us.
                let _ = notifier.successful::<Self>();
            }
            Err(why) => {
                tracing::warn!(%why, "rejecting a client's dmabuf");
                notifier.failed();
            }
        }
    }
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
        // The frame these were waiting for can no longer happen, so they are
        // answered now rather than on their deadlines. Tying a parked sender's
        // lifetime to the condition still being possible is the whole reason
        // failure-mode B mostly does not exist here.
        self.abandon_settled(surface.wl_surface());
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
delegate_dmabuf!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_xdg_shell!(State);
