use std::{
    ffi::{c_double, c_int, c_long, c_void},
    ptr,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use super::{ALL_EDGE_MASK, CaptureEvent, edge_mask};
use crate::input::protocol::{Edge, KeyboardInput, PointerInput};

type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: c_double,
    y: c_double,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    width: c_double,
    height: c_double,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const SCROLL_WHEEL: u32 = 22;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const TAP_DISABLED_TIMEOUT: u32 = 0xffff_fffe;
const TAP_DISABLED_USER: u32 = 0xffff_ffff;
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;
const FIELD_KEYCODE: u32 = 9;
const FIELD_SCROLL_Y: u32 = 11;
const FIELD_SCROLL_X: u32 = 12;
const FIELD_SOURCE_USER_DATA: u32 = 42;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const SYNTHETIC_EVENT_TAG: c_long = 0x6c616e636174;
const FLAG_CAPS_LOCK: u64 = 1 << 16;
const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_OPTION: u64 = 1 << 19;
const FLAG_COMMAND: u64 = 1 << 20;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events: u64,
        callback: unsafe extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> c_long;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: c_long);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        event_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        ...
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGWarpMouseCursorPosition(position: CGPoint) -> c_int;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayHideCursor(display: u32) -> c_int;
    fn CGDisplayShowCursor(display: u32) -> c_int;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(loop_ref: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRelease(value: *const c_void);
}

struct State {
    tap: usize,
    active: bool,
    edge: Edge,
    position: f64,
    allowed_edges: u8,
    events: mpsc::UnboundedSender<CaptureEvent>,
}

pub struct Capture {
    pub events: mpsc::UnboundedReceiver<CaptureEvent>,
    state: Arc<Mutex<State>>,
}

impl Capture {
    pub async fn new() -> Result<Self> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(State {
            tap: 0,
            active: false,
            edge: Edge::Right,
            position: 0.5,
            allowed_edges: ALL_EDGE_MASK,
            events: events_tx,
        }));
        let callback_state = Arc::into_raw(state.clone()) as usize;
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("lan-cat-macos-capture".into())
            .spawn(move || capture_thread(callback_state as *const Mutex<State>, startup_tx))?;
        startup_rx
            .recv()
            .context("macOS capture thread stopped during startup")??;
        Ok(Self { events, state })
    }

    pub fn release(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !state.active {
            return;
        }
        state.active = false;
        let point = edge_point(state.edge, state.position, 20.0);
        unsafe {
            CGDisplayShowCursor(CGMainDisplayID());
            CGWarpMouseCursorPosition(point);
        }
    }

    pub fn set_allowed_edge(&self, edge: Option<Edge>) {
        if let Ok(mut state) = self.state.lock() {
            state.allowed_edges = edge.map_or(0, edge_mask);
        }
    }

    pub fn allow_all_edges(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.allowed_edges = ALL_EDGE_MASK;
        }
    }

    pub fn screen_dimensions(&self) -> (f64, f64) {
        screen_dimensions()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.release();
    }
}

fn capture_thread(state: *const Mutex<State>, startup: std::sync::mpsc::SyncSender<Result<()>>) {
    let mask = [
        LEFT_DOWN,
        LEFT_UP,
        RIGHT_DOWN,
        RIGHT_UP,
        MOUSE_MOVED,
        LEFT_DRAGGED,
        RIGHT_DRAGGED,
        KEY_DOWN,
        KEY_UP,
        FLAGS_CHANGED,
        SCROLL_WHEEL,
        OTHER_DOWN,
        OTHER_UP,
        OTHER_DRAGGED,
    ]
    .into_iter()
    .fold(0, |mask, event| mask | (1_u64 << event));
    let tap = unsafe { CGEventTapCreate(0, 0, 0, mask, callback, state.cast_mut().cast()) };
    if tap.is_null() {
        let _ = startup.send(Err(anyhow::anyhow!(
            "macOS Accessibility and Input Monitoring permissions are required"
        )));
        unsafe {
            drop(Arc::from_raw(state));
        }
        return;
    }
    if let Ok(mut state) = unsafe { &*state }.lock() {
        state.tap = tap as usize;
    }
    let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap, 0) };
    if source.is_null() {
        let _ = startup.send(Err(anyhow::anyhow!(
            "cannot create macOS cursor event source"
        )));
        unsafe {
            CFRelease(tap);
            drop(Arc::from_raw(state));
        }
        return;
    }
    unsafe {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
    let _ = startup.send(Ok(()));
    unsafe {
        CFRunLoopRun();
        CFRelease(source);
        CFRelease(tap);
        drop(Arc::from_raw(state));
    }
}

