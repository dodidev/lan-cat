use std::{collections::VecDeque, sync::mpsc, thread, time::Duration};

use anyhow::Result;
use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypeHTML,
    NSPasteboardTypePNG, NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSString};
use tokio::sync::mpsc as async_mpsc;

use super::{
    Change, Command,
    files::{file_uri, materialize_files, paths_from_file_uris, read_file_paths},
};
use crate::protocol::{ClipboardFile, ClipboardPayload};

pub(super) const NAME: &str = "macos-nspasteboard";

pub(super) fn spawn(
    changes: async_mpsc::UnboundedSender<Change>,
    commands: mpsc::Receiver<Command>,
) -> Result<Option<ClipboardPayload>> {
    let initial = read_payload().filter(|payload| payload.files.is_empty());
    thread::Builder::new()
        .name("lan-cat-pasteboard".into())
        .spawn(move || {
            autoreleasepool(|_| {
                let pasteboard = NSPasteboard::generalPasteboard();
                let mut count = pasteboard.changeCount();
                let mut injected_count = None;
                let mut retained = VecDeque::new();
                loop {
                    match commands.recv_timeout(Duration::from_millis(250)) {
                        Ok(Command::Set(payload)) => {
                            pasteboard.clearContents();
                            if let Err(error) = write_payload(&pasteboard, payload, &mut retained) {
                                tracing::warn!(%error, "failed to write macOS pasteboard");
                            }
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
                    match read_file_paths_from(&pasteboard) {
                        Ok(Some(paths)) => {
                            let _ = changes.send(Change::Files(paths));
                        }
                        Ok(None) => {
                            if let Some(payload) = read_payload_from(&pasteboard) {
                                let _ = changes.send(Change::Payload(payload));
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to read copied file paths");
                        }
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
    match read_files(pasteboard) {
        Ok(Some(files)) => {
            let payload = ClipboardPayload {
                files,
                ..Default::default()
            };
            payload.validate().ok()?;
            return Some(payload);
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "failed to read copied files");
            return None;
        }
    }
    // SAFETY: These AppKit pasteboard type constants exist on every supported macOS version.
    let payload = unsafe {
        ClipboardPayload {
            text: pasteboard
                .stringForType(NSPasteboardTypeString)
                .map(|value| value.to_string()),
            html: pasteboard
                .stringForType(NSPasteboardTypeHTML)
                .map(|value| value.to_string()),
            rtf: data_for_type(pasteboard, NSPasteboardTypeRTF)
                .and_then(|bytes| String::from_utf8(bytes).ok()),
            png: data_for_type(pasteboard, NSPasteboardTypePNG),
            files: Vec::new(),
        }
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

fn read_files(pasteboard: &NSPasteboard) -> Result<Option<Vec<ClipboardFile>>> {
    let Some(paths) = read_file_paths_from(pasteboard)? else {
        return Ok(None);
    };
    Ok(Some(read_file_paths(paths)?))
}

fn read_file_paths_from(pasteboard: &NSPasteboard) -> Result<Option<Vec<std::path::PathBuf>>> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Ok(None);
    };
    let mut uris = Vec::new();
    for item in items.to_vec() {
        // SAFETY: FileURL exists on every supported macOS version.
        if let Some(uri) = unsafe { item.stringForType(NSPasteboardTypeFileURL) } {
            uris.push(uri.to_string());
        }
    }
    if uris.is_empty() {
        return Ok(None);
    }
    Ok(Some(paths_from_file_uris(uris.iter().map(String::as_str))?))
}

fn write_payload(
    pasteboard: &NSPasteboard,
    payload: ClipboardPayload,
    retained: &mut VecDeque<tempfile::TempDir>,
) -> Result<()> {
    if !payload.files.is_empty() {
        let paths = materialize_files(&payload.files, retained)?;
        let mut objects = Vec::with_capacity(paths.len());
        for path in paths {
            let item = NSPasteboardItem::new();
            let uri = NSString::from_str(&file_uri(&path));
            // SAFETY: FileURL exists on every supported macOS version.
            if !unsafe { item.setString_forType(&uri, NSPasteboardTypeFileURL) } {
                anyhow::bail!("pasteboard rejected file URL data");
            }
            objects.push(ProtocolObject::<dyn NSPasteboardWriting>::from_retained(
                item,
            ));
        }
        let objects = NSArray::from_retained_slice(&objects);
        if !pasteboard.writeObjects(&objects) {
            anyhow::bail!("pasteboard rejected copied files");
        }
        return Ok(());
    }
    // SAFETY: These AppKit pasteboard type constants exist on every supported macOS version.
    unsafe {
        if let Some(text) = payload.text {
            if !pasteboard.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString) {
                anyhow::bail!("pasteboard rejected text/plain data");
            }
        }
        if let Some(html) = payload.html {
            if !pasteboard.setString_forType(&NSString::from_str(&html), NSPasteboardTypeHTML) {
                anyhow::bail!("pasteboard rejected text/html data");
            }
        }
        if let Some(rtf) = payload.rtf {
            let data = NSData::with_bytes(rtf.as_bytes());
            if !pasteboard.setData_forType(Some(&data), NSPasteboardTypeRTF) {
                anyhow::bail!("pasteboard rejected text/rtf data");
            }
        }
        if let Some(png) = payload.png {
            let data = NSData::with_bytes(&png);
            if !pasteboard.setData_forType(Some(&data), NSPasteboardTypePNG) {
                anyhow::bail!("pasteboard rejected image/png data");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_items_round_trip_through_macos_pasteboard() {
        autoreleasepool(|_| {
            let pasteboard = NSPasteboard::pasteboardWithUniqueName();
            let payload = ClipboardPayload {
                files: vec![ClipboardFile {
                    name: "report.txt".into(),
                    data: b"contents".to_vec(),
                }],
                ..Default::default()
            };
            let mut retained = VecDeque::new();

            pasteboard.clearContents();
            write_payload(&pasteboard, payload.clone(), &mut retained).unwrap();

            assert_eq!(read_files(&pasteboard).unwrap(), Some(payload.files));
        });
    }
}
