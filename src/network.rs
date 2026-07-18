use std::{
    collections::HashMap,
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use snow::{HandshakeState, TransportState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

use crate::{
    config::{Config, Peer},
    protocol::Message,
};

const PAIR_SERVICE: &str = "_lan-cat-pair._tcp.local.";
const SYNC_SERVICE: &str = "_lan-cat._tcp.local.";
const MAX_NOISE_PLAINTEXT: usize = 60_000;
const CHUNK_HEADER: usize = 16;
static GROUP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize, Deserialize)]
struct Identity {
    version: u16,
    id: String,
    name: String,
}

pub fn device_id(public_key: &[u8]) -> String {
    hex::encode(&blake3::hash(public_key).as_bytes()[..16])
}

pub async fn pair_interactive() -> Result<()> {
    if crate::ipc::daemon_available().await {
        bail!("daemon is running; stop it on both devices before pairing");
    }
    let mut cfg = Config::load_or_create()?;
    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let port = listener.local_addr()?.port();
    let local_id = device_id(&cfg.public_key()?);
    let mdns = publish(
        PAIR_SERVICE,
        &local_id,
        port,
        &[("id", &local_id), ("v", "1")],
    )?;
    let browse = mdns.browse(PAIR_SERVICE)?;
    println!("Pairing window open. Run `lan-cat pair` on other device.");

    let deadline = tokio::time::sleep(std::time::Duration::from_secs(120));
    tokio::pin!(deadline);
    let result = loop {
        tokio::select! {
            _ = &mut deadline => bail!("pairing timed out"),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                break tokio::time::timeout(
                    std::time::Duration::from_secs(60), pair_stream(stream, false, &cfg)
                ).await.context("pairing handshake timed out")?;
            }
            event = browse.recv_async() => {
                let event = event?;
                let ServiceEvent::ServiceResolved(info) = event else { continue };
                let Some(remote_id) = info.get_property_val_str("id") else { continue };
                if remote_id == local_id || local_id.as_str() > remote_id { continue; }
                let Some(addr) = service_addr(&info) else { continue };
                match TcpStream::connect(addr).await {
                    Ok(stream) => break tokio::time::timeout(
                        std::time::Duration::from_secs(60), pair_stream(stream, true, &cfg)
                    ).await.context("pairing handshake timed out")?,
                    Err(error) => tracing::debug!(%error, "pair connect failed"),
                }
            }
        }
    };
    let _ = mdns.shutdown();
    let (remote_id, peer) = result?;
    cfg.peers.insert(remote_id.clone(), peer);
    cfg.save()?;
    println!("Paired {remote_id}.");
    Ok(())
}

async fn pair_stream(stream: TcpStream, initiator: bool, cfg: &Config) -> Result<(String, Peer)> {
    let private = cfg.private_key_bytes()?;
    let params = "Noise_XX_25519_ChaChaPoly_BLAKE2s".parse()?;
    let builder = snow::Builder::new(params).local_private_key(&private)?;
    let mut hs = if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    };
    let identity = Identity {
        version: 1,
        id: device_id(&cfg.public_key()?),
        name: cfg.name.clone(),
    };
    let payload = serde_cbor::to_vec(&identity)?;
    let mut stream = stream;
    let remote: Identity;
    if initiator {
        noise_write_handshake(&mut stream, &mut hs, &[]).await?;
        let data = noise_read_handshake(&mut stream, &mut hs).await?;
        remote = serde_cbor::from_slice(&data)?;
        noise_write_handshake(&mut stream, &mut hs, &payload).await?;
    } else {
        let _ = noise_read_handshake(&mut stream, &mut hs).await?;
        noise_write_handshake(&mut stream, &mut hs, &payload).await?;
        let data = noise_read_handshake(&mut stream, &mut hs).await?;
        remote = serde_cbor::from_slice(&data)?;
    }
    validate_identity(&remote)?;
    let remote_key = hs
        .get_remote_static()
        .context("pairing peer omitted static key")?
        .to_vec();
    if device_id(&remote_key) != remote.id {
        bail!("pairing identity mismatch");
    }
    let code = pairing_code(hs.get_handshake_hash());
    println!("Peer: {} ({})", remote.name, remote.id);
    println!("Authentication code: {code:06}");
    let confirmed = tokio::task::spawn_blocking(|| {
        print!("Does code match on both devices? [y/N] ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok::<bool, io::Error>(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
    })
    .await??;

    let mut transport = hs.into_transport_mode()?;
    noise_write_record(&mut stream, &mut transport, &[u8::from(confirmed)]).await?;
    let remote_confirmation = noise_read_record(&mut stream, &mut transport).await?;
    if !confirmed || remote_confirmation.as_slice() != [1] {
        bail!("pairing rejected on one or both devices");
    }
    Ok((
        remote.id,
        Peer {
            name: remote.name,
            public_key: hex::encode(remote_key),
        },
    ))
}

