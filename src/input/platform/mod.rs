#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod wayland;

#[cfg(target_os = "macos")]
pub use macos::{Capture, Injector};
#[cfg(target_os = "linux")]
pub use wayland::{Capture, Injector};

use super::protocol::{Edge, KeyboardInput, PointerInput};

pub(super) const ALL_EDGE_MASK: u8 = 0x0f;

pub(super) fn edge_mask(edge: Edge) -> u8 {
    1 << edge as u8
}

#[derive(Clone, Copy, Debug)]
pub enum CaptureEvent {
    Begin {
        edge: Edge,
        position: f64,
        screen_width: f64,
        screen_height: f64,
    },
    Pointer(PointerInput),
    Keyboard(KeyboardInput),
    LocalInput,
    LocalKeyboard(KeyboardInput),
    #[cfg(target_os = "linux")]
    CaptureLost,
}
