use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::{
    net::UdpSocket,
    sync::{RwLock, mpsc},
};
use x25519_dalek::{X25519_BASEPOINT_BYTES, x25519};

use super::protocol::{InputMessage, PointerInput};
use crate::config::Config;

const SERVICE: &str = "_lan-cat-input._udp.local.";
const PORT: u16 = 4242;
const WIRE_VERSION: u8 = 1;
const MAX_DATAGRAM: usize = 4096;
const MAX_PENDING_MESSAGES: usize = 32;

#[derive(Debug)]
pub struct Outbound {
    pub peer: String,
    pub message: InputMessage,
}

#[derive(Debug)]
pub struct Inbound {
    pub peer: String,
    pub message: InputMessage,
}

#[derive(Serialize, Deserialize)]
enum Wire {
    Hello {
        version: u8,
        sender: String,
        ephemeral: [u8; 32],
        nonce: [u8; 16],
        response: bool,
        tag: [u8; 32],
    },
    Data {
        version: u8,
        sender: String,
        sequence: u64,
        ciphertext: Vec<u8>,
    },
}

struct Session {
    addr: SocketAddr,
    send_key: [u8; 32],
    receive_key: [u8; 32],
    send_sequence: u64,
    receive_highest: u64,
    receive_window: u64,
}

pub async fn start(
    cfg: Arc<RwLock<Config>>,
    local_id: &str,
) -> Result<(
    mpsc::UnboundedSender<Outbound>,
    mpsc::UnboundedReceiver<Inbound>,
)> {
    let socket = UdpSocket::bind(("0.0.0.0", PORT))
        .await
        .context("bind cursor UDP port 4242")?;
    let mdns = publish(local_id, PORT)?;
    let browse = mdns.browse(SERVICE)?;
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let local_id = local_id.to_owned();
    tokio::task::spawn_local(async move {
        if let Err(error) = run(socket, mdns, browse, cfg, local_id, outbound_rx, inbound_tx).await
        {
            tracing::error!(%error, "cursor UDP transport stopped");
        }
    });
    Ok((outbound_tx, inbound_rx))
}