unsafe extern "C" fn callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let state = unsafe { &*(user_info as *const Mutex<State>) };
    if matches!(event_type, TAP_DISABLED_TIMEOUT | TAP_DISABLED_USER) {
        if let Ok(state) = state.lock() {
            unsafe {
                CGEventTapEnable(state.tap as CFMachPortRef, true);
            }
        }
        return event;
    }
    let Ok(mut state) = state.lock() else {
        return event;
    };
    let synthetic = unsafe { CGEventGetIntegerValueField(event, FIELD_SOURCE_USER_DATA) }
        == SYNTHETIC_EVENT_TAG;
    if synthetic {
        return event;
    }
    if !state.active && is_motion(event_type) {
        let point = unsafe { CGEventGetLocation(event) };
        let dx = unsafe { CGEventGetIntegerValueField(event, FIELD_DELTA_X) } as f64;
        let dy = unsafe { CGEventGetIntegerValueField(event, FIELD_DELTA_Y) } as f64;
        if let Some((edge, position)) = crossed_edge(point, dx, dy)
            && state.allowed_edges & edge_mask(edge) != 0
        {
            state.active = true;
            state.edge = edge;
            state.position = position;
            unsafe {
                CGDisplayHideCursor(CGMainDisplayID());
            }
            let (screen_width, screen_height) = screen_dimensions();
            let _ = state.events.send(CaptureEvent::Begin {
                edge,
                position,
                screen_width,
                screen_height,
            });
            return ptr::null_mut();
        }
    }
    if !state.active && !synthetic && is_local_input(event_type) {
        let _ = state.events.send(CaptureEvent::LocalInput);
        if matches!(event_type, KEY_DOWN | KEY_UP) {
            let mac_key = unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) } as u16;
            if let Some(key) = mac_to_evdev(mac_key) {
                let _ = state
                    .events
                    .send(CaptureEvent::LocalKeyboard(KeyboardInput {
                        key,
                        state: u32::from(event_type == KEY_DOWN),
                    }));
            }
        }
    }
    if !state.active {
        return event;
    }
    if is_motion(event_type) {
        let point = edge_point(state.edge, state.position, 1.0);
        unsafe {
            CGWarpMouseCursorPosition(point);
        }
    }
    if matches!(event_type, KEY_DOWN | KEY_UP) {
        let mac_key = unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) } as u16;
        if let Some(key) = mac_to_evdev(mac_key) {
            let _ = state.events.send(CaptureEvent::Keyboard(KeyboardInput {
                key,
                state: u32::from(event_type == KEY_DOWN),
            }));
        }
        return ptr::null_mut();
    }
    if event_type == FLAGS_CHANGED {
        let mac_key = unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) } as u16;
        let flags = unsafe { CGEventGetFlags(event) };
        if let Some((key, mask)) = modifier_to_evdev(mac_key) {
            let _ = state.events.send(CaptureEvent::Keyboard(KeyboardInput {
                key,
                state: u32::from(flags & mask != 0),
            }));
        }
        return ptr::null_mut();
    }
    let pointer = match event_type {
        MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => {
            let dx = unsafe { CGEventGetIntegerValueField(event, FIELD_DELTA_X) } as f64;
            let dy = unsafe { CGEventGetIntegerValueField(event, FIELD_DELTA_Y) } as f64;
            Some(PointerInput::Motion { dx, dy })
        }
        LEFT_DOWN | LEFT_UP => Some(PointerInput::Button {
            button: BTN_LEFT,
            state: u32::from(event_type == LEFT_DOWN),
        }),
        RIGHT_DOWN | RIGHT_UP => Some(PointerInput::Button {
            button: BTN_RIGHT,
            state: u32::from(event_type == RIGHT_DOWN),
        }),
        OTHER_DOWN | OTHER_UP
            if unsafe { CGEventGetIntegerValueField(event, FIELD_BUTTON_NUMBER) } == 2 =>
        {
            Some(PointerInput::Button {
                button: BTN_MIDDLE,
                state: u32::from(event_type == OTHER_DOWN),
            })
        }
        SCROLL_WHEEL => Some(PointerInput::Axis {
            axis: if unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_Y) } != 0 {
                0
            } else {
                1
            },
            value: if unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_Y) } != 0 {
                (unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_Y) }) as f64
            } else {
                (unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_X) }) as f64
            },
        }),
        _ => None,
    };
    if let Some(pointer) = pointer {
        let _ = state.events.send(CaptureEvent::Pointer(pointer));
    }
    ptr::null_mut()
}

