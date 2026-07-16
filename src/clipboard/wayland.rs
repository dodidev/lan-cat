use std::{
    collections::VecDeque,
    io::Read,
    process::{Command as ProcessCommand, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::Result;
use tokio::sync::mpsc as async_mpsc;
use wl_clipboard_rs::{
    copy::{MimeSource, MimeType as CopyMime, Options, Source},
    paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types},
};

use super::{
    Change, Command,
    files::{file_uri, materialize_files, paths_from_file_uris, read_file_paths},
};
use crate::protocol::{ClipboardFile, ClipboardPayload, MAX_PAYLOAD_BYTES};

pub(super) const NAME: &str = "wayland-data-control";

pub(super) fn spawn(
    changes: async_mpsc::UnboundedSender<Change>,
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
    let initial_file_paths = current_file_paths();
    let initial = initial_file_paths.is_none().then(read_payload).flatten();
    let initial_payload = initial.clone();
    let (watch_tx, watch_rx) = mpsc::channel();
    spawn_selection_watcher(watch_tx);
    thread::Builder::new()
        .name("lan-cat-wayland".into())
        .spawn(move || {
            let mut baseline = initial_payload;
            let mut baseline_files = initial_file_paths;
            let mut injected: Option<[u8; 32]> = None;
            let mut injected_files: Option<Vec<std::path::PathBuf>> = None;
            let mut handled_files: Option<Vec<std::path::PathBuf>> = None;
            let mut retained = VecDeque::new();
            loop {
                match commands.recv_timeout(Duration::from_millis(250)) {
                    Ok(Command::Set(payload)) => {
                        let digest = payload.digest();
                        match write_payload(payload, &mut retained) {
                            Ok(paths) => {
                                injected = Some(digest);
                                injected_files = paths;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "failed to write Wayland clipboard");
                            }
                        }
                    }
                    Ok(Command::MarkFilesHandled(paths)) => {
                        baseline_files = Some(paths.clone());
                        injected_files = Some(paths.clone());
                        handled_files = Some(paths);
                        injected = None;
                    }
                    Ok(Command::Rebaseline) => {
                        baseline_files = current_file_paths();
                        baseline = baseline_files.is_none().then(read_payload).flatten();
                        injected = None;
                        injected_files = None;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let mut selection_changed = false;
                while watch_rx.try_recv().is_ok() {
                    selection_changed = true;
                }
                let Some(change) = read_change() else {
                    continue;
                };
                let payload = match &change {
                    Change::Payload(payload) => {
                        baseline_files = None;
                        handled_files = None;
                        payload
                    }
                    Change::Files(paths) => {
                        if !selection_changed && handled_files.as_ref() == Some(paths) {
                            continue;
                        }
                        if !selection_changed && baseline_files.as_ref() == Some(paths) {
                            continue;
                        }
                        baseline_files = Some(paths.clone());
                        baseline = None;
                        handled_files = None;
                        if injected_files.as_ref() == Some(paths) {
                            continue;
                        }
                        if injected.take().is_some() {
                            continue;
                        }
                        let _ = changes.send(change);
                        continue;
                    }
                };
                if baseline.as_ref() == Some(payload) {
                    continue;
                }
                baseline = Some(payload.clone());
                injected_files = None;
                let digest = payload.digest();
                if injected.take() == Some(digest) {
                    continue;
                }
                let _ = changes.send(change);
            }
        })?;
    Ok(initial)
}

fn spawn_selection_watcher(changes: mpsc::Sender<()>) {
    thread::Builder::new()
        .name("lan-cat-wayland-watch".into())
        .spawn(move || {
            let mut child = match ProcessCommand::new("wl-paste")
                .args(["--watch", "printf", "."])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    tracing::debug!(%error, "wl-paste selection watcher unavailable");
                    return;
                }
            };
            let Some(mut stdout) = child.stdout.take() else {
                return;
            };
            let mut byte = [0_u8; 1];
            while stdout.read(&mut byte).is_ok_and(|read| read > 0) {
                if changes.send(()).is_err() {
                    break;
                }
            }
            let _ = child.kill();
        })
        .ok();
}

