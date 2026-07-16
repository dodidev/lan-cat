use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::config;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pause,
    Resume,
    Unpair { peer: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
}

pub async fn request(request: Request) -> Result<()> {
    let path = config::runtime_socket()?;
    let mut stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connect daemon at {}; run `lan-cat daemon` or `lan-cat service start`",
            path.display()
        )
    })?;
    stream.write_all(&serde_json::to_vec(&request)?).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line)?;
    println!("{}", response.message);
    if response.ok {
        Ok(())
    } else {
        bail!(response.message)
    }
}

pub async fn daemon_available() -> bool {
    config::runtime_socket()
        .ok()
        .is_some_and(|path| std::os::unix::net::UnixStream::connect(path).is_ok())
}

pub fn bind() -> Result<UnixListener> {
    let path = config::runtime_socket()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            bail!("another daemon is already running");
        }
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn read(stream: &mut UnixStream) -> Result<Request> {
    verify_same_user(stream)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.len() > 4096 {
        bail!("IPC request too large");
    }
    Ok(serde_json::from_str(&line)?)
}

pub async fn write(stream: &mut UnixStream, response: Response) -> Result<()> {
    stream.write_all(&serde_json::to_vec(&response)?).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_same_user(stream: &UnixStream) -> Result<()> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let credentials = getsockopt(stream, PeerCredentials)?;
    if credentials.uid() != nix::unistd::Uid::current().as_raw() {
        bail!("IPC peer user mismatch");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_same_user(stream: &UnixStream) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if uid != unsafe { libc::geteuid() } {
        bail!("IPC peer user mismatch");
    }
    Ok(())
}

pub struct SocketGuard;

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(path) = config::runtime_socket() {
            let _ = remove_socket(&path);
        }
    }
}

fn remove_socket(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}