fn validate_identity(identity: &Identity) -> Result<()> {
    if identity.version != 1 {
        bail!("unsupported pairing protocol {}", identity.version);
    }
    if identity.name.is_empty() || identity.name.len() > 63 {
        bail!("invalid peer name");
    }
    Ok(())
}

fn pairing_code(hash: &[u8]) -> u32 {
    u32::from_be_bytes(
        hash[..4]
            .try_into()
            .expect("Noise hash is at least 4 bytes"),
    ) % 1_000_000
}

pub struct Discovery {
    daemon: ServiceDaemon,
    pub receiver: mdns_sd::Receiver<ServiceEvent>,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

pub fn start_discovery(id: &str, port: u16) -> Result<Discovery> {
    let daemon = publish(SYNC_SERVICE, id, port, &[("id", id), ("v", "1")])?;
    let receiver = daemon.browse(SYNC_SERVICE)?;
    Ok(Discovery { daemon, receiver })
}

fn publish(
    service: &str,
    instance: &str,
    port: u16,
    properties: &[(&str, &str)],
) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let hostname = format!("{}.local.", &instance[..instance.len().min(32)]);
    let props: HashMap<String, String> = properties
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let info = ServiceInfo::new(service, instance, &hostname, "", port, props)?.enable_addr_auto();
    daemon.register(info)?;
    Ok(daemon)
}

pub fn resolved_peer(info: &mdns_sd::ResolvedService) -> Option<(String, SocketAddr)> {
    if info.get_property_val_str("v") != Some("1") {
        return None;
    }
    let id = info.get_property_val_str("id")?.to_owned();
    Some((id, service_addr(info)?))
}

fn service_addr(info: &mdns_sd::ResolvedService) -> Option<SocketAddr> {
    let ip = info
        .get_addresses()
        .iter()
        .map(|a| a.to_ip_addr())
        .find(|ip| matches!(ip, IpAddr::V4(_)))
        .or_else(|| info.get_addresses().iter().map(|a| a.to_ip_addr()).next())?;
    Some(SocketAddr::new(ip, info.get_port()))
}

pub struct SecureConnection {
    read: Mutex<tokio::net::tcp::OwnedReadHalf>,
    write: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    state: Arc<Mutex<TransportState>>,
}

impl SecureConnection {
    pub async fn connect(addr: SocketAddr, cfg: &Config, peer: &Peer) -> Result<Self> {
        let mut stream = TcpStream::connect(addr).await?;
        let id = device_id(&cfg.public_key()?);
        stream.write_all(id.as_bytes()).await?;
        Self::operational(stream, true, cfg, peer).await
    }

    pub async fn accept(stream: TcpStream, cfg: &Config, peer: &Peer) -> Result<Self> {
        Self::operational(stream, false, cfg, peer).await
    }

