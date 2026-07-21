use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs,
    io::Read,
    path::PathBuf,
};

#[cfg(any(target_os = "linux", test))]
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
};

use anyhow::{Context, Result};

use crate::protocol::{ClipboardFile, MAX_FILES, MAX_PAYLOAD_BYTES};

const RETAINED_FILE_CLIPBOARDS: usize = 4;

#[cfg(test)]
pub(super) fn read_file_uris<'a>(
    uris: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<ClipboardFile>> {
    read_file_paths(paths_from_file_uris(uris)?)
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn paths_from_file_uris<'a>(
    uris: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<PathBuf>> {
    uris.into_iter().map(path_from_file_uri).collect()
}

pub(super) fn read_file_paths(paths: Vec<PathBuf>) -> Result<Vec<ClipboardFile>> {
    let mut files = Vec::new();
    let mut names = HashSet::new();
    let mut remaining = MAX_PAYLOAD_BYTES;
    for path in paths {
        if files.len() == MAX_FILES {
            anyhow::bail!("clipboard contains more than {MAX_FILES} files");
        }
        let original_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .context("clipboard filename is not valid UTF-8")?
            .to_owned();
        let name = unique_file_name(&original_name, &mut names);
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

fn unique_file_name(name: &str, used: &mut HashSet<String>) -> String {
    if used.insert(name.to_owned()) {
        return name.to_owned();
    }

    let (stem, extension) = name
        .rsplit_once('.')
        .map_or((name, ""), |(stem, extension)| {
            if stem.is_empty() {
                (name, "")
            } else {
                (stem, extension)
            }
        });
    for index in 2.. {
        let candidate = if extension.is_empty() {
            format!("{stem} ({index})")
        } else {
            format!("{stem} ({index}).{extension}")
        };
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded numeric suffix must eventually produce a unique filename")
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
pub(super) fn file_uri(path: &Path) -> String {
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

pub(super) fn materialize_files(
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

    #[test]
    fn duplicate_clipboard_filenames_are_renamed() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = first_dir.path().join("report.txt");
        let second = second_dir.path().join("report.txt");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();

        let files = read_file_paths(vec![first, second]).unwrap();

        assert_eq!(files[0].name, "report.txt");
        assert_eq!(files[0].data, b"one");
        assert_eq!(files[1].name, "report (2).txt");
        assert_eq!(files[1].data, b"two");
    }
}
