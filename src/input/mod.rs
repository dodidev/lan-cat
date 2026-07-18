pub mod beacon;
mod platform;
pub mod protocol;
mod transport;

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::config::Config;
use beacon::Beacon;
use platform::{Capture, CaptureEvent, Injector};
use protocol::{Edge, InputMessage};
use transport::{Inbound, Outbound};

const CONFIRM_TIME: Duration = Duration::from_secs(3);
const PRESS_GAP: Duration = Duration::from_millis(280);
const TAKEOVER_REPEAT_TIME: Duration = Duration::from_millis(700);
const TAKEOVER_REPEAT_GAP: Duration = Duration::from_millis(70);

enum Outgoing {
    Probing {
        peer: String,
        edge: Edge,
        position: f64,
        started: Instant,
        last_push: Instant,
        acknowledged: bool,
    },
    Sending {
        peer: String,
        ready: bool,
    },
}

struct Incoming {
    peer: String,
    edge: Edge,
}

struct Preview {
    peer: String,
    edge: Edge,
    last_probe: Instant,
    progress: f32,
    beacon: Beacon,
}

pub fn spawn(cfg: Config, local_id: String) -> Result<()> {
    std::thread::Builder::new()
        .name("lan-cat-input".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("input runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                if let Err(error) = run(cfg, local_id).await {
                    tracing::error!(%error, "cursor service stopped");
                }
            });
        })?;
    Ok(())
}

