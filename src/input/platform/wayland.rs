use std::{
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer,
    delegate_pointer_constraints, delegate_registry, delegate_relative_pointer, delegate_seat,
    delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tokio::sync::mpsc;
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{zwp_confined_pointer_v1, zwp_locked_pointer_v1},
    relative_pointer::zv1::client::zwp_relative_pointer_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use super::CaptureEvent;
use crate::input::protocol::{Edge, PointerInput};

const EDGE_THICKNESS: u32 = 2;
const POOL_BYTES: usize = 512 * 1024;

pub struct Capture {
    pub events: mpsc::UnboundedReceiver<CaptureEvent>,
    active: Arc<AtomicBool>,
}

impl Capture {
    pub async fn new() -> Result<Self> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let active = Arc::new(AtomicBool::new(false));
        let thread_active = active.clone();

        thread::Builder::new()
            .name("lan-cat-wayland-capture".into())
            .spawn(move || {
                let result = run_capture(events_tx, thread_active, ready_tx.clone());
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .context("spawn Wayland capture thread")?;

        tokio::task::spawn_blocking(move || ready_rx.recv())
            .await
            .context("join Wayland capture startup")?
            .context("Wayland capture exited during startup")??;
        Ok(Self { events, active })
    }

    pub fn release(&self) {
        self.active.store(false, Ordering::Release);
    }
}

struct EdgeLayer {
    edge: Edge,
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
}

struct CaptureState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    _compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    relative_pointers: RelativePointerState,
    constraints: PointerConstraintsState,
    layers: Vec<EdgeLayer>,
    pointer: Option<wl_pointer::WlPointer>,
    relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    locked_pointer: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    hovered: Option<Edge>,
    position: f64,
    current_edge: Option<Edge>,
    active: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<CaptureEvent>,
}

fn run_capture(
    events: mpsc::UnboundedSender<CaptureEvent>,
    active: Arc<AtomicBool>,
    ready: std_mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    let connection = Connection::connect_to_env().context("connect to Wayland compositor")?;
    let (globals, mut queue) = registry_queue_init(&connection).context("read Wayland globals")?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor unavailable")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell unavailable")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm unavailable")?;

    let mut layers = Vec::new();
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let surface = compositor.create_surface(&qh);
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("lan-cat-cursor-edge"),
            None,
        );
        let (anchor, width, height) = match edge {
            Edge::Left => (
                Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
                EDGE_THICKNESS,
                0,
            ),
            Edge::Right => (
                Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
                EDGE_THICKNESS,
                0,
            ),
            Edge::Top => (
                Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                0,
                EDGE_THICKNESS,
            ),
            Edge::Bottom => (
                Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                0,
                EDGE_THICKNESS,
            ),
        };
        layer.set_anchor(anchor);
        layer.set_size(width, height);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        layers.push(EdgeLayer {
            edge,
            layer,
            width,
            height,
            configured: false,
        });
    }

    let pool = SlotPool::new(POOL_BYTES, &shm).context("create Wayland edge buffer pool")?;
    let mut state = CaptureState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        _compositor: compositor,
        shm,
        pool,
        relative_pointers: RelativePointerState::bind(&globals, &qh),
        constraints: PointerConstraintsState::bind(&globals, &qh),
        layers,
        pointer: None,
        relative_pointer: None,
        locked_pointer: None,
        hovered: None,
        position: 0.5,
        current_edge: None,
        active,
        events,
    };

    queue
        .roundtrip(&mut state)
        .context("configure Wayland edge surfaces")?;
    if state.pointer.is_none() {
        bail!("Wayland seat has no pointer capability");
    }
    ready.send(Ok(())).ok();

    loop {
        queue
            .dispatch_pending(&mut state)
            .context("dispatch Wayland pointer events")?;
        state.sync_release();
        queue.flush().context("flush Wayland capture requests")?;

        if let Some(read) = queue.prepare_read() {
            let mut descriptor = libc::pollfd {
                fd: connection.backend().poll_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Timeout also lets network cancellation release a locked pointer.
            let result = unsafe { libc::poll(&mut descriptor, 1, 20) };
            if result > 0 && descriptor.revents & libc::POLLIN != 0 {
                read.read().context("read Wayland pointer events")?;
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl CaptureState {
    fn layer_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<&EdgeLayer> {
        self.layers
            .iter()
            .find(|layer| layer.layer.wl_surface() == surface)
    }

    fn draw_layer(&mut self, layer: &LayerSurface, width: u32, height: u32) {
        let Some(bytes) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(4))
        else {
            return;
        };
        if bytes == 0 || bytes > POOL_BYTES {
            return;
        }
        if let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) {
            canvas.fill(0);
            layer.wl_surface().attach(Some(buffer.wl_buffer()), 0, 0);
            layer
                .wl_surface()
                .damage_buffer(0, 0, width as i32, height as i32);
            layer.commit();
        }
    }

    fn sync_release(&mut self) {
        if self.current_edge.is_some() && !self.active.load(Ordering::Acquire) {
            if let Some(lock) = self.locked_pointer.take() {
                lock.destroy();
            }
            self.current_edge = None;
        }
    }

    fn start_capture(&mut self, qh: &QueueHandle<Self>, edge: Edge) {
        if self.current_edge.is_some() {
            return;
        }
        let (Some(pointer), Some(layer)) = (
            self.pointer.as_ref(),
            self.layers.iter().find(|layer| layer.edge == edge),
        ) else {
            return;
        };
        let Ok(lock) = self.constraints.lock_pointer(
            layer.layer.wl_surface(),
            pointer,
            None,
            wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::Lifetime::Persistent,
            qh,
        ) else {
            return;
        };
        self.locked_pointer = Some(lock);
        self.current_edge = Some(edge);
        self.active.store(true, Ordering::Release);
        let _ = self.events.send(CaptureEvent::Begin {
            edge,
            position: self.position,
        });
    }
}

impl CompositorHandler for CaptureState {
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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

impl OutputHandler for CaptureState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for CaptureState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.layers.retain(|candidate| candidate.layer != *layer);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(index) = self
            .layers
            .iter()
            .position(|candidate| candidate.layer == *layer)
        else {
            return;
        };
        let edge = self.layers[index].edge;
        let (width, height) = match edge {
            Edge::Left | Edge::Right => (EDGE_THICKNESS, configure.new_size.1.max(1)),
            Edge::Top | Edge::Bottom => (configure.new_size.0.max(1), EDGE_THICKNESS),
        };
        self.layers[index].width = width;
        self.layers[index].height = height;
        self.layers[index].configured = true;
        self.draw_layer(layer, width, height);
    }
}

impl SeatHandler for CaptureState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.relative_pointer = self
                    .relative_pointers
                    .get_relative_pointer(&pointer, qh)
                    .ok();
                self.pointer = Some(pointer);
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.relative_pointer
                .take()
                .map(|pointer| pointer.destroy());
            self.pointer.take().map(|pointer| pointer.release());
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for CaptureState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let edge = self
                .layer_for_surface(&event.surface)
                .map(|layer| layer.edge);
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    if let Some(edge) = edge {
                        self.hovered = Some(edge);
                        if let Some(layer) = self.layer_for_surface(&event.surface) {
                            let span = match edge {
                                Edge::Left | Edge::Right => layer.height,
                                _ => layer.width,
                            }
                            .max(1);
                            let coordinate = match edge {
                                Edge::Left | Edge::Right => event.position.1,
                                _ => event.position.0,
                            };
                            self.position = (coordinate / f64::from(span)).clamp(0.0, 1.0);
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.current_edge.is_none() {
                        self.hovered = None;
                    }
                }
                PointerEventKind::Press { button, .. } if self.current_edge.is_some() => {
                    let _ = self
                        .events
                        .send(CaptureEvent::Pointer(PointerInput::Button {
                            button,
                            state: 1,
                        }));
                }
                PointerEventKind::Release { button, .. } if self.current_edge.is_some() => {
                    let _ = self
                        .events
                        .send(CaptureEvent::Pointer(PointerInput::Button {
                            button,
                            state: 0,
                        }));
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } if self.current_edge.is_some() => {
                    if !vertical.is_none() {
                        let _ = self.events.send(CaptureEvent::Pointer(PointerInput::Axis {
                            axis: 0,
                            value: vertical.absolute,
                        }));
                        if vertical.discrete != 0 {
                            let _ = self.events.send(CaptureEvent::Pointer(
                                PointerInput::AxisDiscrete120 {
                                    axis: 0,
                                    value: vertical.discrete * 120,
                                },
                            ));
                        }
                    }
                    if !horizontal.is_none() {
                        let _ = self.events.send(CaptureEvent::Pointer(PointerInput::Axis {
                            axis: 1,
                            value: horizontal.absolute,
                        }));
                        if horizontal.discrete != 0 {
                            let _ = self.events.send(CaptureEvent::Pointer(
                                PointerInput::AxisDiscrete120 {
                                    axis: 1,
                                    value: horizontal.discrete * 120,
                                },
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl RelativePointerHandler for CaptureState {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        let (dx, dy) = event.delta;
        if self.current_edge.is_none() {
            if let Some(edge) = self.hovered {
                let outward = match edge {
                    Edge::Left => dx < 0.0,
                    Edge::Right => dx > 0.0,
                    Edge::Top => dy < 0.0,
                    Edge::Bottom => dy > 0.0,
                };
                if outward {
                    self.start_capture(qh, edge);
                }
            }
        }
        if self.current_edge.is_some() {
            let _ = self
                .events
                .send(CaptureEvent::Pointer(PointerInput::Motion { dx, dy }));
        }
    }
}

impl PointerConstraintsHandler for CaptureState {
    fn confined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
    fn unconfined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
    fn locked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
    fn unlocked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
}

impl ShmHandler for CaptureState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for CaptureState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(CaptureState);
delegate_layer!(CaptureState);
delegate_output!(CaptureState);
delegate_pointer!(CaptureState);
delegate_pointer_constraints!(CaptureState);
delegate_registry!(CaptureState);
delegate_relative_pointer!(CaptureState);
delegate_seat!(CaptureState);
delegate_shm!(CaptureState);

pub struct Injector {
    connection: Connection,
    queue: wayland_client::EventQueue<InjectorState>,
    state: InjectorState,
    pointer: ZwlrVirtualPointerV1,
}

struct InjectorState;

impl Injector {
    pub async fn new() -> Result<Self> {
        let connection = Connection::connect_to_env().context("connect injector to Wayland")?;
        let (globals, queue) =
            registry_queue_init::<InjectorState>(&connection).context("read injector globals")?;
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| anyhow!("wlr-virtual-pointer protocol unavailable"))?;
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        Ok(Self {
            connection,
            queue,
            state: InjectorState,
            pointer,
        })
    }

    pub fn begin(&mut self, edge: Edge, position: f64) -> Result<()> {
        let extent = 65_535;
        let along = (position.clamp(0.0, 1.0) * f64::from(extent)) as u32;
        let inset = 64;
        let (x, y) = match edge {
            Edge::Left => (inset, along),
            Edge::Right => (extent - inset, along),
            Edge::Top => (along, inset),
            Edge::Bottom => (along, extent - inset),
        };
        self.pointer.motion_absolute(now_ms(), x, y, extent, extent);
        self.pointer.frame();
        self.flush()
    }

    pub fn apply(&mut self, input: PointerInput) -> Result<()> {
        match input {
            PointerInput::Motion { dx, dy } => self.pointer.motion(now_ms(), dx, dy),
            PointerInput::Button { button, state } => {
                let state = if state == 0 {
                    wl_pointer::ButtonState::Released
                } else {
                    wl_pointer::ButtonState::Pressed
                };
                self.pointer.button(now_ms(), button, state);
            }
            PointerInput::Axis { axis, value } => {
                self.pointer.axis(now_ms(), wayland_axis(axis)?, value)
            }
            PointerInput::AxisDiscrete120 { axis, value } => {
                self.pointer.axis_discrete(
                    now_ms(),
                    wayland_axis(axis)?,
                    f64::from(value) / 120.0,
                    value / 120,
                );
            }
        }
        self.pointer.frame();
        self.flush()
    }

    pub fn end(&mut self) -> Result<()> {
        self.pointer.frame();
        self.flush()
    }

    fn flush(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatch virtual pointer")?;
        self.connection.flush().context("flush virtual pointer")?;
        Ok(())
    }
}

fn wayland_axis(axis: u8) -> Result<wl_pointer::Axis> {
    match axis {
        0 => Ok(wl_pointer::Axis::VerticalScroll),
        1 => Ok(wl_pointer::Axis::HorizontalScroll),
        _ => bail!("invalid Wayland pointer axis {axis}"),
    }
}

fn now_ms() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InjectorState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(InjectorState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(InjectorState: ignore ZwlrVirtualPointerV1);
delegate_noop!(InjectorState: ignore wl_buffer::WlBuffer);
