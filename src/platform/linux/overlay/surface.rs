//! The overlay's thread: its own Wayland connection, the layer surface and the loop that
//! draws the scene into `wl_shm` buffers. The loop waits on the Wayland socket and an
//! `eventfd` (the ink recorder rings it on every change), and commits only when the picture
//! changed, since `wf-recorder` writes a frame for every commit.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, EventQueue, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};

use super::raster::Raster;
use super::scene::{self, Scene};
use super::{Output, Shared, buffer_scale, pick_output};
use crate::capture::scrawl_log;
use crate::platform::linux::SharedCaptureTelemetry;
use crate::screens::Rect;

/// How often the loop looks while ink changes with time (about a frame of a 120 Hz display).
const ANIMATING_MS: i32 = 8;
/// How often it looks otherwise, to catch changes nobody rings for (an undone stroke).
const IDLE_MS: i32 = 50;
/// A frame callback not seen after this long is taken as withheld: draw anyway.
const FRAME_WAIT: Duration = Duration::from_millis(100);
/// How long the first configure may take.
const CONFIGURE_WAIT: Duration = Duration::from_secs(3);

/// What the thread starts from.
pub(super) struct Launch {
    pub telemetry: Arc<SharedCaptureTelemetry>,
    pub shared: Arc<Shared>,
    pub wake: Arc<OwnedFd>,
    pub on: Option<Rect>,
    pub pointer: Option<[f64; 2]>,
    /// Each monitor's rect and scale from Hyprland, to notice a fractional scale.
    pub scale_of: Vec<(Rect, f64)>,
}

/// The thread's body: tells `ready` the output's rect once the surface is configured (or why
/// it can't be), then serves until stopped. A failure after that is logged: the recording
/// goes on without ink.
pub(super) fn run(launch: Launch, ready: SyncSender<Result<Rect, String>>) {
    let mut ready = Some(ready);
    if let Err(error) = serve(&launch, &mut ready) {
        match ready.take() {
            Some(ready) => {
                let _ = ready.send(Err(error));
            }
            None => scrawl_log(format!(
                "The ink overlay stopped: {error}. The recording goes on without ink"
            )),
        }
    }
}

struct Current {
    output: wl_output::WlOutput,
    /// Global points.
    rect: Rect,
    buffer_scale: i32,
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    current: Option<Current>,
    configured: bool,
    closed: bool,
    raster: Option<Raster>,
    last_scene: Option<Scene>,
    animating: bool,
    frame_pending: Option<Instant>,
    telemetry: Arc<SharedCaptureTelemetry>,
    shared: Arc<Shared>,
    scale_of: Vec<(Rect, f64)>,
    logged_fraction: bool,
}

fn serve(launch: &Launch, ready: &mut Option<SyncSender<Result<Rect, String>>>) -> Result<(), String> {
    let conn =
        Connection::connect_to_env().map_err(|error| format!("no Wayland connection: {error}"))?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)
        .map_err(|error| format!("could not list the Wayland globals: {error}"))?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|error| format!("the compositor has no wl_compositor: {error}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|_| {
        "the compositor has no zwlr_layer_shell_v1 (wlr-layer-shell), which the ink overlay needs"
            .to_string()
    })?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|error| format!("the compositor has no wl_shm: {error}"))?;
    let pool = SlotPool::new(4096, &shm)
        .map_err(|error| format!("could not make a shared-memory pool: {error}"))?;
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        layer: None,
        current: None,
        configured: false,
        closed: false,
        raster: None,
        last_scene: None,
        animating: false,
        frame_pending: None,
        telemetry: Arc::clone(&launch.telemetry),
        shared: Arc::clone(&launch.shared),
        scale_of: launch.scale_of.clone(),
        logged_fraction: false,
    };
    // The outputs' logical rects arrive with xdg-output, a round trip after they are listed.
    for _ in 0..2 {
        queue
            .roundtrip(&mut app)
            .map_err(|error| format!("Wayland round trip failed: {error}"))?;
    }
    app.publish_outputs();
    let outputs = app.outputs();
    let rects: Vec<Output> = outputs.iter().map(|(_, o)| *o).collect();
    let index = pick_output(&rects, launch.on, launch.pointer)
        .ok_or("the compositor lists no outputs")?;
    app.place_on(&outputs[index].0, outputs[index].1, &qh)?;

    let started = Instant::now();
    while !app.configured {
        if launch.shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if app.closed {
            return Err("the compositor closed the ink surface".into());
        }
        if started.elapsed() > CONFIGURE_WAIT {
            return Err("the compositor never configured the ink surface".into());
        }
        pump(&mut queue, &mut app, &launch.wake, 50)?;
    }
    let screen = app.current.as_ref().map_or(outputs[index].1.rect, |c| c.rect);
    if let Some(ready) = ready.take() {
        let _ = ready.send(Ok(screen));
    }

    loop {
        if launch.shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if app.closed {
            return Err("the compositor closed the ink surface".into());
        }
        let moved = launch
            .shared
            .move_to
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(rect) = moved {
            app.move_to(rect, &qh)?;
        }
        app.draw(&qh)?;
        let timeout = if app.animating { ANIMATING_MS } else { IDLE_MS };
        pump(&mut queue, &mut app, &launch.wake, timeout)?;
    }
}