async fn run(cfg: Config, local_id: String) -> Result<()> {
    let mut capture = Capture::new().await?;
    let mut injector = Injector::new().await?;
    let (outbound, mut inbound) = transport::start(&cfg, &local_id).await?;
    let mut outgoing: Option<Outgoing> = None;
    let mut incoming: Option<Incoming> = None;
    let mut preview: Option<Preview> = None;
    let mut takeover: Option<(String, Instant, Instant)> = None;
    let mut confirmed_peers = HashSet::new();
    let mut last_seen: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_ping = Instant::now() - Duration::from_secs(1);

    loop {
        tokio::select! {
            event = capture.events.recv() => {
                let Some(event) = event else { anyhow::bail!("cursor capture stopped") };
                match event {
                    CaptureEvent::Begin { edge, position } => {
                        if incoming.as_ref().is_some_and(|active| active.edge == edge) {
                            let active = incoming.take().expect("incoming cursor");
                            injector.end()?;
                            send(&outbound, active.peer, InputMessage::Leave)?;
                            capture.release();
                            continue;
                        }
                        if outgoing.is_some() || incoming.is_some() {
                            continue;
                        }
                        let Some(peer) = select_peer(&cfg, &last_seen, position) else {
                            capture.release();
                            continue;
                        };
                        if confirmed_peers.contains(&peer) {
                            send(
                                &outbound,
                                peer.clone(),
                                InputMessage::Enter { edge: edge.opposite(), position },
                            )?;
                            outgoing = Some(Outgoing::Sending { peer, ready: false });
                            continue;
                        }
                        let now = Instant::now();
                        outgoing = Some(Outgoing::Probing {
                            peer: peer.clone(),
                            edge,
                            position,
                            started: now,
                            last_push: now,
                            acknowledged: false,
                        });
                        send_probe(&outbound, peer, edge, position, 0.0)?;
                    }
                    CaptureEvent::Pointer(pointer) => match &mut outgoing {
                        Some(Outgoing::Probing { peer, edge, position, started, last_push, .. }) => {
                            if let Some((outward, along)) = pressure(*edge, pointer) {
                                *position = (*position + along).clamp(0.0, 1.0);
                                if outward > 0.0 {
                                    *last_push = Instant::now();
                                    let progress = (started.elapsed().as_secs_f32() / CONFIRM_TIME.as_secs_f32()).min(1.0);
                                    send_probe(&outbound, peer.clone(), *edge, *position, progress)?;
                                } else if outward < -1.5 {
                                    send(&outbound, peer.clone(), InputMessage::Cancel)?;
                                    outgoing = None;
                                    capture.release();
                                }
                            }
                        }
                        Some(Outgoing::Sending { peer, ready: true }) => {
                            send(&outbound, peer.clone(), InputMessage::Pointer(pointer))?;
                        }
                        _ => {}
                    },
                    CaptureEvent::Keyboard(keyboard) => {
                        if let Some(Outgoing::Sending { peer, ready: true }) = &outgoing {
                            send(&outbound, peer.clone(), InputMessage::Keyboard(keyboard))?;
                        }
                    }
                    CaptureEvent::LocalInput => {
                        if let Some(active) = incoming.take() {
                            injector.end()?;
                            capture.release();
                            takeover = Some((active.peer.clone(), Instant::now(), Instant::now()));
                            send_takeover(&outbound, active.peer)?;
                        }
                    }
                }
            }
            event = inbound.recv() => {
                let Some(Inbound { peer, message }) = event else { anyhow::bail!("cursor transport stopped") };
                    message.validate()?;
                    last_seen.insert(peer.clone(), Instant::now());
                    match message {
                        InputMessage::Probe { edge, position, progress } => {
                            if incoming.is_some() {
                                send(&outbound, peer, InputMessage::Cancel)?;
                                continue;
                            }
                            // Confirmed peers may enter directly; do not replay the portal UI.
                            if confirmed_peers.contains(&peer) {
                                send(&outbound, peer, InputMessage::ProbeAck)?;
                                continue;
                            }
                            let replace = preview.as_ref().is_none_or(|value| value.peer != peer || value.edge != edge);
                        if replace {
                            if let Some(mut old) = preview.take() { old.beacon.cancel(); }
                            preview = Some(Preview {
                                peer: peer.clone(),
                                edge,
                                last_probe: Instant::now(),
                                progress,
                                beacon: Beacon::show(edge, position, &peer)?,
                            });
                        }
                        if let Some(value) = &mut preview {
                            value.last_probe = Instant::now();
                            value.progress = progress;
                            value.beacon.update(position, progress, false);
                        }
                        send(&outbound, peer, InputMessage::ProbeAck)?;
                    }
                    InputMessage::ProbeAck => {
                        if let Some(Outgoing::Probing { peer: active, acknowledged, .. }) = &mut outgoing {
                            if *active == peer { *acknowledged = true; }
                        }
                    }
                    InputMessage::Cancel => {
                        if outgoing.as_ref().is_some_and(|value| outgoing_peer(value) == peer) {
                            outgoing = None;
                            capture.release();
                        }
                        if preview.as_ref().is_some_and(|value| value.peer == peer) {
                            if let Some(mut value) = preview.take() { value.beacon.cancel(); }
                        }
                    }
                    InputMessage::Enter { edge, position } => {
                        if takeover.as_ref().is_some_and(|(active, started, _)| {
                            *active == peer && started.elapsed() <= TAKEOVER_REPEAT_TIME
                        }) {
                            send_takeover(&outbound, peer)?;
                            continue;
                        }
                        let preview_matches = preview.as_ref().is_some_and(|value| {
                            value.peer == peer
                                && value.edge == edge
                                && value.progress >= 0.9
                                && value.last_probe.elapsed() <= PRESS_GAP
                        });
                        let outgoing_wins = outgoing.is_some() && local_id > peer;
                        if (!preview_matches && !confirmed_peers.contains(&peer))
                            || incoming.is_some()
                            || outgoing_wins
                        {
                            send(&outbound, peer, InputMessage::Cancel)?;
                            continue;
                        }
                        if let Some(mut value) = preview.take() { value.beacon.confirm(); }
                        if outgoing.is_some() {
                            outgoing = None;
                            capture.release();
                        }
                        injector.begin(edge, position)?;
                        confirmed_peers.insert(peer.clone());
                        incoming = Some(Incoming { peer: peer.clone(), edge });
                        send(&outbound, peer, InputMessage::Ack)?;
                    }
                    InputMessage::Ack => {
                        if let Some(Outgoing::Sending { peer: active, ready }) = &mut outgoing {
                            if *active == peer {
                                *ready = true;
                                confirmed_peers.insert(peer);
                            }
                        }
                    }
                    InputMessage::Leave => {
                        if outgoing.as_ref().is_some_and(|value| outgoing_peer(value) == peer) {
                            outgoing = None;
                            capture.release();
                        }
                        if incoming.as_ref().is_some_and(|value| value.peer == peer) {
                            injector.end()?;
                            incoming = None;
                        }
                    }
                    InputMessage::Pointer(pointer) => {
                        let takeover_active = takeover.as_ref().is_some_and(|(active, started, _)| {
                            *active == peer && started.elapsed() <= TAKEOVER_REPEAT_TIME
                        });
                        if takeover_active {
                            send_takeover(&outbound, peer)?;
                        } else if incoming.as_ref().is_some_and(|value| value.peer == peer) {
                            injector.apply(pointer)?;
                        }
                    }
                    InputMessage::Keyboard(keyboard) => {
                        let takeover_active = takeover.as_ref().is_some_and(|(active, started, _)| {
                            *active == peer && started.elapsed() <= TAKEOVER_REPEAT_TIME
                        });
                        if takeover_active {
                            send_takeover(&outbound, peer)?;
                        } else if incoming.as_ref().is_some_and(|value| value.peer == peer) {
                            injector.apply_keyboard(keyboard)?;
                        }
                    }
                    InputMessage::Ping => send(&outbound, peer, InputMessage::Pong)?,
                    InputMessage::Pong => {}
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                if last_ping.elapsed() >= Duration::from_millis(500) {
                    for peer in cfg.peers.keys() {
                        send(&outbound, peer.clone(), InputMessage::Ping)?;
                    }
                    last_ping = now;
                }
                if let Some((peer, started, last_sent)) = &mut takeover {
                    if started.elapsed() > TAKEOVER_REPEAT_TIME {
                        takeover = None;
                    } else if last_sent.elapsed() >= TAKEOVER_REPEAT_GAP {
                        send_takeover(&outbound, peer.clone())?;
                        *last_sent = Instant::now();
                    }
                }
                let mut confirm = None;
                let mut cancel = None;
                if let Some(Outgoing::Probing { peer, edge, position, started, last_push, acknowledged }) = &outgoing {
                    if last_push.elapsed() > PRESS_GAP {
                        cancel = Some(peer.clone());
                    } else if *acknowledged && started.elapsed() >= CONFIRM_TIME {
                        confirm = Some((peer.clone(), edge.opposite(), *position));
                    }
                }
                if let Some(peer) = cancel {
                    send(&outbound, peer, InputMessage::Cancel)?;
                    outgoing = None;
                    capture.release();
                }
                if let Some((peer, edge, position)) = confirm {
                    send(&outbound, peer.clone(), InputMessage::Enter { edge, position })?;
                    outgoing = Some(Outgoing::Sending { peer, ready: false });
                }
                if preview.as_ref().is_some_and(|value| value.last_probe.elapsed() > PRESS_GAP) {
                    if let Some(mut value) = preview.take() { value.beacon.cancel(); }
                }
                if let Some(value) = outgoing.as_ref() {
                    let peer = outgoing_peer(value);
                    if last_seen.get(peer).is_none_or(|seen| seen.elapsed() > Duration::from_secs(2)) {
                        outgoing = None;
                        capture.release();
                    }
                }
                if incoming.as_ref().is_some_and(|value| {
                    last_seen.get(&value.peer).is_none_or(|seen| seen.elapsed() > Duration::from_secs(2))
                }) {
                    injector.end()?;
                    incoming = None;
                }
            }
        }
    }
}

fn select_peer(cfg: &Config, online: &HashMap<String, Instant>, position: f64) -> Option<String> {
    let mut peers: Vec<_> = cfg
        .peers
        .keys()
        .filter(|peer| {
            online
                .get(*peer)
                .is_some_and(|seen| seen.elapsed() < Duration::from_secs(2))
        })
        .cloned()
        .collect();
    peers.sort();
    if peers.is_empty() {
        return None;
    }
    let index =
        ((position.clamp(0.0, 0.999_999) * peers.len() as f64) as usize).min(peers.len() - 1);
    peers.get(index).cloned()
}

fn pressure(edge: Edge, pointer: protocol::PointerInput) -> Option<(f64, f64)> {
    let protocol::PointerInput::Motion { dx, dy } = pointer else {
        return None;
    };
    Some(match edge {
        Edge::Left => (-dx, dy / 1000.0),
        Edge::Right => (dx, dy / 1000.0),
        Edge::Top => (-dy, dx / 1000.0),
        Edge::Bottom => (dy, dx / 1000.0),
    })
}

fn send_probe(
    sender: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    peer: String,
    source_edge: Edge,
    position: f64,
    progress: f32,
) -> Result<()> {
    send(
        sender,
        peer,
        InputMessage::Probe {
            edge: source_edge.opposite(),
            position,
            progress,
        },
    )
}

fn send_takeover(
    sender: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    peer: String,
) -> Result<()> {
    send(sender, peer.clone(), InputMessage::Leave)?;
    send(sender, peer, InputMessage::Cancel)
}

fn outgoing_peer(value: &Outgoing) -> &str {
    match value {
        Outgoing::Probing { peer, .. } | Outgoing::Sending { peer, .. } => peer,
    }
}

fn send(
    sender: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    peer: String,
    message: InputMessage,
) -> Result<()> {
    sender.send(Outbound { peer, message })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::PointerInput;

    #[test]
    fn pressure_maps_every_edge_outward_and_along() {
        assert_eq!(
            pressure(Edge::Left, PointerInput::Motion { dx: -4.0, dy: 20.0 }),
            Some((4.0, 0.02))
        );
        assert_eq!(
            pressure(Edge::Right, PointerInput::Motion { dx: 4.0, dy: 20.0 }),
            Some((4.0, 0.02))
        );
        assert_eq!(
            pressure(Edge::Top, PointerInput::Motion { dx: 20.0, dy: -4.0 }),
            Some((4.0, 0.02))
        );
        assert_eq!(
            pressure(Edge::Bottom, PointerInput::Motion { dx: 20.0, dy: 4.0 }),
            Some((4.0, 0.02))
        );
    }
}
