use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, broadcast, mpsc},
};

use crate::{
    clipboard::{Change as ClipboardChange, Clipboard},
    config::Config,
    ipc::{self, Response},
    network::{self, SecureConnection},
    ordering::ClockRelation,
    protocol::{ClipboardEvent, Message, PROTOCOL_VERSION},
    transfer::{self, Manager as TransferManager},
};

#[derive(Clone, Debug)]
struct BusEvent {
    source: Option<String>,
    target: Option<String>,
    message: Message,
}

#[derive(Debug)]
struct Incoming {
    peer: String,
    message: Message,
}

#[derive(Clone)]
struct NetworkContext {
    cfg: Arc<RwLock<Config>>,
    active: Arc<Mutex<HashSet<String>>>,
    latest: Arc<RwLock<Option<ClipboardEvent>>>,
    bus: broadcast::Sender<BusEvent>,
    incoming: mpsc::UnboundedSender<Incoming>,
}

enum Control {
    Pause,
    Resume,
    SyncFiles {
        payload: crate::protocol::ClipboardPayload,
        paths: Vec<std::path::PathBuf>,
    },
}

pub async fn run() -> Result<()> {
    configure_wayland_windows();
    let cfg = Config::load_or_create()?;
    let id = network::device_id(&cfg.public_key()?);
    let mut clipboard = Clipboard::start()?;
    let backend = clipboard.backend;
    let ipc_listener = ipc::bind().context("another daemon may already be running")?;
    let _socket_guard = ipc::SocketGuard;
    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let port = listener.local_addr()?.port();
    let discovery = network::start_discovery(&id, port)?;

    let cfg = Arc::new(RwLock::new(cfg));
    if cfg.read().await.cursor.enabled {
        crate::input::spawn(cfg.clone(), id.clone())?;
    }
    let active = Arc::new(Mutex::new(HashSet::new()));
    let latest: Arc<RwLock<Option<ClipboardEvent>>> = Arc::new(RwLock::new(None));
    let (bus_tx, _) = broadcast::channel::<BusEvent>(128);
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<Incoming>();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Control>();
    let (transfer_tx, mut transfer_rx) = mpsc::unbounded_channel();
    let transfers = TransferManager::new(transfer_tx);

    let network = NetworkContext {
        cfg: cfg.clone(),
        active: active.clone(),
        latest: latest.clone(),
        bus: bus_tx.clone(),
        incoming: incoming_tx,
    };
    tokio::spawn(network_manager(listener, discovery, id.clone(), network));
    tokio::spawn(ipc_server(
        ipc_listener,
        cfg.clone(),
        active.clone(),
        backend,
        control_tx,
        bus_tx.clone(),
        transfers.clone(),
    ));

    tracing::info!(device_id = %id, %port, backend, "lan-cat daemon started");
    let mut clock = cfg.read().await.clock.clone();
    let mut current: Option<ClipboardEvent> = None;
    let mut seen_order = VecDeque::new();
    let mut seen = HashSet::new();
    if !cfg.read().await.paused {
        if let Some(payload) = clipboard.initial_payload.take() {
            let sequence = clock.increment(&id);
            {
                let mut value = cfg.write().await;
                value.clock = clock.clone();
                value.save()?;
            }
            let event = ClipboardEvent::new(id.clone(), sequence, clock.clone(), payload)?;
            remember(event.id, &mut seen, &mut seen_order);
            current = Some(event.clone());
            *latest.write().await = Some(event);
        }
    }

    loop {
        tokio::select! {
            local = clipboard.changes.recv() => {
                let Some(change) = local else { bail!("clipboard backend stopped"); };
                if cfg.read().await.paused { continue; }
                let ClipboardChange::Payload(payload) = change else {
                    if let ClipboardChange::Files(paths) = change {
                        if let Err(error) = spawn_copy_prompt(paths) {
                            tracing::warn!(%error, "failed to open copied-file prompt");
                        }
                    }
                    continue;
                };
                publish_local_payload(
                    payload,
                    &id,
                    &mut clock,
                    &cfg,
                    &mut seen,
                    &mut seen_order,
                    &mut current,
                    &latest,
                    &bus_tx,
                ).await?;
            }
            incoming = incoming_rx.recv() => {
                let Some(Incoming { peer, message }) = incoming else { bail!("network manager stopped"); };
                if let Message::Transfer(message) = message {
                    if let Err(error) = transfers.handle(peer, message).await {
                        tracing::warn!(%error, "rejected file transfer message");
                    }
                    continue;
                }
                let Message::Clipboard(event) = message else { continue };
                if cfg.read().await.paused || seen.contains(&event.id) { continue; }
                if let Err(error) = event.validate() {
                    tracing::warn!(%peer, %error, "rejected clipboard event");
                    continue;
                }
                if !cfg.read().await.peers.contains_key(&event.origin) {
                    tracing::warn!(origin = %event.origin, "rejected event from untrusted origin");
                    continue;
                }
                remember(event.id, &mut seen, &mut seen_order);
                clock.merge(&event.clock);
                {
                    let mut value = cfg.write().await;
                    value.clock = clock.clone();
                    value.save()?;
                }
                let wins = current.as_ref().is_none_or(|old| match event.clock.relation(&old.clock) {
                    ClockRelation::After => true,
                    ClockRelation::Concurrent => event.origin > old.origin,
                    ClockRelation::Before | ClockRelation::Equal => false,
                });
                // Forward every valid unseen event; each peer applies same deterministic ordering.
                let _ = bus_tx.send(BusEvent { source: Some(peer), target: None, message: Message::Clipboard(event.clone()) });
                if wins {
                    clipboard.set_payload(event.payload.clone())?;
                    current = Some(event.clone());
                    *latest.write().await = Some(event);
                }
            }
            control = control_rx.recv() => match control {
                Some(Control::Pause) => { *latest.write().await = None; }
                Some(Control::Resume) => {
                    clipboard.rebaseline()?;
                    current = None;
                    *latest.write().await = None;
                }
                Some(Control::SyncFiles { payload, paths }) => {
                    clipboard.mark_files_handled(paths)?;
                    publish_local_payload(
                        payload,
                        &id,
                        &mut clock,
                        &cfg,
                        &mut seen,
                        &mut seen_order,
                        &mut current,
                        &latest,
                        &bus_tx,
                    ).await?;
                }
                None => bail!("IPC control channel stopped"),
            },
            outbound = transfer_rx.recv() => {
                let Some(transfer::Outbound { peer, message }) = outbound else {
                    bail!("transfer manager stopped");
                };
                let _ = bus_tx.send(BusEvent {
                    source: None,
                    target: Some(peer),
                    message: Message::Transfer(message),
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_wayland_windows() {
    if std::env::var_os("SWAYSOCK").is_none() {
        return;
    }
    if !register_sway_rule(
        r#"[app_id="lan-cat-copy-prompt"]"#,
        "floating enable, move position center",
    ) {
        tracing::warn!("could not register Sway floating rule for copy prompt");
    }
    if !register_sway_rule(
        r#"[app_id="lan-cat-cursor-portal"]"#,
        "floating enable, border none, move position 0 0, resize set 100 ppt 100 ppt",
    ) {
        tracing::warn!("could not register Sway floating rule for cursor portal");
    }
}

#[cfg(target_os = "linux")]
fn register_sway_rule(criteria: &str, command: &str) -> bool {
    std::process::Command::new("swaymsg")
        .args(["for_window", criteria, command])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(target_os = "linux"))]
fn configure_wayland_windows() {}

#[allow(clippy::too_many_arguments)]
async fn publish_local_payload(
    payload: crate::protocol::ClipboardPayload,
    id: &str,
    clock: &mut crate::ordering::VersionVector,
    cfg: &Arc<RwLock<Config>>,
    seen: &mut HashSet<uuid::Uuid>,
    seen_order: &mut VecDeque<uuid::Uuid>,
    current: &mut Option<ClipboardEvent>,
    latest: &Arc<RwLock<Option<ClipboardEvent>>>,
    bus: &broadcast::Sender<BusEvent>,
) -> Result<()> {
    let sequence = clock.increment(id);
    {
        let mut value = cfg.write().await;
        value.clock = clock.clone();
        value.save()?;
    }
    let event = ClipboardEvent::new(id.to_owned(), sequence, clock.clone(), payload)?;
    remember(event.id, seen, seen_order);
    *current = Some(event.clone());
    *latest.write().await = Some(event.clone());
    let _ = bus.send(BusEvent {
        source: None,
        target: None,
        message: Message::Clipboard(event),
    });
    Ok(())
}

fn spawn_copy_prompt(paths: Vec<std::path::PathBuf>) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/lan-cat-copy-prompt.log")
        .context("open copy prompt log")?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("copy-share-ui")
        .arg("--")
        .args(paths)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .context("launch copied-file prompt")?;
    Ok(())
}

fn remember(id: uuid::Uuid, seen: &mut HashSet<uuid::Uuid>, order: &mut VecDeque<uuid::Uuid>) {
    seen.insert(id);
    order.push_back(id);
    if order.len() > 4096 {
        if let Some(old) = order.pop_front() {
            seen.remove(&old);
        }
    }
}

async fn network_manager(
    listener: TcpListener,
    discovery: network::Discovery,
    local_id: String,
    context: NetworkContext,
) {
    let browse = discovery.receiver.clone();
    let dialing = Arc::new(Mutex::new(HashSet::new()));
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((mut stream, _)) => {
                    match network::read_peer_preface(&mut stream).await {
                        Ok(peer_id) if local_id > peer_id => {
                            let context = context.clone();
                            tokio::spawn(async move {
                                if tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    spawn_peer(stream, false, peer_id, context),
                                ).await.is_err() {
                                    tracing::warn!("inbound authentication timed out");
                                }
                            });
                        }
                        Ok(_) => tracing::debug!("ignored duplicate-direction inbound connection"),
                        Err(error) => tracing::warn!(%error, "invalid inbound connection"),
                    }
                }
                Err(error) => tracing::warn!(%error, "TCP accept failed"),
            },
            event = browse.recv_async() => match event {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let Some((peer_id, addr)) = network::resolved_peer(&info) else { continue };
                    if local_id >= peer_id || dialing.lock().await.contains(&peer_id) { continue; }
                    if !context.cfg.read().await.peers.contains_key(&peer_id) { continue; }
                    dialing.lock().await.insert(peer_id.clone());
                    tokio::spawn(outbound_supervisor(addr, peer_id, dialing.clone(), context.clone()));
                }
                Ok(_) => {},
                Err(error) => { tracing::error!(%error, "mDNS browser stopped"); break; }
            }
        }
    }
}