fn current_file_paths() -> Option<Vec<std::path::PathBuf>> {
    let mimes = get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?;
    read_file_path_list(&mimes).ok().flatten()
}

fn read_change() -> Option<Change> {
    let mimes = get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?;
    match read_file_path_list(&mimes) {
        Ok(Some(paths)) => return Some(Change::Files(paths)),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "failed to read copied file paths");
            return None;
        }
    }
    read_payload().map(Change::Payload)
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
    let Some(paths) = read_file_path_list(mimes)? else {
        return Ok(None);
    };
    Ok(Some(read_file_paths(paths)?))
}

fn read_file_path_list(
    mimes: &std::collections::HashSet<String>,
) -> Result<Option<Vec<std::path::PathBuf>>> {
    let candidates = file_mime_candidates(mimes);
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut last_error = None;
    for mime in candidates {
        match read_mime(MimeType::Specific(mime)) {
            Ok(bytes) => {
                let list = String::from_utf8(bytes)?;
                if let Some(paths) = parse_file_uri_list(&list)? {
                    tracing::debug!(%mime, files = paths.len(), "detected copied file list");
                    return Ok(Some(paths));
                }
            }
            Err(error) => {
                tracing::debug!(%mime, %error, "failed to read file clipboard MIME data");
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(None)
}

fn file_mime_candidates(mimes: &std::collections::HashSet<String>) -> Vec<&str> {
    let mut values = Vec::new();
    for wanted in ["x-special/gnome-copied-files", "text/uri-list"] {
        for mime in mimes {
            if mime
                .split(';')
                .next()
                .is_some_and(|base| base.eq_ignore_ascii_case(wanted))
            {
                values.push(mime.as_str());
            }
        }
    }
    values
}

fn parse_file_uri_list(list: &str) -> Result<Option<Vec<std::path::PathBuf>>> {
    let uris: Vec<_> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| *line != "copy" && *line != "cut")
        .filter(|line| line.starts_with("file://"))
        .collect();
    if uris.is_empty() {
        return Ok(None);
    }
    Ok(Some(paths_from_file_uris(uris)?))
}

fn write_payload(
    payload: ClipboardPayload,
    retained: &mut VecDeque<tempfile::TempDir>,
) -> Result<Option<Vec<std::path::PathBuf>>> {
    if !payload.files.is_empty() {
        let paths = materialize_files(&payload.files, retained)?;
        let uris = paths.iter().map(|path| file_uri(path)).collect::<Vec<_>>();
        let uri_list = format!("{}\r\n", uris.join("\r\n"));
        let gnome_list = format!("copy\n{}\n", uris.join("\n"));
        Options::new().copy_multi(vec![
            MimeSource {
                source: Source::Bytes(uri_list.into_bytes().into()),
                mime_type: CopyMime::Specific("text/uri-list".into()),
            },
            MimeSource {
                source: Source::Bytes(gnome_list.into_bytes().into()),
                mime_type: CopyMime::Specific("x-special/gnome-copied-files".into()),
            },
            MimeSource {
                source: Source::Bytes(b"0".to_vec().into()),
                mime_type: CopyMime::Specific("application/x-kde-cutselection".into()),
            },
        ])?;
        return Ok(Some(paths));
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
    Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_mime_candidates_prefer_gnome_then_uri_list_case_insensitive() {
        let mimes = [
            "TEXT/URI-LIST;charset=utf-8".to_owned(),
            "x-special/gnome-copied-files".to_owned(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            file_mime_candidates(&mimes),
            vec![
                "x-special/gnome-copied-files",
                "TEXT/URI-LIST;charset=utf-8"
            ]
        );
    }

    #[test]
    fn parse_file_uri_list_ignores_gnome_operation_line() {
        let parsed = parse_file_uri_list("copy\nfile:///tmp/report%201.txt\n# comment\n").unwrap();
        assert_eq!(
            parsed.unwrap(),
            vec![std::path::PathBuf::from("/tmp/report 1.txt")]
        );
    }
}
