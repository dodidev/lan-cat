pub mod protocol;

use std::{
    collections::HashMap,
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(not(test))]
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, sync::mpsc};
use uuid::Uuid;

use self::protocol::{CHUNK_BYTES, FileManifest, TransferMessage, validate_manifest};

#[derive(Clone, Debug)]
pub struct Outbound {
    pub peer: String,
    pub message: TransferMessage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Waiting,
    Transferring,
    Completed,
    Rejected,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferView {
    pub id: Uuid,
    pub direction: Direction,
    pub peer: String,
    pub files: Vec<FileManifest>,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub state: TransferState,
    pub destination: Option<PathBuf>,
    pub error: Option<String>,
}

struct Job {
    view: TransferView,
    kind: JobKind,
}

enum JobKind {
    Outgoing { paths: Vec<PathBuf> },
    Incoming(IncomingJob),
}

struct IncomingJob {
    part_dir: Option<PathBuf>,
    offsets: Vec<u64>,
}

struct Inner {
    jobs: HashMap<Uuid, Job>,
    signals: HashMap<Uuid, mpsc::UnboundedSender<TransferMessage>>,
}

pub struct Manager {
    inner: tokio::sync::Mutex<Inner>,
    outbound: mpsc::UnboundedSender<Outbound>,
}

impl Manager {
    pub fn new(outbound: mpsc::UnboundedSender<Outbound>) -> Arc<Self> {
        Arc::new(Self {
            inner: tokio::sync::Mutex::new(Inner {
                jobs: HashMap::new(),
                signals: HashMap::new(),
            }),
            outbound,
        })
    }

    pub async fn start(self: &Arc<Self>, peer: String, paths: Vec<PathBuf>) -> Result<Uuid> {
        let (files, total_bytes) = inspect_paths(&paths)?;
        validate_manifest(&files, total_bytes)?;
        let id = Uuid::new_v4();
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        {
            let mut inner = self.inner.lock().await;
            inner.signals.insert(id, signal_tx);
            inner.jobs.insert(
                id,
                Job {
                    view: TransferView {
                        id,
                        direction: Direction::Send,
                        peer: peer.clone(),
                        files: files.clone(),
                        total_bytes,
                        transferred_bytes: 0,
                        state: TransferState::Waiting,
                        destination: None,
                        error: None,
                    },
                    kind: JobKind::Outgoing { paths },
                },
            );
        }
        self.send(
            &peer,
            TransferMessage::Offer {
                id,
                files,
                total_bytes,
            },
        )?;
        let manager = self.clone();
        tokio::spawn(async move {
            let result = manager.clone().run_sender(id, signal_rx).await;
            if let Err(error) = result {
                manager.fail(id, error.to_string()).await;
            } else {
                manager.inner.lock().await.signals.remove(&id);
            }
        });
        Ok(id)
    }

    pub async fn accept(&self, id: Uuid, destination: PathBuf) -> Result<()> {
        fs::create_dir_all(&destination)?;
        let mut inner = self.inner.lock().await;
        let job = inner.jobs.get_mut(&id).context("unknown transfer")?;
        if job.view.direction != Direction::Receive || job.view.state != TransferState::Waiting {
            bail!("transfer is not waiting for acceptance");
        }
        for file in &job.view.files {
            if destination.join(&file.name).exists() {
                bail!("destination already contains {}", file.name);
            }
        }
        let part_dir = destination.join(format!(".lan-cat-{id}.part"));
        fs::create_dir(&part_dir)?;
        for file in &job.view.files {
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(part_dir.join(format!("{}.part", file.name)))?;
        }
        let JobKind::Incoming(incoming) = &mut job.kind else {
            bail!("invalid transfer direction");
        };
        incoming.part_dir = Some(part_dir);
        job.view.destination = Some(destination);
        job.view.state = TransferState::Transferring;
        let peer = job.view.peer.clone();
        drop(inner);
        self.send(&peer, TransferMessage::Response { id, accepted: true })
    }

    pub async fn reject(&self, id: Uuid) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let job = inner.jobs.get_mut(&id).context("unknown transfer")?;
        if job.view.direction != Direction::Receive || job.view.state != TransferState::Waiting {
            bail!("transfer is not waiting for rejection");
        }
        job.view.state = TransferState::Rejected;
        let peer = job.view.peer.clone();
        drop(inner);
        self.send(
            &peer,
            TransferMessage::Response {
                id,
                accepted: false,
            },
        )
    }