async fn outbound_supervisor(
    addr: std::net::SocketAddr,
    peer_id: String,
    dialing: Arc<Mutex<HashSet<String>>>,
    context: NetworkContext,
) {
    let mut delay = 1_u64;
    loop {
        let snapshot = context.cfg.read().await.clone();
        let Some(peer) = snapshot.peers.get(&peer_id) else {
            break;
        };
        match SecureConnection::connect(addr, &snapshot, peer).await {
            Ok(connection) => {
                context.active.lock().await.insert(peer_id.clone());
                let result = peer_loop(
                    connection,
                    &peer_id,
                    context.cfg.clone(),
                    context.latest.clone(),
                    context.bus.clone(),
                    context.incoming.clone(),
                )
                .await;
                context.active.lock().await.remove(&peer_id);
                if let Err(error) = result {
                    tracing::debug!(%peer_id, %error, "peer disconnected");
                }
                delay = 1;
            }
            Err(error) => {
                tracing::debug!(%peer_id, %error, retry_seconds = delay, "peer connect failed")
            }
        }
        if !context.cfg.read().await.peers.contains_key(&peer_id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        delay = (delay * 2).min(60);
    }
    dialing.lock().await.remove(&peer_id);
}

use mdns_sd::ServiceEvent;

async fn spawn_peer(stream: TcpStream, initiator: bool, peer_id: String, context: NetworkContext) {
    if initiator || context.active.lock().await.contains(&peer_id) {
        return;
    }
    let peer = context.cfg.read().await.peers.get(&peer_id).cloned();
    let Some(peer) = peer else { return };
    let snapshot = context.cfg.read().await.clone();
    match SecureConnection::accept(stream, &snapshot, &peer).await {
        Ok(connection) => spawn_connected(connection, peer_id, context).await,
        Err(error) => tracing::warn!(%peer_id, %error, "peer authentication failed"),
    }
}

async fn spawn_connected(connection: SecureConnection, peer_id: String, context: NetworkContext) {
    if !context.active.lock().await.insert(peer_id.clone()) {
        return;
    }
    tokio::spawn(async move {
        let result = peer_loop(
            connection,
            &peer_id,
            context.cfg.clone(),
            context.latest.clone(),
            context.bus.clone(),
            context.incoming.clone(),
        )
        .await;
        context.active.lock().await.remove(&peer_id);
        if let Err(error) = result {
            tracing::debug!(%peer_id, %error, "peer disconnected");
        }
    });
}

async fn peer_loop(
    connection: SecureConnection,
    peer_id: &str,
    cfg: Arc<RwLock<Config>>,
    latest: Arc<RwLock<Option<ClipboardEvent>>>,
    bus: broadcast::Sender<BusEvent>,
    incoming: mpsc::UnboundedSender<Incoming>,
) -> Result<()> {
    let local_id = network::device_id(&cfg.read().await.public_key()?);
    connection
        .send(&Message::Hello {
            version: PROTOCOL_VERSION,
            device_id: local_id,
        })
        .await?;
    match connection.receive().await? {
        Message::Hello {
            version: PROTOCOL_VERSION,
            device_id,
        } if device_id == peer_id => {}
        _ => bail!("peer hello mismatch"),
    }
    if let Some(event) = latest.read().await.clone() {
        connection.send(&Message::Clipboard(event)).await?;
    }
    let mut outbound = bus.subscribe();
    loop {
        tokio::select! {
            message = connection.receive() => {
                incoming.send(Incoming { peer: peer_id.to_owned(), message: message? })?;
            }
            event = outbound.recv() => {
                let event = event?;
                if event.source.as_deref() == Some(peer_id) { continue; }
                if event.target.as_deref().is_some_and(|target| target != peer_id) { continue; }
                if !cfg.read().await.peers.contains_key(peer_id) { bail!("peer was unpaired"); }
                connection.send(&event.message).await?;
            }
        }
    }
}

async fn ipc_server(
    listener: tokio::net::UnixListener,
    cfg: Arc<RwLock<Config>>,
    active: Arc<Mutex<HashSet<String>>>,
    backend: &'static str,
    control: mpsc::UnboundedSender<Control>,
    bus: broadcast::Sender<BusEvent>,
    transfers: Arc<TransferManager>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let cfg = cfg.clone();
        let active = active.clone();
        let control = control.clone();
        let bus = bus.clone();
        let transfers = transfers.clone();
        tokio::spawn(async move {
            let response = match ipc::read(&mut stream).await {
                Ok(request) => {
                    handle_ipc(request, cfg, active, backend, control, bus, transfers).await
                }
                Err(error) => Response {
                    ok: false,
                    message: error.to_string(),
                    data: None,
                },
            };
            let _ = ipc::write(&mut stream, response).await;
        });
    }
}

