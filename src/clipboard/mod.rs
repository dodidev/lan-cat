use std::{path::PathBuf, sync::mpsc};

use anyhow::{Context, Result};
use tokio::sync::mpsc as async_mpsc;

use crate::protocol::ClipboardPayload;

mod files;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
use wayland as platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("lan-cat supports only Linux and macOS");

pub(super) enum Command {
    Set(ClipboardPayload),
    MarkFilesHandled(Vec<PathBuf>),
    Rebaseline,
}

pub enum Change {
    Payload(ClipboardPayload),
    Files(Vec<PathBuf>),
}

pub struct Clipboard {
    pub changes: async_mpsc::UnboundedReceiver<Change>,
    pub initial_payload: Option<ClipboardPayload>,
    commands: mpsc::Sender<Command>,
    pub backend: &'static str,
}

pub fn payload_from_paths(paths: Vec<PathBuf>) -> Result<ClipboardPayload> {
    let payload = ClipboardPayload {
        files: files::read_file_paths(paths)?,
        ..Default::default()
    };
    payload.validate()?;
    Ok(payload)
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

    pub fn mark_files_handled(&self, paths: Vec<PathBuf>) -> Result<()> {
        self.commands
            .send(Command::MarkFilesHandled(paths))
            .context("clipboard backend stopped")
    }

    pub fn rebaseline(&self) -> Result<()> {
        self.commands
            .send(Command::Rebaseline)
            .context("clipboard backend stopped")
    }
}