/// Dispatches what is pending, then waits up to `timeout_ms` for the compositor or the wake-up
/// `eventfd` and dispatches what that brought.
fn pump(
    queue: &mut EventQueue<App>,
    app: &mut App,
    wake: &OwnedFd,
    timeout_ms: i32,
) -> Result<(), String> {
    let lost = |error: &dyn std::fmt::Display| format!("the Wayland connection failed: {error}");
    queue.dispatch_pending(app).map_err(|e| lost(&e))?;
    if let Err(error) = queue.flush()
        && !matches!(&error, smithay_client_toolkit::reexports::client::backend::WaylandError::Io(io)
            if io.kind() == std::io::ErrorKind::WouldBlock)
    {
        return Err(lost(&error));
    }
    if let Some(guard) = queue.prepare_read() {
        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
        if fds[0].revents & libc::POLLIN != 0 {
            if let Err(error) = guard.read()
                && !matches!(&error, smithay_client_toolkit::reexports::client::backend::WaylandError::Io(io)
                    if io.kind() == std::io::ErrorKind::WouldBlock)
            {
                return Err(lost(&error));
            }
        } else {
            drop(guard);
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let mut count = 0_u64;
            unsafe { libc::read(wake.as_raw_fd(), (&raw mut count).cast(), 8) };
        }
    }
    queue.dispatch_pending(app).map_err(|e| lost(&e))?;
    Ok(())
}

impl App {
    /// Every output with its logical rect and the integer scale it reports.
    fn outputs(&self) -> Vec<(wl_output::WlOutput, Output)> {
        self.output_state
            .outputs()
            .filter_map(|output| {
                let info = self.output_state.info(&output)?;
                let rect = match (info.logical_position, info.logical_size) {
                    (Some((x, y)), Some((w, h))) => [f64::from(x), f64::from(y), f64::from(w), f64::from(h)],
                    // No xdg-output: where the output says it is, as big as its mode in points.
                    _ => {
                        let mode = info.modes.iter().find(|m| m.current)?;
                        let scale = f64::from(info.scale_factor.max(1));
                        [
                            f64::from(info.location.0),
                            f64::from(info.location.1),
                            f64::from(mode.dimensions.0) / scale,
                            f64::from(mode.dimensions.1) / scale,
                        ]
                    }
                };
                Some((
                    output,
                    Output {
                        rect,
                        scale: info.scale_factor,
                    },
                ))
            })
            .collect()
    }

    fn publish_outputs(&self) {
        *self
            .shared
            .outputs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            self.outputs().into_iter().map(|(_, o)| o).collect();
    }

    /// The buffer scale for `output`: its integer one, or 1 when the monitor is scaled by a
    /// fraction (logged once).
    fn scale_for(&mut self, output: Output) -> i32 {
        let monitor = self
            .scale_of
            .iter()
            .find(|(rect, _)| (0..4).all(|i| (rect[i] - output.rect[i]).abs() < 1.0))
            .map(|(_, scale)| *scale);
        let (scale, note) = buffer_scale(output.scale, monitor);
        if let Some(note) = note
            && !self.logged_fraction
        {
            self.logged_fraction = true;
            scrawl_log(format!("Ink overlay: {note}"));
        }
        scale
    }

