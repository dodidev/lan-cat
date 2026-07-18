use std::{
    ffi::{c_double, c_int, c_long, c_void},
    ptr,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use super::CaptureEvent;
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
const SCROLL_WHEEL: u32 = 22;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const TAP_DISABLED_TIMEOUT: u32 = 0xffff_fffe;
const TAP_DISABLED_USER: u32 = 0xffff_ffff;
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;
const FIELD_SCROLL_Y: u32 = 11;
const FIELD_SCROLL_X: u32 = 12;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

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
        let point = edge_point(state.edge, state.position, 3.0);
        unsafe {
            CGDisplayShowCursor(CGMainDisplayID());
            CGWarpMouseCursorPosition(point);
        }
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
    if !state.active && is_motion(event_type) {
        let point = unsafe { CGEventGetLocation(event) };
        let dx = unsafe { CGEventGetIntegerValueField(event, FIELD_DELTA_X) } as f64;
        let dy = unsafe { CGEventGetIntegerValueField(event, FIELD_DELTA_Y) } as f64;
        if let Some((edge, position)) = crossed_edge(point, dx, dy) {
            state.active = true;
            state.edge = edge;
            state.position = position;
            unsafe {
                CGDisplayHideCursor(CGMainDisplayID());
            }
            let _ = state.events.send(CaptureEvent::Begin { edge, position });
            return ptr::null_mut();
        }
    }
    if !state.active {
        return event;
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
}

impl Injector {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            point: edge_point(Edge::Left, 0.5, 3.0),
            buttons: 0,
        })
    }

    pub fn begin(&mut self, edge: Edge, position: f64) -> Result<()> {
        self.point = edge_point(edge, position, 3.0);
        unsafe {
            CGWarpMouseCursorPosition(self.point);
        }
        post_mouse(MOUSE_MOVED, self.point, 0)
    }

    pub fn apply(&mut self, input: PointerInput) -> Result<()> {
        match input {
            PointerInput::Motion { dx, dy } => {
                let bounds = display_bounds();
                self.point.x = (self.point.x + dx)
                    .clamp(bounds.origin.x, bounds.origin.x + bounds.size.width - 1.0);
                self.point.y = (self.point.y + dy)
                    .clamp(bounds.origin.y, bounds.origin.y + bounds.size.height - 1.0);
                let event_type = if self.buttons & 1 != 0 {
                    LEFT_DRAGGED
                } else if self.buttons & 2 != 0 {
                    RIGHT_DRAGGED
                } else if self.buttons & 4 != 0 {
                    OTHER_DRAGGED
                } else {
                    MOUSE_MOVED
                };
                post_mouse(event_type, self.point, 0)?;
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
        Ok(())
    }

    pub fn apply_keyboard(&mut self, _input: KeyboardInput) -> Result<()> {
        Ok(())
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
fn is_motion(event: u32) -> bool {
    matches!(
        event,
        MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED
    )
}

fn post_mouse(event_type: u32, point: CGPoint, button: u32) -> Result<()> {
    let event = unsafe { CGEventCreateMouseEvent(ptr::null(), event_type, point, button) };
    if event.is_null() {
        bail!("cannot create macOS mouse event");
    }
    unsafe {
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
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
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}