async fn run(
    socket: UdpSocket,
    mdns: ServiceDaemon,
    browse: mdns_sd::Receiver<ServiceEvent>,
    cfg: Arc<RwLock<Config>>,
    local_id: String,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    inbound: mpsc::UnboundedSender<Inbound>,
) -> Result<()> {
    let ephemeral_secret: [u8; 32] = rand::random();
    let ephemeral_public = x25519(ephemeral_secret, X25519_BASEPOINT_BYTES);
    let local_nonce: [u8; 16] = rand::random();
    let mut addresses = HashMap::<String, SocketAddr>::new();
    let mut sessions = HashMap::<String, Session>::new();
    let mut pending = HashMap::<String, VecDeque<InputMessage>>::new();
    let mut seen_handshakes = HashSet::<(String, [u8; 32], [u8; 16])>::new();
    let mut buffer = vec![0_u8; MAX_DATAGRAM];
    let mut hello_tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            event = browse.recv_async() => {
                match event {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        let Some((peer, addr)) = resolved_peer(&info) else { continue };
                        let cfg = cfg.read().await;
                        if peer == local_id || !is_cursor_peer(&cfg, &peer) { continue; }
                        addresses.insert(peer.clone(), addr);
                        if let Err(error) = send_hello(
                            &socket, &cfg, &local_id, &peer, addr,
                            ephemeral_public, local_nonce,
                            false,
                        ).await {
                            tracing::debug!(%peer, %addr, %error, "cursor hello send failed");
                            addresses.remove(&peer);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::debug!(%error, "cursor mDNS browse event failed");
                    }
                }
            },
            received = socket.recv_from(&mut buffer) => {
                let Ok((len, addr)) = received else {
                    tracing::debug!(error = %received.expect_err("recv error"), "cursor UDP receive failed");
                    continue;
                };
                let cfg = cfg.read().await;
                match receive_datagram(
                    &socket,
                    &cfg,
                    &local_id,
                    &buffer[..len],
                    addr,
                    ephemeral_secret,
                    ephemeral_public,
                    local_nonce,
                    &mut sessions,
                    &mut seen_handshakes,
                    &inbound,
                ).await {
                    Ok(Some(peer)) => {
                        if let Err(error) = flush_pending(&socket, &local_id, &mut sessions, &mut pending, &peer).await {
                            tracing::debug!(%peer, %error, "cursor pending send failed");
                            sessions.remove(&peer);
                            addresses.remove(&peer);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(%addr, %error, "ignored cursor datagram");
                    }
                }
            }
            message = outbound.recv() => {
                let Some(message) = message else { break };
                let cfg = cfg.read().await;
                if !is_cursor_peer(&cfg, &message.peer) {
                    sessions.remove(&message.peer);
                    addresses.remove(&message.peer);
                    pending.remove(&message.peer);
                    continue;
                }
                if let Some(session) = sessions.get_mut(&message.peer) {
                    if let Err(error) = send_data(&socket, &local_id, session, message.message).await {
                        tracing::debug!(peer = %message.peer, %error, "cursor data send failed");
                        sessions.remove(&message.peer);
                        addresses.remove(&message.peer);
                    }
                } else if let Some(&addr) = addresses.get(&message.peer) {
                    if let Err(error) = send_hello(
                        &socket, &cfg, &local_id, &message.peer, addr,
                        ephemeral_public, local_nonce,
                        false,
                    ).await {
                        tracing::debug!(peer = %message.peer, %addr, %error, "cursor hello send failed");
                        addresses.remove(&message.peer);
                    }
                    queue_pending(&mut pending, message.peer, message.message);
                } else {
                    tracing::debug!(peer = %message.peer, "cursor peer address unavailable; queued message");
                    queue_pending(&mut pending, message.peer, message.message);
                }
            }
            _ = hello_tick.tick() => {
                let cfg = cfg.read().await;
                addresses.retain(|peer, _| is_cursor_peer(&cfg, peer));
                sessions.retain(|peer, _| is_cursor_peer(&cfg, peer));
                pending.retain(|peer, _| is_cursor_peer(&cfg, peer));
                let peers: Vec<_> = addresses.iter().map(|(peer, &addr)| (peer.clone(), addr)).collect();
                for (peer, addr) in peers {
                    if !sessions.contains_key(&peer) {
                        if let Err(error) = send_hello(
                            &socket, &cfg, &local_id, &peer, addr,
                            ephemeral_public, local_nonce,
                            false,
                        ).await {
                            tracing::debug!(%peer, %addr, %error, "cursor hello send failed");
                            addresses.remove(&peer);
                        }
                    }
                }
            }
        }
    }
    let _ = mdns.shutdown();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn receive_datagram(
    socket: &UdpSocket,
    cfg: &Config,
    local_id: &str,
    bytes: &[u8],
    addr: SocketAddr,
    ephemeral_secret: [u8; 32],
    ephemeral_public: [u8; 32],
    local_nonce: [u8; 16],
    sessions: &mut HashMap<String, Session>,
    seen_handshakes: &mut HashSet<(String, [u8; 32], [u8; 16])>,
    inbound: &mpsc::UnboundedSender<Inbound>,
) -> Result<Option<String>> {
    let wire: Wire = serde_cbor::from_slice(bytes)?;
    match wire {
        Wire::Hello {
            version,
            sender,
            ephemeral,
            nonce,
            response,
            tag,
        } => {
            if version != WIRE_VERSION || !is_cursor_peer(cfg, &sender) {
                bail!("unknown cursor peer or protocol version");
            }
            let static_shared = static_shared(cfg, &sender)?;
            let expected = hello_tag(&static_shared, &sender, &ephemeral, &nonce, response);
            if expected != tag {
                bail!("cursor hello authentication failed");
            }
            let (send_key, receive_key) = session_keys(
                &static_shared,
                x25519(ephemeral_secret, ephemeral),
                local_id,
                &sender,
                &local_nonce,
                &nonce,
            );
            let handshake = (sender.clone(), ephemeral, nonce);
            if seen_handshakes.contains(&handshake) {
                if let Some(session) = sessions.get(&sender) {
                    if session.send_key != send_key || session.addr != addr {
                        bail!("stale cursor handshake replay");
                    }
                }
            }
            seen_handshakes.insert(handshake);
            sessions
                .entry(sender.clone())
                .and_modify(|session| {
                    session.addr = addr;
                    if session.send_key != send_key {
                        *session = Session {
                            addr,
                            send_key,
                            receive_key,
                            send_sequence: 0,
                            receive_highest: 0,
                            receive_window: 0,
                        };
                    }
                })
                .or_insert(Session {
                    addr,
                    send_key,
                    receive_key,
                    send_sequence: 0,
                    receive_highest: 0,
                    receive_window: 0,
                });
            if !response {
                send_hello(
                    socket,
                    cfg,
                    local_id,
                    &sender,
                    addr,
                    ephemeral_public,
                    local_nonce,
                    true,
                )
                .await?;
            }
            Ok(Some(sender))
        }
        Wire::Data {
            version,
            sender,
            sequence,
            ciphertext,
        } => {
            if version != WIRE_VERSION || !is_cursor_peer(cfg, &sender) {
                bail!("unknown cursor peer or protocol version");
            }
            let session = sessions
                .get_mut(&sender)
                .context("cursor session is not authenticated")?;
            if addr != session.addr {
                bail!("stale or misrouted cursor datagram");
            }
            let plain = decrypt(&session.receive_key, &sender, sequence, &ciphertext)?;
            if !accept_sequence(
                &mut session.receive_highest,
                &mut session.receive_window,
                sequence,
            ) {
                bail!("duplicate or stale cursor datagram");
            }
            let message: InputMessage = serde_cbor::from_slice(&plain)?;
            message.validate()?;
            inbound.send(Inbound {
                peer: sender,
                message,
            })?;
            Ok(None)
        }
    }
}

