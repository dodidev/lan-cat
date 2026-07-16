use std::{sync::mpsc, thread, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::mpsc as async_mpsc;

use crate::protocol::{MAX_TEXT_BYTES, validate_text};

enum Command {
    Set(String),
    Rebaseline,
}

pub struct Clipboard {
    pub changes: async_mpsc::UnboundedReceiver<String>,
    pub initial_text: Option<String>,
    commands: mpsc::Sender<Command>,
    pub backend: &'static str,
}

impl Clipboard {
    pub fn start() -> Result<Self> {
        let (change_tx, change_rx) = async_mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::channel();
        let initial_text = platform::spawn(change_tx, command_rx)?;
        Ok(Self {
            changes: change_rx,
            initial_text,
            commands: command_tx,
            backend: platform::NAME,
        })
    }

    pub fn set_text(&self, text: String) -> Result<()> {
        validate_text(&text)?;
        self.commands
            .send(Command::Set(text))
            .context("clipboard backend stopped")
    }

    pub fn rebaseline(&self) -> Result<()> {
        self.commands
            .send(Command::Rebaseline)
            .context("clipboard backend stopped")
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io::Read;

    use super::*;
    use wl_clipboard_rs::{
        copy::{MimeType as CopyMime, Options, Source},
        paste::{ClipboardType, MimeType, Seat, get_contents},
    };

    pub const NAME: &str = "wayland-data-control";

    pub fn spawn(
        changes: async_mpsc::UnboundedSender<String>,
        commands: mpsc::Receiver<Command>,
    ) -> Result<Option<String>> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            anyhow::bail!("WAYLAND_DISPLAY is not set; lan-cat requires a Wayland session");
        }
        match wl_clipboard_rs::paste::get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(_) | Err(wl_clipboard_rs::paste::Error::ClipboardEmpty) => {}
            Err(error) => anyhow::bail!(
                "Wayland clipboard unavailable: {error}. Compositor must support ext-data-control-v1 or wlr-data-control-v1"
            ),
        }
        let initial = read_text().ok().filter(|text| validate_text(text).is_ok());
        thread::Builder::new()
            .name("lan-cat-wayland".into())
            .spawn(move || {
                let mut baseline = read_text().ok();
                let mut injected: Option<[u8; 32]> = None;
                loop {
                    match commands.recv_timeout(Duration::from_millis(250)) {
                        Ok(Command::Set(text)) => {
                            injected = Some(*blake3::hash(text.as_bytes()).as_bytes());
                            if let Err(error) = Options::new()
                                .copy(Source::Bytes(text.into_bytes().into()), CopyMime::Text)
                            {
                                tracing::warn!(%error, "failed to write Wayland clipboard");
                            }
                        }
                        Ok(Command::Rebaseline) => {
                            baseline = read_text().ok();
                            injected = None;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let Ok(text) = read_text() else { continue };
                    if baseline.as_ref() == Some(&text) {
                        continue;
                    }
                    baseline = Some(text.clone());
                    let hash = *blake3::hash(text.as_bytes()).as_bytes();
                    if injected.take() == Some(hash) {
                        continue;
                    }
                    if validate_text(&text).is_ok() {
                        let _ = changes.send(text);
                    }
                }
            })?;
        Ok(initial)
    }

    fn read_text() -> Result<String> {
        let (reader, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text)?;
        let mut bytes = Vec::new();
        reader
            .take((MAX_TEXT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_TEXT_BYTES {
            anyhow::bail!("clipboard text exceeds size limit");
        }
        Ok(String::from_utf8(bytes)?)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    pub const NAME: &str = "macos-nspasteboard";

    pub fn spawn(
        changes: async_mpsc::UnboundedSender<String>,
        commands: mpsc::Receiver<Command>,
    ) -> Result<Option<String>> {
        let initial = read_text().filter(|text| validate_text(text).is_ok());
        thread::Builder::new()
            .name("lan-cat-pasteboard".into())
            .spawn(move || {
                autoreleasepool(|_| {
                    let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
                    let mut count = unsafe { pasteboard.changeCount() };
                    let mut injected_count = None;
                    loop {
                        match commands.recv_timeout(Duration::from_millis(250)) {
                            Ok(Command::Set(text)) => unsafe {
                                pasteboard.clearContents();
                                pasteboard.setString_forType(
                                    &NSString::from_str(&text),
                                    NSPasteboardTypeString,
                                );
                                count = pasteboard.changeCount();
                                injected_count = Some(count);
                            },
                            Ok(Command::Rebaseline) => unsafe {
                                count = pasteboard.changeCount();
                                injected_count = None;
                            },
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        let current = unsafe { pasteboard.changeCount() };
                        if current == count {
                            continue;
                        }
                        count = current;
                        if injected_count.take() == Some(current) {
                            continue;
                        }
                        let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) }
                            .map(|value| value.to_string());
                        if let Some(text) = text.filter(|value| validate_text(value).is_ok()) {
                            let _ = changes.send(text);
                        }
                    }
                });
            })?;
        Ok(initial)
    }

    fn read_text() -> Option<String> {
        autoreleasepool(|_| {
            let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
            unsafe { pasteboard.stringForType(NSPasteboardTypeString) }
                .map(|value| value.to_string())
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("lan-cat supports only Linux and macOS");