    async fn operational(
        mut stream: TcpStream,
        initiator: bool,
        cfg: &Config,
        peer: &Peer,
    ) -> Result<Self> {
        let private = cfg.private_key_bytes()?;
        let remote = hex::decode(&peer.public_key)?;
        let params = "Noise_KK_25519_ChaChaPoly_BLAKE2s".parse()?;
        let builder = snow::Builder::new(params)
            .local_private_key(&private)?
            .remote_public_key(&remote)?;
        let mut hs = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        if initiator {
            noise_write_handshake(&mut stream, &mut hs, &[]).await?;
            let _ = noise_read_handshake(&mut stream, &mut hs).await?;
        } else {
            let _ = noise_read_handshake(&mut stream, &mut hs).await?;
            noise_write_handshake(&mut stream, &mut hs, &[]).await?;
        }
        let state = Arc::new(Mutex::new(hs.into_transport_mode()?));
        let (read, write) = stream.into_split();
        Ok(Self {
            read: Mutex::new(read),
            write: Mutex::new(write),
            state,
        })
    }

    pub async fn send(&self, message: &Message) -> Result<()> {
        let bytes = serde_cbor::to_vec(message)?;
        let group = GROUP_ID.fetch_add(1, Ordering::Relaxed);
        let payload_size = MAX_NOISE_PLAINTEXT - CHUNK_HEADER;
        let total = bytes.len().div_ceil(payload_size) as u32;
        let mut write = self.write.lock().await;
        for (index, chunk) in bytes.chunks(payload_size).enumerate() {
            let mut plain = Vec::with_capacity(CHUNK_HEADER + chunk.len());
            plain.extend_from_slice(&group.to_be_bytes());
            plain.extend_from_slice(&(index as u32).to_be_bytes());
            plain.extend_from_slice(&total.to_be_bytes());
            plain.extend_from_slice(chunk);
            let encrypted = {
                let mut state = self.state.lock().await;
                encrypt_record(&mut state, &plain)?
            };
            write.write_u32(encrypted.len() as u32).await?;
            write.write_all(&encrypted).await?;
        }
        Ok(())
    }

    pub async fn receive(&self) -> Result<Message> {
        let mut read = self.read.lock().await;
        let mut all = Vec::new();
        let mut wanted_group = None;
        for index in 0_u32.. {
            let len = read.read_u32().await? as usize;
            if len > MAX_NOISE_PLAINTEXT + 32 {
                bail!("encrypted record too large");
            }
            let mut encrypted = vec![0; len];
            read.read_exact(&mut encrypted).await?;
            let plain = {
                let mut state = self.state.lock().await;
                decrypt_record(&mut state, &encrypted)?
            };
            if plain.len() < CHUNK_HEADER {
                bail!("short application chunk");
            }
            let group = u64::from_be_bytes(plain[..8].try_into()?);
            let got_index = u32::from_be_bytes(plain[8..12].try_into()?);
            let total = u32::from_be_bytes(plain[12..16].try_into()?);
            if wanted_group.get_or_insert(group) != &group
                || got_index != index
                || total == 0
                || total > 512
            {
                bail!("invalid application chunk sequence");
            }
            all.extend_from_slice(&plain[CHUNK_HEADER..]);
            if all.len() > crate::protocol::MAX_PAYLOAD_BYTES + 256 * 1024 {
                bail!("application message too large");
            }
            if index + 1 == total {
                break;
            }
        }
        Ok(serde_cbor::from_slice(&all)?)
    }
}

pub async fn read_peer_preface(stream: &mut TcpStream) -> Result<String> {
    let mut id = [0_u8; 32];
    stream.read_exact(&mut id).await?;
    let id = std::str::from_utf8(&id)?;
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid peer preface");
    }
    Ok(id.to_ascii_lowercase())
}

async fn noise_write_handshake(
    stream: &mut TcpStream,
    state: &mut HandshakeState,
    payload: &[u8],
) -> Result<()> {
    let mut out = vec![0; payload.len() + 256];
    let len = state.write_message(payload, &mut out)?;
    stream.write_u16(len.try_into()?).await?;
    stream.write_all(&out[..len]).await?;
    Ok(())
}