    pub async fn cancel(&self, id: Uuid) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let job = inner.jobs.get_mut(&id).context("unknown transfer")?;
        if matches!(
            job.view.state,
            TransferState::Completed | TransferState::Rejected | TransferState::Cancelled
        ) {
            return Ok(());
        }
        job.view.state = TransferState::Cancelled;
        let peer = job.view.peer.clone();
        cleanup(job);
        if let Some(signal) = inner.signals.get(&id) {
            let _ = signal.send(TransferMessage::Cancel {
                id,
                reason: "cancelled by user".into(),
            });
        }
        drop(inner);
        self.send(
            &peer,
            TransferMessage::Cancel {
                id,
                reason: "cancelled by user".into(),
            },
        )
    }

    pub async fn view(&self, id: Uuid) -> Option<TransferView> {
        self.inner
            .lock()
            .await
            .jobs
            .get(&id)
            .map(|job| job.view.clone())
    }

    pub async fn views(&self) -> Vec<TransferView> {
        self.inner
            .lock()
            .await
            .jobs
            .values()
            .map(|job| job.view.clone())
            .collect()
    }

    pub async fn handle(self: &Arc<Self>, peer: String, message: TransferMessage) -> Result<()> {
        match message {
            TransferMessage::Offer {
                id,
                files,
                total_bytes,
            } => {
                validate_manifest(&files, total_bytes)?;
                let mut inner = self.inner.lock().await;
                if inner.jobs.contains_key(&id) {
                    return Ok(());
                }
                inner.jobs.insert(
                    id,
                    Job {
                        view: TransferView {
                            id,
                            direction: Direction::Receive,
                            peer,
                            files: files.clone(),
                            total_bytes,
                            transferred_bytes: 0,
                            state: TransferState::Waiting,
                            destination: default_destination(),
                            error: None,
                        },
                        kind: JobKind::Incoming(IncomingJob {
                            part_dir: None,
                            offsets: vec![0; files.len()],
                        }),
                    },
                );
                drop(inner);
                launch_receive_ui(id);
            }
            TransferMessage::Response { id, accepted } => {
                self.ensure_peer(id, &peer).await?;
                self.route_signal(id, TransferMessage::Response { id, accepted })
                    .await?;
            }
            TransferMessage::Ack {
                id,
                file_index,
                next_offset,
            } => {
                self.ensure_peer(id, &peer).await?;
                self.route_signal(
                    id,
                    TransferMessage::Ack {
                        id,
                        file_index,
                        next_offset,
                    },
                )
                .await?;
            }
            TransferMessage::Chunk {
                id,
                file_index,
                offset,
                data,
            } => {
                if data.len() > CHUNK_BYTES {
                    bail!("transfer chunk exceeds limit");
                }
                let mut inner = self.inner.lock().await;
                let job = inner.jobs.get_mut(&id).context("unknown transfer")?;
                if job.view.peer != peer || job.view.state != TransferState::Transferring {
                    bail!("unexpected transfer chunk");
                }
                let JobKind::Incoming(incoming) = &mut job.kind else {
                    bail!("chunk received for outgoing transfer");
                };
                let index = file_index as usize;
                let manifest = job.view.files.get(index).context("invalid file index")?;
                let next = offset
                    .checked_add(data.len() as u64)
                    .context("transfer offset overflow")?;
                if incoming.offsets.get(index).copied() != Some(offset) || next > manifest.size {
                    bail!("invalid transfer chunk offset");
                }
                let part_dir = incoming
                    .part_dir
                    .as_ref()
                    .context("transfer not accepted")?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .open(part_dir.join(format!("{}.part", manifest.name)))?;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(&data)?;
                incoming.offsets[index] = next;
                job.view.transferred_bytes += data.len() as u64;
                let peer = job.view.peer.clone();
                drop(inner);
                self.send(
                    &peer,
                    TransferMessage::Ack {
                        id,
                        file_index,
                        next_offset: next,
                    },
                )?;
            }
            TransferMessage::Complete { id } => {
                let mut inner = self.inner.lock().await;
                let job = inner.jobs.get_mut(&id).context("unknown transfer")?;
                if job.view.peer != peer || job.view.transferred_bytes != job.view.total_bytes {
                    bail!("incomplete transfer");
                }
                finish_incoming(job)?;
                job.view.state = TransferState::Completed;
                let peer = job.view.peer.clone();
                drop(inner);
                self.send(&peer, TransferMessage::Finished { id })?;
            }
            TransferMessage::Finished { id } => {
                self.ensure_peer(id, &peer).await?;
                self.route_signal(id, TransferMessage::Finished { id })
                    .await?;
            }
            TransferMessage::Cancel { id, reason } => {
                if reason.len() > 1024 {
                    bail!("transfer cancellation reason is too long");
                }
                let mut inner = self.inner.lock().await;
                if let Some(job) = inner.jobs.get_mut(&id) {
                    if job.view.peer != peer {
                        bail!("transfer peer mismatch");
                    }
                    job.view.state = TransferState::Cancelled;
                    job.view.error = Some(reason.clone());
                    cleanup(job);
                }
                if let Some(signal) = inner.signals.get(&id) {
                    let _ = signal.send(TransferMessage::Cancel { id, reason });
                }
            }
        }
        Ok(())
    }

    async fn run_sender(
        self: Arc<Self>,
        id: Uuid,
        mut signals: mpsc::UnboundedReceiver<TransferMessage>,
    ) -> Result<()> {
        match tokio::time::timeout(Duration::from_secs(300), signals.recv()).await {
            Ok(Some(TransferMessage::Response { accepted: true, .. })) => {}
            Ok(Some(TransferMessage::Response {
                accepted: false, ..
            })) => {
                self.set_state(id, TransferState::Rejected).await?;
                return Ok(());
            }
            Ok(Some(TransferMessage::Cancel { reason, .. })) => bail!(reason),
            _ => bail!("transfer offer timed out"),
        }
        self.set_state(id, TransferState::Transferring).await?;
        let (peer, paths) = {
            let inner = self.inner.lock().await;
            let job = inner.jobs.get(&id).context("unknown transfer")?;
            let JobKind::Outgoing { paths } = &job.kind else {
                bail!("invalid transfer direction");
            };
            (job.view.peer.clone(), paths.clone())
        };
        for (file_index, path) in paths.iter().enumerate() {
            let mut file = tokio::fs::File::open(path).await?;
            let mut offset = 0_u64;
            loop {
                let mut data = vec![0; CHUNK_BYTES];
                let read = file.read(&mut data).await?;
                if read == 0 {
                    break;
                }
                data.truncate(read);
                self.send(
                    &peer,
                    TransferMessage::Chunk {
                        id,
                        file_index: file_index as u32,
                        offset,
                        data,
                    },
                )?;
                let wanted = offset + read as u64;
                match tokio::time::timeout(Duration::from_secs(30), signals.recv()).await {
                    Ok(Some(TransferMessage::Ack {
                        file_index: got_index,
                        next_offset,
                        ..
                    })) if got_index == file_index as u32 && next_offset == wanted => {}
                    Ok(Some(TransferMessage::Cancel { reason, .. })) => bail!(reason),
                    _ => bail!("transfer acknowledgement timed out"),
                }
                offset = wanted;
                self.set_progress(id, read as u64).await?;
            }
        }
        self.send(&peer, TransferMessage::Complete { id })?;
        match tokio::time::timeout(Duration::from_secs(30), signals.recv()).await {
            Ok(Some(TransferMessage::Finished { .. })) => {
                self.set_state(id, TransferState::Completed).await?;
            }
            Ok(Some(TransferMessage::Cancel { reason, .. })) => bail!(reason),
            _ => bail!("transfer completion timed out"),
        }
        Ok(())
    }

    async fn route_signal(&self, id: Uuid, message: TransferMessage) -> Result<()> {
        let inner = self.inner.lock().await;
        let signal = inner
            .signals
            .get(&id)
            .context("unknown outgoing transfer")?;
        signal.send(message).context("transfer sender stopped")
    }

    async fn set_state(&self, id: Uuid, state: TransferState) -> Result<()> {
        let mut inner = self.inner.lock().await;
        inner
            .jobs
            .get_mut(&id)
            .context("unknown transfer")?
            .view
            .state = state;
        Ok(())
    }

    async fn ensure_peer(&self, id: Uuid, peer: &str) -> Result<()> {
        let inner = self.inner.lock().await;
        let job = inner.jobs.get(&id).context("unknown transfer")?;
        if job.view.peer != peer {
            bail!("transfer peer mismatch");
        }
        Ok(())
    }

    async fn set_progress(&self, id: Uuid, bytes: u64) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let view = &mut inner.jobs.get_mut(&id).context("unknown transfer")?.view;
        view.transferred_bytes = view
            .transferred_bytes
            .checked_add(bytes)
            .context("transfer progress overflow")?;
        Ok(())
    }

    async fn fail(&self, id: Uuid, error: String) {
        let mut inner = self.inner.lock().await;
        if let Some(job) = inner.jobs.get_mut(&id) {
            if !matches!(
                job.view.state,
                TransferState::Completed | TransferState::Rejected | TransferState::Cancelled
            ) {
                job.view.state = TransferState::Failed;
                job.view.error = Some(error.clone());
                cleanup(job);
                let _ = self.outbound.send(Outbound {
                    peer: job.view.peer.clone(),
                    message: TransferMessage::Cancel { id, reason: error },
                });
            }
        }
        inner.signals.remove(&id);
    }

    fn send(&self, peer: &str, message: TransferMessage) -> Result<()> {
        self.outbound
            .send(Outbound {
                peer: peer.to_owned(),
                message,
            })
            .context("network transfer channel stopped")
    }
}

