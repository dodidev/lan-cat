use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::sync::mpsc as async_mpsc;

use crate::protocol::{ClipboardFile, ClipboardPayload, MAX_FILES, MAX_PAYLOAD_BYTES};

const RETAINED_FILE_CLIPBOARDS: usize = 4;

fn read_file_uris<'a>(uris: impl IntoIterator<Item = &'a str>) -> Result<Vec<ClipboardFile>> {
    let mut files = Vec::new();
    let mut remaining = MAX_PAYLOAD_BYTES;
    for uri in uris {
        if files.len() == MAX_FILES {
            anyhow::bail!("clipboard contains more than {MAX_FILES} files");
        }
        let path = path_from_file_uri(uri)?;
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .context("clipboard filename is not valid UTF-8")?
            .to_owned();
        remaining = remaining
            .checked_sub(name.len())
            .context("clipboard files exceed size limit")?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            anyhow::bail!("clipboard paths must be regular files");
        }
        if metadata.len() > remaining as u64 {
            anyhow::bail!("clipboard files exceed {MAX_PAYLOAD_BYTES} bytes");
        }
        let mut data = Vec::new();
        fs::File::open(&path)?
            .take((remaining + 1) as u64)
            .read_to_end(&mut data)?;
        if data.len() > remaining {
            anyhow::bail!("clipboard files exceed {MAX_PAYLOAD_BYTES} bytes");
        }
        remaining -= data.len();
        files.push(ClipboardFile { name, data });
    }
    Ok(files)
}

fn path_from_file_uri(uri: &str) -> Result<PathBuf> {
    let value = uri
        .trim()
        .strip_prefix("file://")
        .context("not a file URI")?;
    let path = value.strip_prefix("localhost").unwrap_or(value);
    if !path.starts_with('/') {
        anyhow::bail!("remote file URI is unsupported");
    }
    let bytes = percent_decode(path.as_bytes())?;
    if bytes.contains(&0) {
        anyhow::bail!("file URI contains a NUL byte");
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn percent_decode(value: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            let encoded = value
                .get(index + 1..index + 3)
                .context("invalid percent escape in file URI")?;
            let text = std::str::from_utf8(encoded)?;
            decoded
                .push(u8::from_str_radix(text, 16).context("invalid percent escape in file URI")?);
            index += 3;
        } else {
            decoded.push(value[index]);
            index += 1;
        }
    }
    Ok(decoded)
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            uri.push(*byte as char);
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    uri
}

fn materialize_files(
    files: &[ClipboardFile],
    retained: &mut VecDeque<tempfile::TempDir>,
) -> Result<Vec<PathBuf>> {
    let dir = tempfile::Builder::new()
        .prefix("lan-cat-files-")
        .tempdir()?;
    let mut paths = Vec::with_capacity(files.len());
    for file in files {
        let path = dir.path().join(&file.name);
        fs::write(&path, &file.data)?;
        paths.push(path);
    }
    retained.push_back(dir);
    while retained.len() > RETAINED_FILE_CLIPBOARDS {
        retained.pop_front();
    }
    Ok(paths)
}

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
    use super::*;
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
                let mut retained = VecDeque::new();
                loop {
                    match commands.recv_timeout(Duration::from_millis(250)) {
                        Ok(Command::Set(payload)) => {
                            injected = Some(payload.digest());
                            if let Err(error) = write_payload(payload, &mut retained) {
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
        match read_files(&mimes) {
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
        let payload = ClipboardPayload {
            text: read_text().ok(),
            html: read_string_mime(&mimes, "text/html"),
            rtf: read_string_mime(&mimes, "text/rtf"),
            png: read_bytes_mime(&mimes, "image/png"),
            files: Vec::new(),
        };
        payload.validate().ok()?;
        Some(payload)
    }

    fn read_files(mimes: &std::collections::HashSet<String>) -> Result<Option<Vec<ClipboardFile>>> {
        if !mimes.contains("text/uri-list") {
            return Ok(None);
        }
        let bytes = read_mime(MimeType::Specific("text/uri-list"))?;
        let list = String::from_utf8(bytes)?;
        let uris: Vec<_> = list
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter(|line| line.starts_with("file://"))
            .collect();
        if uris.is_empty() {
            return Ok(None);
        }
        Ok(Some(read_file_uris(uris)?))
    }

    fn write_payload(
        payload: ClipboardPayload,
        retained: &mut VecDeque<tempfile::TempDir>,
    ) -> Result<()> {
        if !payload.files.is_empty() {
            let paths = materialize_files(&payload.files, retained)?;
            let mut list = paths
                .iter()
                .map(|path| file_uri(path))
                .collect::<Vec<_>>()
                .join("\r\n");
            list.push_str("\r\n");
            Options::new().copy(
                Source::Bytes(list.into_bytes().into()),
                CopyMime::Specific("text/uri-list".into()),
            )?;
            return Ok(());
        }
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
    use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypeHTML,
        NSPasteboardTypePNG, NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardWriting,
    };
    use objc2_foundation::{NSArray, NSData, NSString};

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
                    let mut retained = VecDeque::new();
                    loop {
                        match commands.recv_timeout(Duration::from_millis(250)) {
                            Ok(Command::Set(payload)) => {
                                pasteboard.clearContents();
                                if let Err(error) =
                                    write_payload(&pasteboard, payload, &mut retained)
                                {
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
        Ok(Some(read_file_uris(uris.iter().map(String::as_str))?))
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
                if !pasteboard.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString)
                {
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
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("lan-cat supports only Linux and macOS");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_round_trip_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report #1.txt");
        fs::write(&path, b"contents").unwrap();

        let uri = file_uri(&path);
        assert!(uri.contains("report%20%231.txt"));
        assert_eq!(path_from_file_uri(&uri).unwrap(), path);

        let files = read_file_uris([uri.as_str()]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "report #1.txt");
        assert_eq!(files[0].data, b"contents");
    }

    #[test]
    fn file_uri_rejects_remote_hosts_and_bad_escapes() {
        assert!(path_from_file_uri("file://server/share/file.txt").is_err());
        assert!(path_from_file_uri("file:///tmp/bad%XXname").is_err());
    }
}