async fn noise_read_handshake(
    stream: &mut TcpStream,
    state: &mut HandshakeState,
) -> Result<Vec<u8>> {
    let len = stream.read_u16().await? as usize;
    if len > 4096 {
        bail!("handshake message too large");
    }
    let mut msg = vec![0; len];
    stream.read_exact(&mut msg).await?;
    let mut out = vec![0; 4096];
    let len = state.read_message(&msg, &mut out)?;
    out.truncate(len);
    Ok(out)
}

async fn noise_write_record(
    stream: &mut TcpStream,
    state: &mut TransportState,
    plain: &[u8],
) -> Result<()> {
    let out = encrypt_record(state, plain)?;
    stream.write_u32(out.len() as u32).await?;
    stream.write_all(&out).await?;
    Ok(())
}

async fn noise_read_record(stream: &mut TcpStream, state: &mut TransportState) -> Result<Vec<u8>> {
    let len = stream.read_u32().await? as usize;
    if len > MAX_NOISE_PLAINTEXT + 32 {
        bail!("encrypted record too large");
    }
    let mut input = vec![0; len];
    stream.read_exact(&mut input).await?;
    decrypt_record(state, &input)
}

fn encrypt_record(state: &mut TransportState, plain: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0; plain.len() + 32];
    let len = state.write_message(plain, &mut out)?;
    out.truncate(len);
    Ok(out)
}

fn decrypt_record(state: &mut TransportState, encrypted: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0; encrypted.len()];
    let len = state.read_message(encrypted, &mut out)?;
    out.truncate(len);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ordering::VersionVector,
        protocol::{ClipboardEvent, ClipboardPayload},
    };

    fn test_config(name: &str) -> Config {
        let params = "Noise_NN_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
        let key = snow::Builder::new(params).generate_keypair().unwrap();
        Config {
            version: 1,
            name: name.into(),
            private_key: hex::encode(key.private),
            public_key: hex::encode(key.public),
            paused: false,
            peers: Default::default(),
            cursor: Default::default(),
            clock: VersionVector::default(),
        }
    }

    #[test]
    fn stable_device_id_and_code() {
        assert_eq!(device_id(&[7; 32]), device_id(&[7; 32]));
        assert!(pairing_code(&[0xff; 32]) < 1_000_000);
    }

    #[tokio::test]
    async fn authenticated_connection_fragments_large_message() {
        let left = test_config("left");
        let right = test_config("right");
        let left_id = device_id(&left.public_key().unwrap());
        let right_id = device_id(&right.public_key().unwrap());
        let left_peer = Peer {
            name: left.name.clone(),
            public_key: left.public_key.clone(),
        };
        let right_peer = Peer {
            name: right.name.clone(),
            public_key: right.public_key.clone(),
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_right = right.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(read_peer_preface(&mut stream).await.unwrap(), left_id);
            let connection = SecureConnection::accept(stream, &server_right, &left_peer)
                .await
                .unwrap();
            connection.receive().await.unwrap()
        });
        let connection = SecureConnection::connect(addr, &left, &right_peer)
            .await
            .unwrap();
        let mut clock = VersionVector::default();
        let sequence = clock.increment(&right_id);
        let text = "x".repeat(crate::protocol::MAX_PAYLOAD_BYTES);
        let event =
            ClipboardEvent::new(right_id, sequence, clock, ClipboardPayload::text(text)).unwrap();
        connection.send(&Message::Clipboard(event)).await.unwrap();
        let Message::Clipboard(received) = server.await.unwrap() else {
            panic!("wrong message")
        };
        received.validate().unwrap();
        assert_eq!(
            received.payload.text.unwrap().len(),
            crate::protocol::MAX_PAYLOAD_BYTES
        );
    }
}