fn inspect_paths(paths: &[PathBuf]) -> Result<(Vec<FileManifest>, u64)> {
    let mut files = Vec::with_capacity(paths.len());
    let mut total = 0_u64;
    for path in paths {
        let metadata =
            fs::metadata(path).with_context(|| format!("read metadata for {}", path.display()))?;
        if !metadata.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("file name is not valid UTF-8")?
            .to_owned();
        total = total
            .checked_add(metadata.len())
            .context("transfer size overflow")?;
        files.push(FileManifest {
            name,
            size: metadata.len(),
        });
    }
    Ok((files, total))
}

fn finish_incoming(job: &mut Job) -> Result<()> {
    let JobKind::Incoming(incoming) = &mut job.kind else {
        bail!("completion received for outgoing transfer");
    };
    for (index, file) in job.view.files.iter().enumerate() {
        if incoming.offsets[index] != file.size {
            bail!("file {} is incomplete", file.name);
        }
    }
    let part_dir = incoming
        .part_dir
        .as_ref()
        .context("transfer was not accepted")?;
    let destination = job
        .view
        .destination
        .as_ref()
        .context("missing destination")?;
    for file in &job.view.files {
        fs::rename(
            part_dir.join(format!("{}.part", file.name)),
            destination.join(&file.name),
        )?;
    }
    fs::remove_dir(part_dir)?;
    incoming.part_dir = None;
    Ok(())
}