fn queue_pending(
    pending: &mut HashMap<String, VecDeque<InputMessage>>,
    peer: String,
    message: InputMessage,
) {
    let queue = pending.entry(peer).or_default();
    if queue.len() == MAX_PENDING_MESSAGES {
        queue.pop_front();
    }
    queue.push_back(message);
}

async fn flush_pending(
    socket: &UdpSocket,
    local_id: &str,
    sessions: &mut HashMap<String, Session>,
    pending: &mut HashMap<String, VecDeque<InputMessage>>,
    peer: &str,
) -> Result<()> {
    let Some(mut queue) = pending.remove(peer) else {
        return Ok(());
    };
    let Some(session) = sessions.get_mut(peer) else {
        pending.insert(peer.to_owned(), queue);
        return Ok(());
    };
    while let Some(message) = queue.pop_front() {
        send_data(socket, local_id, session, message).await?;
    }
    Ok(())
}

fn accept_sequence(highest: &mut u64, window: &mut u64, sequence: u64) -> bool {
    if sequence == 0 {
        return false;
    }
    if sequence > *highest {
        let shift = sequence - *highest;
        *window = if shift >= 64 {
            1
        } else {
            (*window << shift) | 1
        };
        *highest = sequence;
        return true;
    }
    let age = *highest - sequence;
    if age >= 64 || (*window & (1 << age)) != 0 {
        return false;
    }
    *window |= 1 << age;
    true
}

#[allow(clippy::too_many_arguments)]
async fn send_hello(
    socket: &UdpSocket,
    cfg: &Config,
    local_id: &str,
    peer: &str,
    addr: SocketAddr,
    ephemeral: [u8; 32],
    nonce: [u8; 16],
    response: bool,
) -> Result<()> {
    let shared = static_shared(cfg, peer)?;
    let wire = Wire::Hello {
        version: WIRE_VERSION,
        sender: local_id.to_owned(),
        ephemeral,
        nonce,
        response,
        tag: hello_tag(&shared, local_id, &ephemeral, &nonce, response),
    };
    socket.send_to(&serde_cbor::to_vec(&wire)?, addr).await?;
    Ok(())
}

