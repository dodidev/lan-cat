#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod wayland;

#[cfg(target_os = "macos")]
pub use macos::{Capture, Injector};
#[cfg(target_os = "linux")]
pub use wayland::{Capture, Injector};

use super::protocol::{Edge, KeyboardInput, PointerInput};

#[derive(Clone, Copy, Debug)]
pub enum CaptureEvent {
    Begin { edge: Edge, position: f64 },
    Pointer(PointerInput),
    Keyboard(KeyboardInput),
    LocalInput,
    LocalKeyboard(KeyboardInput),
}