fn cleanup(job: &mut Job) {
    if let JobKind::Incoming(incoming) = &mut job.kind {
        if let Some(path) = incoming.part_dir.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn default_destination() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

#[cfg(not(test))]
fn launch_receive_ui(id: Uuid) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    if let Err(error) = std::process::Command::new(executable)
        .arg("transfer-ui")
        .arg("--id")
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        tracing::warn!(%error, "failed to open transfer dialog");
    }
}

#[cfg(test)]
fn launch_receive_ui(_id: Uuid) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transfer_round_trip_with_acknowledgements() {
        let source_dir = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("report.txt");
        let contents = vec![42_u8; CHUNK_BYTES * 2 + 17];
        fs::write(&source, &contents).unwrap();

        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        let a = Manager::new(a_tx);
        let b = Manager::new(b_tx);

        let b_router = b.clone();
        tokio::spawn(async move {
            while let Some(outbound) = a_rx.recv().await {
                b_router.handle("A".into(), outbound.message).await.unwrap();
            }
        });
        let a_router = a.clone();
        tokio::spawn(async move {
            while let Some(outbound) = b_rx.recv().await {
                a_router.handle("B".into(), outbound.message).await.unwrap();
            }
        });

        let id = a.start("B".into(), vec![source]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if b.view(id).await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        b.accept(id, destination.path().to_owned()).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if a.view(id).await.unwrap().state == TransferState::Completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        assert_eq!(
            fs::read(destination.path().join("report.txt")).unwrap(),
            contents
        );
        assert_eq!(b.view(id).await.unwrap().state, TransferState::Completed);
    }
}