async fn send_data(
    socket: &UdpSocket,
    local_id: &str,
    session: &mut Session,
    message: InputMessage,
) -> Result<()> {
    session.send_sequence = session
        .send_sequence
        .checked_add(1)
        .context("cursor sequence exhausted")?;
    let sequence = session.send_sequence;
    let ciphertext = encrypt(
        &session.send_key,
        local_id,
        sequence,
        &serde_cbor::to_vec(&message)?,
    )?;
    let bytes = serde_cbor::to_vec(&Wire::Data {
        version: WIRE_VERSION,
        sender: local_id.to_owned(),
        sequence,
        ciphertext,
    })?;
    let copies = if matches!(
        message,
        InputMessage::Enter { .. }
            | InputMessage::Ack
            | InputMessage::Leave
            | InputMessage::Pointer(PointerInput::Button { .. })
    ) {
        3
    } else {
        1
    };
    for _ in 0..copies {
        socket.send_to(&bytes, session.addr).await?;
    }
    Ok(())
}

fn static_shared(cfg: &Config, peer_id: &str) -> Result<[u8; 32]> {
    let peer = cfg
        .peers
        .get(peer_id)
        .context("cursor peer is not paired")?;
    let remote: [u8; 32] = hex::decode(&peer.public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("peer key must be 32 bytes"))?;
    Ok(x25519(cfg.private_key_bytes()?, remote))
}

fn hello_tag(
    shared: &[u8; 32],
    sender: &str,
    ephemeral: &[u8; 32],
    nonce: &[u8; 16],
    response: bool,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_keyed(shared);
    hash.update(b"lan-cat-input-hello-v1");
    hash.update(sender.as_bytes());
    hash.update(ephemeral);
    hash.update(nonce);
    hash.update(&[u8::from(response)]);
    *hash.finalize().as_bytes()
}

fn session_keys(
    static_shared: &[u8; 32],
    ephemeral_shared: [u8; 32],
    local_id: &str,
    peer_id: &str,
    local_nonce: &[u8; 16],
    peer_nonce: &[u8; 16],
) -> ([u8; 32], [u8; 32]) {
    let local_first = local_id < peer_id;
    let (first_id, second_id, first_nonce, second_nonce) = if local_first {
        (local_id, peer_id, local_nonce, peer_nonce)
    } else {
        (peer_id, local_id, peer_nonce, local_nonce)
    };
    let mut base = blake3::Hasher::new_keyed(static_shared);
    base.update(b"lan-cat-input-session-v1");
    base.update(&ephemeral_shared);
    base.update(first_id.as_bytes());
    base.update(second_id.as_bytes());
    base.update(first_nonce);
    base.update(second_nonce);
    let base = *base.finalize().as_bytes();
    (
        direction_key(&base, local_id, peer_id),
        direction_key(&base, peer_id, local_id),
    )
}

fn direction_key(base: &[u8; 32], sender: &str, receiver: &str) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_keyed(base);
    hash.update(b"direction");
    hash.update(sender.as_bytes());
    hash.update(receiver.as_bytes());
    *hash.finalize().as_bytes()
}

fn encrypt(key: &[u8; 32], sender: &str, sequence: u64, plain: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = packet_nonce(sequence);
    cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plain,
                aad: &packet_aad(sender, sequence),
            },
        )
        .map_err(|_| anyhow::anyhow!("cursor encryption failed"))
}

fn decrypt(key: &[u8; 32], sender: &str, sequence: u64, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = packet_nonce(sequence);
    cipher
        .decrypt(
            (&nonce).into(),
            Payload {
                msg: ciphertext,
                aad: &packet_aad(sender, sequence),
            },
        )
        .map_err(|_| anyhow::anyhow!("cursor authentication failed"))
}