pub struct Injector {
    point: CGPoint,
    buttons: u8,
    held_keys: Vec<u32>,
}

impl Injector {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            point: edge_point(Edge::Left, 0.5, 20.0),
            buttons: 0,
            held_keys: Vec::new(),
        })
    }

    pub fn begin(&mut self, edge: Edge, position: f64) -> Result<()> {
        // Use larger inset to give cursor room to move in both directions
        self.point = edge_point(edge, position, 20.0);
        unsafe {
            CGWarpMouseCursorPosition(self.point);
        }
        post_mouse(MOUSE_MOVED, self.point, 0)
    }

    pub fn apply(&mut self, input: PointerInput) -> Result<()> {
        match input {
            PointerInput::Motion { dx, dy } => {
                let bounds = display_bounds();
                // Allow cursor to move very close to edges (within 0.5 pixels) to enable exit detection
                // while still preventing it from going completely off-screen
                self.point.x = (self.point.x + dx)
                    .clamp(bounds.origin.x - 0.5, bounds.origin.x + bounds.size.width + 0.5);
                self.point.y = (self.point.y + dy)
                    .clamp(bounds.origin.y - 0.5, bounds.origin.y + bounds.size.height + 0.5);
                let event_type = if self.buttons & 1 != 0 {
                    LEFT_DRAGGED
                } else if self.buttons & 2 != 0 {
                    RIGHT_DRAGGED
                } else if self.buttons & 4 != 0 {
                    OTHER_DRAGGED
                } else {
                    MOUSE_MOVED
                };
                post_mouse_with_delta(event_type, self.point, 0, dx, dy)?;
            }
            PointerInput::Button { button, state } => {
                let (bit, down, up, number) = match button {
                    BTN_LEFT => (1, LEFT_DOWN, LEFT_UP, 0),
                    BTN_RIGHT => (2, RIGHT_DOWN, RIGHT_UP, 1),
                    BTN_MIDDLE => (4, OTHER_DOWN, OTHER_UP, 2),
                    _ => return Ok(()),
                };
                if state == 0 {
                    self.buttons &= !bit;
                } else {
                    self.buttons |= bit;
                }
                post_mouse(if state == 0 { up } else { down }, self.point, number)?;
            }
            PointerInput::Axis { axis, value } => post_scroll(axis, value)?,
            PointerInput::AxisDiscrete120 { axis, value } => {
                post_scroll(axis, f64::from(value) / 120.0)?
            }
        }
        Ok(())
    }

    pub fn end(&mut self) -> Result<()> {
        for (bit, button) in [(1, BTN_LEFT), (2, BTN_RIGHT), (4, BTN_MIDDLE)] {
            if self.buttons & bit != 0 {
                self.apply(PointerInput::Button { button, state: 0 })?;
            }
        }
        for key in std::mem::take(&mut self.held_keys) {
            post_key(key, false)?;
        }
        Ok(())
    }

    pub fn left_entry_edge(&self, edge: Edge, input: PointerInput) -> bool {
        let PointerInput::Motion { dx, dy } = input else {
            return false;
        };
        let bounds = display_bounds();
        match edge {
            Edge::Left => dx < 0.0 && self.point.x <= bounds.origin.x + 0.5,
            Edge::Right => dx > 0.0 && self.point.x >= bounds.origin.x + bounds.size.width - 0.5,
            Edge::Top => dy < 0.0 && self.point.y <= bounds.origin.y + 0.5,
            Edge::Bottom => dy > 0.0 && self.point.y >= bounds.origin.y + bounds.size.height - 0.5,
        }
    }

    pub fn apply_keyboard(&mut self, input: KeyboardInput) -> Result<()> {
        if input.state == 0 {
            self.held_keys.retain(|key| *key != input.key);
            post_key(input.key, false)?;
        } else if !self.held_keys.contains(&input.key) {
            self.held_keys.push(input.key);
            post_key(input.key, true)?;
        }
        Ok(())
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

fn crossed_edge(point: CGPoint, dx: f64, dy: f64) -> Option<(Edge, f64)> {
    let bounds = display_bounds();
    let x = (point.x - bounds.origin.x) / bounds.size.width;
    let y = (point.y - bounds.origin.y) / bounds.size.height;
    if dx < 0.0 && x <= 0.001 {
        Some((Edge::Left, y.clamp(0.0, 1.0)))
    } else if dx > 0.0 && x >= 0.999 {
        Some((Edge::Right, y.clamp(0.0, 1.0)))
    } else if dy < 0.0 && y <= 0.001 {
        Some((Edge::Top, x.clamp(0.0, 1.0)))
    } else if dy > 0.0 && y >= 0.999 {
        Some((Edge::Bottom, x.clamp(0.0, 1.0)))
    } else {
        None
    }
}

fn edge_point(edge: Edge, position: f64, inset: f64) -> CGPoint {
    let bounds = display_bounds();
    match edge {
        Edge::Left => CGPoint {
            x: bounds.origin.x + inset,
            y: bounds.origin.y + position * bounds.size.height,
        },
        Edge::Right => CGPoint {
            x: bounds.origin.x + bounds.size.width - inset,
            y: bounds.origin.y + position * bounds.size.height,
        },
        Edge::Top => CGPoint {
            x: bounds.origin.x + position * bounds.size.width,
            y: bounds.origin.y + inset,
        },
        Edge::Bottom => CGPoint {
            x: bounds.origin.x + position * bounds.size.width,
            y: bounds.origin.y + bounds.size.height - inset,
        },
    }
}

fn display_bounds() -> CGRect {
    unsafe { CGDisplayBounds(CGMainDisplayID()) }
}

pub fn screen_dimensions() -> (f64, f64) {
    let bounds = display_bounds();
    (bounds.size.width, bounds.size.height)
}

fn is_motion(event: u32) -> bool {
    matches!(
        event,
        MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED
    )
}

fn is_local_input(event: u32) -> bool {
    matches!(
        event,
        LEFT_DOWN
            | LEFT_UP
            | RIGHT_DOWN
            | RIGHT_UP
            | MOUSE_MOVED
            | LEFT_DRAGGED
            | RIGHT_DRAGGED
            | KEY_DOWN
            | KEY_UP
            | FLAGS_CHANGED
            | SCROLL_WHEEL
            | OTHER_DOWN
            | OTHER_UP
            | OTHER_DRAGGED
    )
}

fn mac_to_evdev(key: u16) -> Option<u32> {
    Some(match key {
        0x00 => 30,  // A
        0x01 => 31,  // S
        0x02 => 32,  // D
        0x03 => 33,  // F
        0x04 => 35,  // H
        0x05 => 34,  // G
        0x06 => 44,  // Z
        0x07 => 45,  // X
        0x08 => 46,  // C
        0x09 => 47,  // V
        0x0b => 48,  // B
        0x0c => 16,  // Q
        0x0d => 17,  // W
        0x0e => 18,  // E
        0x0f => 19,  // R
        0x10 => 21,  // Y
        0x11 => 20,  // T
        0x12 => 2,   // 1
        0x13 => 3,   // 2
        0x14 => 4,   // 3
        0x15 => 5,   // 4
        0x16 => 7,   // 6
        0x17 => 6,   // 5
        0x18 => 13,  // =
        0x19 => 10,  // 9
        0x1a => 8,   // 7
        0x1b => 12,  // -
        0x1c => 9,   // 8
        0x1d => 11,  // 0
        0x1e => 27,  // ]
        0x1f => 24,  // O
        0x20 => 22,  // U
        0x21 => 26,  // [
        0x22 => 23,  // I
        0x23 => 25,  // P
        0x24 => 28,  // Enter
        0x25 => 38,  // L
        0x26 => 36,  // J
        0x27 => 40,  // '
        0x28 => 37,  // K
        0x29 => 39,  // ;
        0x2a => 43,  // \
        0x2b => 51,  // ,
        0x2c => 53,  // /
        0x2d => 49,  // N
        0x2e => 50,  // M
        0x2f => 52,  // .
        0x30 => 15,  // Tab
        0x31 => 57,  // Space
        0x32 => 41,  // `
        0x33 => 14,  // Backspace
        0x35 => 1,   // Escape
        0x36 => 126, // Right command
        0x37 => 125, // Left command
        0x38 => 42,  // Left shift
        0x39 => 58,  // Caps lock
        0x3a => 56,  // Left option
        0x3b => 29,  // Left control
        0x3c => 54,  // Right shift
        0x3d => 100, // Right option
        0x3e => 97,  // Right control
        0x7b => 105, // Left
        0x7c => 106, // Right
        0x7d => 108, // Down
        0x7e => 103, // Up
        _ => return None,
    })
}

fn modifier_to_evdev(key: u16) -> Option<(u32, u64)> {
    Some(match key {
        0x36 => (126, FLAG_COMMAND),
        0x37 => (125, FLAG_COMMAND),
        0x38 => (42, FLAG_SHIFT),
        0x39 => (58, FLAG_CAPS_LOCK),
        0x3a => (56, FLAG_OPTION),
        0x3b => (29, FLAG_CONTROL),
        0x3c => (54, FLAG_SHIFT),
        0x3d => (100, FLAG_OPTION),
        0x3e => (97, FLAG_CONTROL),
        _ => return None,
    })
}

fn evdev_to_mac(key: u32) -> Option<u16> {
    Some(match key {
        1 => 0x35,
        2 => 0x12,
        3 => 0x13,
        4 => 0x14,
        5 => 0x15,
        6 => 0x17,
        7 => 0x16,
        8 => 0x1a,
        9 => 0x1c,
        10 => 0x19,
        11 => 0x1d,
        12 => 0x1b,
        13 => 0x18,
        14 => 0x33,
        15 => 0x30,
        16 => 0x0c,
        17 => 0x0d,
        18 => 0x0e,
        19 => 0x0f,
        20 => 0x11,
        21 => 0x10,
        22 => 0x20,
        23 => 0x22,
        24 => 0x1f,
        25 => 0x23,
        26 => 0x21,
        27 => 0x1e,
        28 => 0x24,
        29 => 0x3b,
        30 => 0x00,
        31 => 0x01,
        32 => 0x02,
        33 => 0x03,
        34 => 0x05,
        35 => 0x04,
        36 => 0x26,
        37 => 0x28,
        38 => 0x25,
        39 => 0x29,
        40 => 0x27,
        41 => 0x32,
        42 => 0x38,
        43 => 0x2a,
        44 => 0x06,
        45 => 0x07,
        46 => 0x08,
        47 => 0x09,
        48 => 0x0b,
        49 => 0x2d,
        50 => 0x2e,
        51 => 0x2b,
        52 => 0x2f,
        53 => 0x2c,
        54 => 0x3c,
        56 => 0x3a,
        57 => 0x31,
        58 => 0x39,
        97 => 0x3e,
        100 => 0x3d,
        103 => 0x7e,
        105 => 0x7b,
        106 => 0x7c,
        108 => 0x7d,
        125 => 0x37,
        126 => 0x36,
        _ => return None,
    })
}

fn post_mouse(event_type: u32, point: CGPoint, button: u32) -> Result<()> {
    post_mouse_with_delta(event_type, point, button, 0.0, 0.0)
}

fn post_mouse_with_delta(
    event_type: u32,
    point: CGPoint,
    button: u32,
    dx: f64,
    dy: f64,
) -> Result<()> {
    let event = unsafe { CGEventCreateMouseEvent(ptr::null(), event_type, point, button) };
    if event.is_null() {
        bail!("cannot create macOS mouse event");
    }
    unsafe {
        CGEventSetIntegerValueField(event, FIELD_SOURCE_USER_DATA, SYNTHETIC_EVENT_TAG);
        if is_motion(event_type) {
            CGEventSetIntegerValueField(event, FIELD_DELTA_X, delta_field(dx));
            CGEventSetIntegerValueField(event, FIELD_DELTA_Y, delta_field(dy));
        }
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

fn delta_field(value: f64) -> c_long {
    if value > 0.0 {
        value.max(1.0) as c_long
    } else if value < 0.0 {
        value.min(-1.0) as c_long
    } else {
        0
    }
}

fn post_scroll(axis: u8, value: f64) -> Result<()> {
    let (vertical, horizontal) = if axis == 0 {
        (value as c_int, 0)
    } else {
        (0, value as c_int)
    };
    let event = unsafe { CGEventCreateScrollWheelEvent(ptr::null(), 0, 2, vertical, horizontal) };
    if event.is_null() {
        bail!("cannot create macOS scroll event");
    }
    unsafe {
        CGEventSetIntegerValueField(event, FIELD_SOURCE_USER_DATA, SYNTHETIC_EVENT_TAG);
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

fn post_key(key: u32, down: bool) -> Result<()> {
    let Some(mac_key) = evdev_to_mac(key) else {
        return Ok(());
    };
    let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), mac_key, down) };
    if event.is_null() {
        bail!("cannot create macOS keyboard event");
    }
    unsafe {
        CGEventSetIntegerValueField(event, FIELD_SOURCE_USER_DATA, SYNTHETIC_EVENT_TAG);
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_keyboard_mapping_round_trips_common_keys() {
        for key in [
            1, 15, 16, 28, 30, 36, 37, 39, 40, 42, 43, 49, 50, 51, 52, 53, 56, 57, 97, 103, 105,
            106, 108, 125, 126,
        ] {
            assert_eq!(mac_to_evdev(evdev_to_mac(key).unwrap()), Some(key));
        }
    }

    #[test]
    fn mac_modifiers_have_flag_masks() {
        assert_eq!(modifier_to_evdev(0x38), Some((42, FLAG_SHIFT)));
        assert_eq!(modifier_to_evdev(0x3b), Some((29, FLAG_CONTROL)));
        assert_eq!(modifier_to_evdev(0x3d), Some((100, FLAG_OPTION)));
        assert_eq!(modifier_to_evdev(0x36), Some((126, FLAG_COMMAND)));
    }

    #[test]
    fn tiny_motion_keeps_delta_direction() {
        assert_eq!(delta_field(0.2), 1);
        assert_eq!(delta_field(-0.2), -1);
        assert_eq!(delta_field(0.0), 0);
    }

    #[test]
    fn left_edge_entry_allows_right_motion() {
        let bounds = display_bounds();
        let injector = Injector {
            point: CGPoint {
                x: bounds.origin.x + 20.0,
                y: bounds.origin.y + 20.0,
            },
            buttons: 0,
            held_keys: Vec::new(),
        };

        assert!(!injector.left_entry_edge(
            Edge::Left,
            PointerInput::Motion { dx: 5.0, dy: 0.0 },
        ));
    }
}