async fn handle_ipc(
    request: ipc::Request,
    cfg: Arc<RwLock<Config>>,
    active: Arc<Mutex<HashSet<String>>>,
    backend: &'static str,
    control: mpsc::UnboundedSender<Control>,
    bus: broadcast::Sender<BusEvent>,
    transfers: Arc<TransferManager>,
) -> Response {
    let result: Result<(String, Option<ipc::ResponseData>)> = async {
        match request {
            ipc::Request::Status => {
                let cfg = cfg.read().await;
                Ok((
                    format!(
                        "{}; backend={backend}; paired={}; connected={}",
                        if cfg.paused { "paused" } else { "running" },
                        cfg.peers.len(),
                        active.lock().await.len()
                    ),
                    None,
                ))
            }
            ipc::Request::Pause => {
                let mut value = cfg.write().await;
                value.paused = true;
                value.save()?;
                control.send(Control::Pause)?;
                Ok((
                    "Synchronization paused; queued content discarded.".into(),
                    None,
                ))
            }
            ipc::Request::Resume => {
                let mut value = cfg.write().await;
                value.paused = false;
                value.save()?;
                control.send(Control::Resume)?;
                Ok((
                    "Synchronization resumed from current clipboard baseline.".into(),
                    None,
                ))
            }
            ipc::Request::ClipboardSyncFiles { paths } => {
                if cfg.read().await.paused {
                    bail!("clipboard synchronization is paused");
                }
                let handled_paths = paths.clone();
                let payload = tokio::task::spawn_blocking(move || {
                    crate::clipboard::payload_from_paths(paths)
                })
                .await??;
                control.send(Control::SyncFiles {
                    payload,
                    paths: handled_paths,
                })?;
                Ok(("Files queued for clipboard synchronization.".into(), None))
            }
            ipc::Request::Unpair { peer } => {
                let mut value = cfg.write().await;
                let matches: Vec<_> = value
                    .peers
                    .keys()
                    .filter(|id| id.starts_with(&peer))
                    .cloned()
                    .collect();
                if matches.len() != 1 {
                    bail!("peer ID must uniquely match one trusted peer");
                }
                let id = &matches[0];
                value.peers.remove(id);
                value.save()?;
                let _ = bus.send(BusEvent {
                    source: None,
                    target: None,
                    message: Message::Ping,
                });
                Ok((format!("Unpaired {id}."), None))
            }
            ipc::Request::PeerList => {
                let cfg = cfg.read().await;
                let connected = active.lock().await;
                let peers = cfg
                    .peers
                    .iter()
                    .map(|(id, peer)| ipc::PeerView {
                        id: id.clone(),
                        name: peer.name.clone(),
                        connected: connected.contains(id),
                    })
                    .collect();
                Ok((
                    "Peer list.".into(),
                    Some(ipc::ResponseData::Peers { peers }),
                ))
            }
            ipc::Request::TransferStart { peer, paths } => {
                if !active.lock().await.contains(&peer) {
                    bail!("selected peer is not connected");
                }
                let id = transfers.start(peer, paths).await?;
                Ok((
                    format!("Transfer {id} offered."),
                    Some(ipc::ResponseData::Started { id }),
                ))
            }
            ipc::Request::TransferList => {
                let values = transfers.views().await;
                Ok((
                    "Transfer list.".into(),
                    Some(ipc::ResponseData::Transfers { transfers: values }),
                ))
            }
            ipc::Request::TransferGet { id } => {
                let transfer = transfers.view(id).await.context("unknown transfer")?;
                Ok((
                    "Transfer status.".into(),
                    Some(ipc::ResponseData::Transfer { transfer }),
                ))
            }
            ipc::Request::TransferAccept { id, destination } => {
                transfers.accept(id, destination).await?;
                Ok(("Transfer accepted.".into(), None))
            }
            ipc::Request::TransferReject { id } => {
                transfers.reject(id).await?;
                Ok(("Transfer rejected.".into(), None))
            }
            ipc::Request::TransferCancel { id } => {
                transfers.cancel(id).await?;
                Ok(("Transfer cancelled.".into(), None))
            }
        }
    }
    .await;
    match result {
        Ok((message, data)) => Response {
            ok: true,
            message,
            data,
        },
        Err(error) => Response {
            ok: false,
            message: error.to_string(),
            data: None,
        },
    }
}