fn packet_nonce(sequence: u64) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[4..].copy_from_slice(&sequence.to_be_bytes());
    nonce
}

fn packet_aad(sender: &str, sequence: u64) -> Vec<u8> {
    let mut aad = b"lan-cat-input-data-v1".to_vec();
    aad.extend_from_slice(sender.as_bytes());
    aad.extend_from_slice(&sequence.to_be_bytes());
    aad
}

fn is_cursor_peer(cfg: &Config, peer: &str) -> bool {
    cfg.cursor.enabled && cfg.peers.contains_key(peer)
}

fn publish(id: &str, port: u16) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let hostname = format!("{}.local.", &id[..id.len().min(32)]);
    let properties = [("id", id), ("v", "1")];
    let info =
        ServiceInfo::new(SERVICE, id, &hostname, "", port, &properties[..])?.enable_addr_auto();
    daemon.register(info)?;
    Ok(daemon)
}

fn resolved_peer(info: &mdns_sd::ResolvedService) -> Option<(String, SocketAddr)> {
    if info.get_property_val_str("v") != Some("1") {
        return None;
    }
    let id = info.get_property_val_str("id")?.to_owned();
    let ip = info
        .get_addresses()
        .iter()
        .map(|address| address.to_ip_addr())
        .find(|ip| matches!(ip, IpAddr::V4(_)))
        .or_else(|| {
            info.get_addresses()
                .iter()
                .map(|address| address.to_ip_addr())
                .next()
        })?;
    Some((id, SocketAddr::new(ip, info.get_port())))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        config::{CursorConfig, Peer},
        ordering::VersionVector,
    };

    fn cursor_config() -> Config {
        Config {
            version: 1,
            name: "local".to_owned(),
            private_key: "00".repeat(32),
            public_key: "11".repeat(32),
            paused: false,
            peers: BTreeMap::from([(
                "peer".to_owned(),
                Peer {
                    name: "peer".to_owned(),
                    public_key: "22".repeat(32),
                },
            )]),
            cursor: CursorConfig { enabled: true },
            clock: VersionVector::default(),
        }
    }

    #[test]
    fn replay_window_accepts_reordered_packets_once() {
        let (mut highest, mut window) = (0, 0);
        assert!(accept_sequence(&mut highest, &mut window, 10));
        assert!(accept_sequence(&mut highest, &mut window, 8));
        assert!(!accept_sequence(&mut highest, &mut window, 8));
        assert!(!accept_sequence(&mut highest, &mut window, 0));
        assert!(accept_sequence(&mut highest, &mut window, 75));
        assert!(!accept_sequence(&mut highest, &mut window, 10));
    }

    #[test]
    fn directional_keys_match_opposite_ends() {
        let shared = [7; 32];
        let ephemeral = [9; 32];
        let nonce_a = [1; 16];
        let nonce_b = [2; 16];
        let (a_send, a_receive) = session_keys(&shared, ephemeral, "a", "b", &nonce_a, &nonce_b);
        let (b_send, b_receive) = session_keys(&shared, ephemeral, "b", "a", &nonce_b, &nonce_a);
        assert_eq!(a_send, b_receive);
        assert_eq!(a_receive, b_send);
    }

    #[test]
    fn encrypted_packet_authenticates_sender_and_sequence() {
        let key = [4; 32];
        let encrypted = encrypt(&key, "peer-a", 8, b"motion").unwrap();
        assert_eq!(decrypt(&key, "peer-a", 8, &encrypted).unwrap(), b"motion");
        assert!(decrypt(&key, "peer-a", 9, &encrypted).is_err());
    }

    #[test]
    fn unpaired_peer_loses_cursor_trust_immediately() {
        let mut cfg = cursor_config();
        assert!(is_cursor_peer(&cfg, "peer"));

        cfg.peers.remove("peer");
        assert!(!is_cursor_peer(&cfg, "peer"));
    }
}
