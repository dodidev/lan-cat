use std::{
    collections::VecDeque,
    ffi::{CStr, OsString},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeHTML, NSPasteboardTypePNG,
    NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};
use tokio::sync::mpsc as async_mpsc;

use super::{
    Change, Command,
    files::{materialize_files, read_file_paths},
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
                        Ok(Command::MarkFilesHandled(_)) => {}
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

fn read_file_paths_from(pasteboard: &NSPasteboard) -> Result<Option<Vec<PathBuf>>> {
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
    Ok(Some(
        uris.iter()
            .map(|uri| macos_path_from_file_uri(uri))
            .collect::<Result<Vec<_>>>()?,
    ))
}

fn macos_path_from_file_uri(uri: &str) -> Result<PathBuf> {
    let url = NSURL::URLWithString(&NSString::from_str(uri)).context("invalid file URL")?;
    if !url.isFileURL() {
        anyhow::bail!("pasteboard URL is not a local file URL");
    }
    let url = url
        .filePathURL()
        .context("cannot resolve macOS file-reference URL")?;
    // SAFETY: NSURL guarantees a NUL-terminated filesystem representation for a file URL.
    let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
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
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            objects.push(ProtocolObject::<dyn NSPasteboardWriting>::from_retained(
                url,
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

    #[test]
    fn nsurl_resolves_file_paths_and_file_references() {
        autoreleasepool(|_| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("Finder file #1.txt");
            std::fs::write(&path, b"contents").unwrap();
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let absolute = url.absoluteString().unwrap().to_string();
            assert_eq!(
                macos_path_from_file_uri(&absolute)
                    .unwrap()
                    .canonicalize()
                    .unwrap(),
                path.canonicalize().unwrap()
            );

            if let Some(reference) = url.fileReferenceURL() {
                let reference = reference.absoluteString().unwrap().to_string();
                assert_eq!(
                    macos_path_from_file_uri(&reference)
                        .unwrap()
                        .canonicalize()
                        .unwrap(),
                    path.canonicalize().unwrap()
                );
            }
        });
    }
}