    /// Makes the layer surface over `output`.
    fn place_on(
        &mut self,
        output: &wl_output::WlOutput,
        info: Output,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let scale = self.scale_for(info);
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("scrawl-ink"),
            Some(output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        // An empty input region: every click and hover goes to what is underneath.
        let region = Region::new(&self.compositor)
            .map_err(|error| format!("could not make an input region: {error}"))?;
        layer.set_input_region(Some(region.wl_region()));
        let _ = layer.set_buffer_scale(scale as u32);
        layer.commit();
        self.layer = Some(layer);
        self.current = Some(Current {
            output: output.clone(),
            rect: info.rect,
            buffer_scale: scale,
        });
        self.configured = false;
        self.raster = None;
        self.last_scene = None;
        self.frame_pending = None;
        Ok(())
    }

    /// Moves to the output `rect` belongs to, if it isn't this one.
    fn move_to(&mut self, rect: Rect, qh: &QueueHandle<Self>) -> Result<(), String> {
        let outputs = self.outputs();
        let list: Vec<Output> = outputs.iter().map(|(_, o)| *o).collect();
        let Some(index) = pick_output(&list, Some(rect), None) else {
            return Ok(());
        };
        let (output, info) = &outputs[index];
        if self
            .current
            .as_ref()
            .is_some_and(|c| c.output == *output && c.rect == info.rect)
        {
            return Ok(());
        }
        // Dropping the old surface destroys it; the new one maps on the other output.
        self.layer = None;
        self.place_on(output, *info, qh)
    }

    /// The scene now, and whether it changes with time alone.
    fn scene(&self, origin: [f64; 2]) -> (Scene, bool) {
        let t = &self.telemetry;
        if !t.active.load(Ordering::Acquire) {
            return (Scene::default(), false);
        }
        let now_ms = t
            .started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            .as_millis() as u64;
        let frame = *t
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let strokes = t
            .strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let live = t
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let target = [
            (frame.x - origin[0]) as f32,
            (frame.y - origin[1]) as f32,
            frame.width as f32,
            frame.height as f32,
        ];
        let tip = t.shift_held.load(Ordering::Acquire).then(|| {
            let [px, py] = *t
                .pointer_screen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            [px - origin[0] as f32, py - origin[1] as f32]
        });
        (
            scene::build(&strokes, live.as_ref(), now_ms, target, tip),
            scene::animating(&strokes, live.as_ref(), now_ms),
        )
    }

    /// Draws and commits if the picture changed and the compositor is ready for a frame.
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(origin) = self.current.as_ref().map(|c| [c.rect[0], c.rect[1]]) else {
            return Ok(());
        };
        if !self.configured || self.raster.is_none() {
            return Ok(());
        }
        let (scene, animating) = self.scene(origin);
        self.animating = animating;
        if self.last_scene.as_ref() == Some(&scene) {
            return Ok(());
        }
        // One frame at a time: the next waits for the compositor's callback.
        if self
            .frame_pending
            .is_some_and(|since| since.elapsed() < FRAME_WAIT)
        {
            return Ok(());
        }
        let (Some(raster), Some(layer)) = (self.raster.as_mut(), self.layer.as_ref()) else {
            return Ok(());
        };
        let damage = raster.draw(&scene);
        self.last_scene = Some(scene);
        let Some(damage) = damage else {
            return Ok(());
        };
        let (width, height) = (raster.width(), raster.height());
        let (buffer, canvas) = self
            .pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .map_err(|error| format!("could not make a buffer: {error}"))?;
        canvas.copy_from_slice(raster.pixels());
        let surface = layer.wl_surface();
        surface.damage_buffer(damage.x0, damage.y0, damage.width(), damage.height());
        surface.frame(qh, surface.clone());
        buffer
            .attach_to(surface)
            .map_err(|error| format!("could not attach a buffer: {error}"))?;
        layer.commit();
        self.frame_pending = Some(Instant::now());
        Ok(())
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.frame_pending = None;
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.publish_outputs();
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.publish_outputs();
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.publish_outputs();
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        let (width, height) = match configure.new_size {
            (w, h) if w > 0 && h > 0 => (w, h),
            // The compositor left the size to us: the output's own.
            _ => (current.rect[2] as u32, current.rect[3] as u32),
        };
        let scale = current.buffer_scale.max(1) as u32;
        let (pixels_w, pixels_h) = (width * scale, height * scale);
        let same = self
            .raster
            .as_ref()
            .is_some_and(|r| r.width() == pixels_w as i32 && r.height() == pixels_h as i32);
        if !same {
            self.raster = Some(Raster::new(pixels_w, pixels_h, scale));
            self.last_scene = None;
        }
        self.configured = true;
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);
