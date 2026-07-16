use std::{sync::mpsc, thread, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::mpsc as async_mpsc;

use crate::protocol::ClipboardPayload;

enum Command {
    Set(ClipboardPayload),
    Rebaseline,
}

pub struct Clipboard {
    pub changes: async_mpsc::UnboundedReceiver<ClipboardPayload>,
    pub initial_payload: Option<ClipboardPayload>,
    commands: mpsc::Sender<Command>,
    pub backend: &'static str,
}

impl Clipboard {
    pub fn start() -> Result<Self> {
        let (change_tx, change_rx) = async_mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::channel();
        let initial_payload = platform::spawn(change_tx, command_rx)?;
        Ok(Self {
            changes: change_rx,
            initial_payload,
            commands: command_tx,
            backend: platform::NAME,
        })
    }

    pub fn set_payload(&self, payload: ClipboardPayload) -> Result<()> {
        payload.validate()?;
        self.commands
            .send(Command::Set(payload))
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
    use crate::protocol::MAX_PAYLOAD_BYTES;
    use wl_clipboard_rs::{
        copy::{MimeSource, MimeType as CopyMime, Options, Source},
        paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types},
    };

    pub const NAME: &str = "wayland-data-control";

    pub fn spawn(
        changes: async_mpsc::UnboundedSender<ClipboardPayload>,
        commands: mpsc::Receiver<Command>,
    ) -> Result<Option<ClipboardPayload>> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            anyhow::bail!("WAYLAND_DISPLAY is not set; lan-cat requires a Wayland session");
        }
        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(_) | Err(wl_clipboard_rs::paste::Error::ClipboardEmpty) => {}
            Err(error) => anyhow::bail!(
                "Wayland clipboard unavailable: {error}. Compositor must support ext-data-control-v1 or wlr-data-control-v1"
            ),
        }
        let initial = read_payload();
        thread::Builder::new()
            .name("lan-cat-wayland".into())
            .spawn(move || {
                let mut baseline = read_payload();
                let mut injected: Option<[u8; 32]> = None;
                loop {
                    match commands.recv_timeout(Duration::from_millis(250)) {
                        Ok(Command::Set(payload)) => {
                            injected = Some(payload.digest());
                            if let Err(error) = write_payload(payload) {
                                tracing::warn!(%error, "failed to write Wayland clipboard");
                            }
                        }
                        Ok(Command::Rebaseline) => {
                            baseline = read_payload();
                            injected = None;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let Some(payload) = read_payload() else {
                        continue;
                    };
                    if baseline.as_ref() == Some(&payload) {
                        continue;
                    }
                    baseline = Some(payload.clone());
                    let digest = payload.digest();
                    if injected.take() == Some(digest) {
                        continue;
                    }
                    let _ = changes.send(payload);
                }
            })?;
        Ok(initial)
    }

    fn read_payload() -> Option<ClipboardPayload> {
        let mimes = get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?;
        let payload = ClipboardPayload {
            text: read_text().ok(),
            html: read_string_mime(&mimes, "text/html"),
            rtf: read_string_mime(&mimes, "text/rtf"),
            png: read_bytes_mime(&mimes, "image/png"),
        };
        payload.validate().ok()?;
        Some(payload)
    }

    fn write_payload(payload: ClipboardPayload) -> Result<()> {
        let mut sources = Vec::new();
        if let Some(text) = payload.text {
            sources.push(MimeSource {
                source: Source::Bytes(text.into_bytes().into()),
                mime_type: CopyMime::Text,
            });
        }
        if let Some(html) = payload.html {
            sources.push(MimeSource {
                source: Source::Bytes(html.into_bytes().into()),
                mime_type: CopyMime::Specific("text/html".into()),
            });
        }
        if let Some(rtf) = payload.rtf {
            sources.push(MimeSource {
                source: Source::Bytes(rtf.into_bytes().into()),
                mime_type: CopyMime::Specific("text/rtf".into()),
            });
        }
        if let Some(png) = payload.png {
            sources.push(MimeSource {
                source: Source::Bytes(png.into_boxed_slice()),
                mime_type: CopyMime::Specific("image/png".into()),
            });
        }
        Options::new().copy_multi(sources)?;
        Ok(())
    }

    fn read_text() -> Result<String> {
        let bytes = read_mime(MimeType::Text)?;
        Ok(String::from_utf8(bytes)?)
    }

    fn read_string_mime(mimes: &std::collections::HashSet<String>, mime: &str) -> Option<String> {
        if !mimes.contains(mime) {
            return None;
        }
        match read_mime(MimeType::Specific(mime)) {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            Err(error) => {
                tracing::warn!(%mime, %error, "failed to read clipboard MIME data");
                None
            }
        }
    }

    fn read_bytes_mime(mimes: &std::collections::HashSet<String>, mime: &str) -> Option<Vec<u8>> {
        if !mimes.contains(mime) {
            return None;
        }
        match read_mime(MimeType::Specific(mime)) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                tracing::warn!(%mime, %error, "failed to read clipboard MIME data");
                None
            }
        }
    }

    fn read_mime(mime: MimeType<'_>) -> Result<Vec<u8>> {
        let (reader, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, mime)?;
        let mut bytes = Vec::new();
        reader
            .take((MAX_PAYLOAD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_PAYLOAD_BYTES {
            anyhow::bail!("clipboard data exceeds size limit");
        }
        Ok(bytes)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardTypeHTML, NSPasteboardTypePNG, NSPasteboardTypeRTF,
        NSPasteboardTypeString,
    };
    use objc2_foundation::{NSData, NSString};

    pub const NAME: &str = "macos-nspasteboard";

    pub fn spawn(
        changes: async_mpsc::UnboundedSender<ClipboardPayload>,
        commands: mpsc::Receiver<Command>,
    ) -> Result<Option<ClipboardPayload>> {
        let initial = read_payload();
        thread::Builder::new()
            .name("lan-cat-pasteboard".into())
            .spawn(move || {
                autoreleasepool(|_| {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    let mut count = pasteboard.changeCount();
                    let mut injected_count = None;
                    loop {
                        match commands.recv_timeout(Duration::from_millis(250)) {
                            Ok(Command::Set(payload)) => {
                                pasteboard.clearContents();
                                write_payload(&pasteboard, payload);
                                count = pasteboard.changeCount();
                                injected_count = Some(count);
                            }
                            Ok(Command::Rebaseline) => {
                                count = pasteboard.changeCount();
                                injected_count = None;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        let current = pasteboard.changeCount();
                        if current == count {
                            continue;
                        }
                        count = current;
                        if injected_count.take() == Some(current) {
                            continue;
                        }
                        if let Some(payload) = read_payload_from(&pasteboard) {
                            let _ = changes.send(payload);
                        }
                    }
                });
            })?;
        Ok(initial)
    }

    fn read_payload() -> Option<ClipboardPayload> {
        autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            read_payload_from(&pasteboard)
        })
    }

    fn read_payload_from(pasteboard: &NSPasteboard) -> Option<ClipboardPayload> {
        let payload = ClipboardPayload {
            text: pasteboard
                .stringForType(NSPasteboardTypeString)
                .map(|value| value.to_string()),
            html: pasteboard
                .stringForType(NSPasteboardTypeHTML)
                .map(|value| value.to_string()),
            rtf: data_for_type(pasteboard, NSPasteboardTypeRTF)
                .and_then(|bytes| String::from_utf8(bytes).ok()),
            png: data_for_type(pasteboard, NSPasteboardTypePNG),
        };
        payload.validate().ok()?;
        Some(payload)
    }

    fn data_for_type(
        pasteboard: &NSPasteboard,
        ty: &'static objc2_app_kit::NSPasteboardType,
    ) -> Option<Vec<u8>> {
        pasteboard.dataForType(ty).map(|data| data.to_vec())
    }

    fn write_payload(pasteboard: &NSPasteboard, payload: ClipboardPayload) {
        if let Some(text) = payload.text {
            pasteboard.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
        }
        if let Some(html) = payload.html {
            pasteboard.setString_forType(&NSString::from_str(&html), NSPasteboardTypeHTML);
        }
        if let Some(rtf) = payload.rtf {
            let data = NSData::with_bytes(rtf.as_bytes());
            pasteboard.setData_forType(Some(&data), NSPasteboardTypeRTF);
        }
        if let Some(png) = payload.png {
            let data = NSData::with_bytes(&png);
            pasteboard.setData_forType(Some(&data), NSPasteboardTypePNG);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("lan-cat supports only Linux and macOS");
