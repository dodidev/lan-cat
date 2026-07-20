use std::{
    collections::HashSet,
    fs::{self, File},
    io,
    os::fd::{AsFd, AsRawFd},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
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
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_surface,
    },
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{zwp_confined_pointer_v1, zwp_locked_pointer_v1},
    relative_pointer::zv1::client::zwp_relative_pointer_v1,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use super::{ALL_EDGE_MASK, CaptureEvent, edge_mask};
use crate::input::protocol::{Edge, KeyboardInput, PointerInput};

const EDGE_THICKNESS: u32 = 2;
const POOL_BYTES: usize = 512 * 1024;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const XKB_MOD_SHIFT: u32 = 1 << 0;
const XKB_MOD_CONTROL: u32 = 1 << 2;
const XKB_MOD_ALT: u32 = 1 << 3;
const XKB_MOD_LOGO: u32 = 1 << 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxInputEvent {
    _time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

pub struct Capture {
    pub events: mpsc::UnboundedReceiver<CaptureEvent>,
    active: Arc<AtomicBool>,
    allowed_edges: Arc<AtomicU8>,
}

impl Capture {
    pub async fn new() -> Result<Self> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let active = Arc::new(AtomicBool::new(false));
        let allowed_edges = Arc::new(AtomicU8::new(ALL_EDGE_MASK));
        let thread_active = active.clone();
        let thread_allowed_edges = allowed_edges.clone();
        let capture_events_tx = events_tx.clone();

        thread::Builder::new()
            .name("lan-cat-wayland-capture".into())
            .spawn(move || {
                let result = run_capture(
                    capture_events_tx,
                    thread_active,
                    thread_allowed_edges,
                    ready_tx.clone(),
                );
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .context("spawn Wayland capture thread")?;
        spawn_local_input_monitor(events_tx);

        tokio::task::spawn_blocking(move || ready_rx.recv())
            .await
            .context("join Wayland capture startup")?
            .context("Wayland capture exited during startup")??;
        Ok(Self {
            events,
            active,
            allowed_edges,
        })
    }

    pub fn release(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn set_allowed_edge(&self, edge: Option<Edge>) {
        self.allowed_edges
            .store(edge.map_or(0, edge_mask), Ordering::Release);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.release();
    }
}

fn spawn_local_input_monitor(events: mpsc::UnboundedSender<CaptureEvent>) {
    thread::Builder::new()
        .name("lan-cat-linux-input-monitor".into())
        .spawn(move || {
            if let Err(error) = run_local_input_monitor(events) {
                tracing::warn!(%error, "local input takeover disabled");
            }
        })
        .ok();
}

fn run_local_input_monitor(events: mpsc::UnboundedSender<CaptureEvent>) -> Result<()> {
    let mut devices = open_input_devices("/dev/input")?;
    if devices.is_empty() {
        bail!("no readable /dev/input/event* devices");
    }
    let event_size = size_of::<LinuxInputEvent>();
    let mut buffer = vec![0_u8; event_size * 32];

    loop {
        let mut pollfds: Vec<_> = devices
            .iter()
            .map(|device| libc::pollfd {
                fd: device.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ready =
            unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 1000) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            bail!("poll /dev/input failed: {error}");
        }

        for (index, pollfd) in pollfds.iter().enumerate().rev() {
            if pollfd.revents & libc::POLLNVAL != 0 {
                devices.swap_remove(index);
                continue;
            }
            if pollfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let bytes = unsafe {
                libc::read(
                    devices[index].as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if bytes < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::WouldBlock
                    && error.kind() != io::ErrorKind::Interrupted
                {
                    devices.swap_remove(index);
                }
                continue;
            }
            for event in physical_inputs(&buffer[..bytes as usize]) {
                let _ = events.send(event);
            }
        }

        if devices.is_empty() {
            devices = open_input_devices("/dev/input")?;
        }
    }
}

fn open_input_devices(path: impl AsRef<Path>) -> Result<Vec<File>> {
    let mut devices = Vec::new();
    let entries = fs::read_dir(path)?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("event") {
            continue;
        }
        match File::open(entry.path()) {
            Ok(file) => {
                let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                if flags >= 0 {
                    unsafe {
                        libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }
                devices.push(file);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(_) => {}
        }
    }
    Ok(devices)
}

fn physical_inputs(buffer: &[u8]) -> Vec<CaptureEvent> {
    let mut events = Vec::new();
    for chunk in buffer.chunks_exact(size_of::<LinuxInputEvent>()) {
        let event = unsafe { chunk.as_ptr().cast::<LinuxInputEvent>().read_unaligned() };
        if matches!(event.kind, EV_KEY | EV_REL | EV_ABS) && event.value != 0 {
            events.push(CaptureEvent::LocalInput);
        }
        if event.kind == EV_KEY && matches!(event.value, 0 | 1) {
            events.push(CaptureEvent::LocalKeyboard(KeyboardInput {
                key: u32::from(event.code),
                state: event.value as u32,
            }));
        }
    }
    events
}

#[cfg(test)]
fn has_physical_input(buffer: &[u8]) -> bool {
    physical_inputs(buffer)
        .iter()
        .any(|event| matches!(event, CaptureEvent::LocalInput))
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
    keyboard: Option<wl_keyboard::WlKeyboard>,
    relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    locked_pointer: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    hovered: Option<Edge>,
    pointer_enter_serial: Option<u32>,
    position: f64,
    current_edge: Option<Edge>,
    active: Arc<AtomicBool>,
    allowed_edges: Arc<AtomicU8>,
    events: mpsc::UnboundedSender<CaptureEvent>,
}

fn run_capture(
    events: mpsc::UnboundedSender<CaptureEvent>,
    active: Arc<AtomicBool>,
    allowed_edges: Arc<AtomicU8>,
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
        keyboard: None,
        relative_pointer: None,
        locked_pointer: None,
        hovered: None,
        pointer_enter_serial: None,
        position: 0.5,
        current_edge: None,
        active,
        allowed_edges,
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
            if let Some(edge) = self.current_edge.take()
                && let Some(layer) = self.layers.iter().find(|layer| layer.edge == edge)
            {
                layer
                    .layer
                    .set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.layer.commit();
            }
        }
    }

    fn start_capture(&mut self, qh: &QueueHandle<Self>, edge: Edge) {
        if self.current_edge.is_some()
            || self.allowed_edges.load(Ordering::Acquire) & edge_mask(edge) == 0
        {
            return;
        }
        let (Some(pointer), Some(layer)) = (
            self.pointer.as_ref(),
            self.layers
                .iter()
                .find(|layer| layer.edge == edge)
                .map(|layer| layer.layer.clone()),
        ) else {
            return;
        };
        let Ok(lock) = self.constraints.lock_pointer(
            layer.wl_surface(),
            pointer,
            None,
            wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::Lifetime::Persistent,
            qh,
        ) else {
            return;
        };
        self.locked_pointer = Some(lock);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        if let Some(serial) = self.pointer_enter_serial {
            pointer.set_cursor(serial, None, 0, 0);
        }
        self.current_edge = Some(edge);
        self.active.store(true, Ordering::Release);
        
        // Estimate screen dimensions from configured layers
        let (screen_width, screen_height) = self.estimate_screen_dimensions();
        
        let _ = self.events.send(CaptureEvent::Begin {
            edge,
            position: self.position,
            screen_width,
            screen_height,
        });
    }
    
    fn estimate_screen_dimensions(&self) -> (f64, f64) {
        // Get the largest width and height from all configured layers
        let mut max_width = 0;
        let mut max_height = 0;
        for layer in &self.layers {
            if layer.configured {
                match layer.edge {
                    Edge::Left | Edge::Right => {
                        max_height = max_height.max(layer.height);
                    }
                    Edge::Top | Edge::Bottom => {
                        max_width = max_width.max(layer.width);
                    }
                }
            }
        }
        // If no layers configured, use reasonable defaults (HD resolution)
        let width = if max_width > 0 { max_width } else { 1920 };
        let height = if max_height > 0 { max_height } else { 1080 };
        (width as f64, height as f64)
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
        } else if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(seat.get_keyboard(qh, ()));
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
            if let Some(pointer) = self.relative_pointer.take() {
                pointer.destroy();
            }
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        } else if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
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
                PointerEventKind::Enter { serial } => {
                    self.pointer_enter_serial = Some(serial);
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
                PointerEventKind::Motion { .. } => {
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
                    self.pointer_enter_serial = None;
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

impl Dispatch<wl_keyboard::WlKeyboard, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_keyboard::Event::Key {
            key,
            state: key_state,
            ..
        } = event
        else {
            return;
        };
        if state.current_edge.is_none() {
            return;
        }
        let key_state = match key_state {
            WEnum::Value(wl_keyboard::KeyState::Released) => 0,
            WEnum::Value(wl_keyboard::KeyState::Pressed) => 1,
            WEnum::Unknown(_) => return,
            _ => return,
        };
        let _ = state.events.send(CaptureEvent::Keyboard(KeyboardInput {
            key,
            state: key_state,
        }));
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
    _keyboard: wl_keyboard::WlKeyboard,
    held_keys: HashSet<u32>,
}

struct InjectorState {
    keyboard: ZwpVirtualKeyboardV1,
    keymap_ready: bool,
}

impl Injector {
    pub async fn new() -> Result<Self> {
        let connection = Connection::connect_to_env().context("connect injector to Wayland")?;
        let (globals, mut queue) =
            registry_queue_init::<InjectorState>(&connection).context("read injector globals")?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=7, ())
            .map_err(|_| anyhow!("Wayland seat unavailable"))?;
        let manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| anyhow!("wlr-virtual-pointer protocol unavailable"))?;
        let pointer = manager.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| anyhow!("virtual-keyboard protocol unavailable"))?;
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        let physical_keyboard = seat.get_keyboard(&qh, ());
        let mut state = InjectorState {
            keyboard,
            keymap_ready: false,
        };
        queue
            .roundtrip(&mut state)
            .context("read Wayland keyboard map")?;
        if !state.keymap_ready {
            bail!("Wayland seat did not provide an XKB keymap");
        }
        Ok(Self {
            connection,
            queue,
            state,
            pointer,
            _keyboard: physical_keyboard,
            held_keys: HashSet::new(),
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

    pub fn apply_keyboard(&mut self, input: KeyboardInput) -> Result<()> {
        self.state.keyboard.key(now_ms(), input.key, input.state);
        if input.state == 0 {
            self.held_keys.remove(&input.key);
        } else {
            self.held_keys.insert(input.key);
        }
        self.state
            .keyboard
            .modifiers(modifiers(&self.held_keys), 0, 0, 0);
        self.flush()
    }

    pub fn end(&mut self) -> Result<()> {
        for key in self.held_keys.drain() {
            self.state.keyboard.key(now_ms(), key, 0);
        }
        self.state.keyboard.modifiers(0, 0, 0, 0);
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

impl Drop for Injector {
    fn drop(&mut self) {
        let _ = self.end();
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

fn modifiers(held_keys: &HashSet<u32>) -> u32 {
    let mut mods = 0;
    for key in held_keys {
        mods |= match *key {
            42 | 54 => XKB_MOD_SHIFT,
            29 | 97 => XKB_MOD_CONTROL,
            56 | 100 => XKB_MOD_ALT,
            125 | 126 => XKB_MOD_LOGO,
            _ => 0,
        };
    }
    mods
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
impl Dispatch<wl_keyboard::WlKeyboard, ()> for InjectorState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_keyboard::Event::Keymap { format, fd, size } = event else {
            return;
        };
        if matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
            state.keyboard.keymap(1, fd.as_fd(), size);
            state.keymap_ready = true;
        }
    }
}
delegate_noop!(InjectorState: ignore wl_seat::WlSeat);
delegate_noop!(InjectorState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(InjectorState: ignore ZwlrVirtualPointerV1);
delegate_noop!(InjectorState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(InjectorState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(InjectorState: ignore wl_buffer::WlBuffer);

#[cfg(test)]
mod tests {
    use super::*;

    fn event_bytes(kind: u16, value: i32) -> Vec<u8> {
        let event = LinuxInputEvent {
            _time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            kind,
            code: 1,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&event as *const LinuxInputEvent).cast::<u8>(),
                size_of::<LinuxInputEvent>(),
            )
        };
        bytes.to_vec()
    }

    #[test]
    fn physical_keyboard_event_triggers_takeover() {
        assert!(has_physical_input(&event_bytes(EV_KEY, 1)));
    }

    #[test]
    fn sync_and_release_events_do_not_trigger_takeover() {
        assert!(!has_physical_input(&event_bytes(0, 0)));
        assert!(!has_physical_input(&event_bytes(EV_KEY, 0)));
    }

    #[test]
    fn modifier_keys_map_to_xkb_masks() {
        let held_keys = HashSet::from([125, 2]);
        assert_eq!(modifiers(&held_keys), XKB_MOD_LOGO);

        let held_keys = HashSet::from([42, 29, 56]);
        assert_eq!(
            modifiers(&held_keys),
            XKB_MOD_SHIFT | XKB_MOD_CONTROL | XKB_MOD_ALT
        );
    }

    #[test]
    fn physical_keyboard_events_include_takeover_and_key_state() {
        let events = physical_inputs(&event_bytes(EV_KEY, 1));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CaptureEvent::LocalInput))
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                CaptureEvent::LocalKeyboard(KeyboardInput { key: 1, state: 1 })
            )
        }));
    }
}
