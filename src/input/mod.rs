pub mod beacon;
mod platform;
pub mod protocol;
mod transport;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use tokio::sync::RwLock;

use crate::config::Config;
use beacon::Beacon;
use platform::{Capture, CaptureEvent, Injector};
use protocol::{Edge, InputMessage, PointerInput};
use transport::{Inbound, Outbound};

const CONFIRM_TIME: Duration = Duration::from_secs(2);
const PRESS_GAP: Duration = Duration::from_millis(500);
const PEER_TIMEOUT: Duration = Duration::from_millis(1_200);
const TAKEOVER_REPEAT_TIME: Duration = Duration::from_millis(700);
const TAKEOVER_REPEAT_GAP: Duration = Duration::from_millis(70);
const REMOTE_ENTRY_INSET: f64 = 3.0;
#[cfg(debug_assertions)]
const DEBUG_ESC_KILL_TIME: Duration = Duration::from_secs(3);
#[cfg(debug_assertions)]
const ESC_KEY: u32 = 1;

enum Outgoing {
    Probing {
        peer: String,
        edge: Edge,
        position: f64,
        started: Instant,
        last_push: Instant,
        acknowledged: bool,
        screen_width: f64,
        screen_height: f64,
    },
    Sending {
        peer: String,
        edge: Edge,
        depth: f64,
        ready: bool,
    },
}

struct Incoming {
    peer: String,
    edge: Edge,
    #[cfg(target_os = "linux")]
    depth: f64,
    peer_screen_width: f64,
    peer_screen_height: f64,
}

struct Preview {
    peer: String,
    edge: Edge,
    last_probe: Instant,
    beacon: Beacon,
}

pub fn spawn(cfg: Arc<RwLock<Config>>, local_id: String) -> Result<()> {
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

async fn run(cfg: Arc<RwLock<Config>>, local_id: String) -> Result<()> {
    let mut capture = Capture::new().await?;
    let mut injector = Injector::new().await?;
    let (outbound, mut inbound) = transport::start(cfg.clone(), &local_id).await?;
    let mut outgoing: Option<Outgoing> = None;
    let mut incoming: Option<Incoming> = None;
    let mut preview: Option<Preview> = None;
    let mut takeover: Option<(String, Instant, Instant)> = None;
    let mut portal_edges: HashMap<String, HashSet<Edge>> = HashMap::new();
    let mut last_seen: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    #[cfg(debug_assertions)]
    let mut escape_started: Option<Instant> = None;

    loop {
        tokio::select! {
            event = capture.events.recv() => {
                let Some(event) = event else {
                    injector.end()?;
                    capture.release();
                    anyhow::bail!("cursor capture stopped")
                };
                match event {
                    CaptureEvent::Begin { edge, position, screen_width, screen_height } => {
                        if incoming.as_ref().is_some_and(|active| active.edge == edge) {
                            let active = incoming.take().expect("incoming cursor");
                            injector.end()?;
                            capture.allow_all_edges();
                            send(&outbound, active.peer, InputMessage::Leave)?;
                            capture.release();
                            continue;
                        }
                        if outgoing.is_some() || incoming.is_some() {
                            continue;
                        }
                        let peer = {
                            let cfg = cfg.read().await;
                            select_peer(
                                &cfg,
                                &last_seen,
                                &portal_edges,
                                edge,
                                position,
                            )
                        };
                        let Some(peer) = peer else {
                            capture.release();
                            continue;
                        };
                        if portal_edge_confirmed(&portal_edges, &peer, edge) {
                            send(
                                &outbound,
                                peer.clone(),
                                InputMessage::Enter {
                                    edge: destination_edge(edge),
                                    position,
                                    screen_width,
                                    screen_height,
                                },
                            )?;
                            outgoing = Some(Outgoing::Sending {
                                peer,
                                edge,
                                depth: REMOTE_ENTRY_INSET,
                                ready: false,
                            });
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
                            screen_width,
                            screen_height,
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
                                    send_force_leave(&outbound, peer.clone())?;
                                    outgoing = None;
                                    capture.release();
                                }
                            }
                        }
                        Some(Outgoing::Sending { peer, edge, depth, ready }) => {
                            if *ready && crosses_host_edge(*edge, pointer, depth) {
                                send_force_leave(&outbound, peer.clone())?;
                                outgoing = None;
                                capture.release();
                            } else if *ready {
                                send(&outbound, peer.clone(), InputMessage::Pointer(pointer))?;
                            }
                        }
                        _ => {}
                    },
                    CaptureEvent::Keyboard(keyboard) => {
                        #[cfg(debug_assertions)]
                        update_debug_escape_kill(keyboard, &mut escape_started);
                        if let Some(Outgoing::Sending { peer, ready: true, .. }) = &outgoing {
                            send(&outbound, peer.clone(), InputMessage::Keyboard(keyboard))?;
                        }
                    }
                    CaptureEvent::LocalInput => {
                        if let Some(active) = incoming.take() {
                            injector.end()?;
                            capture.allow_all_edges();
                            capture.release();
                            takeover = Some((active.peer.clone(), Instant::now(), Instant::now()));
                            send_force_leave(&outbound, active.peer)?;
                        }
                    }
                    CaptureEvent::LocalKeyboard(_keyboard) => {
                        #[cfg(debug_assertions)]
                        update_debug_escape_kill(_keyboard, &mut escape_started);
                    }
                    #[cfg(target_os = "linux")]
                    CaptureEvent::CaptureLost => {
                        if let Some(active) = outgoing.take() {
                            send_force_leave(&outbound, outgoing_peer(&active).to_owned())?;
                        }
                        if let Some(active) = incoming.take() {
                            injector.end()?;
                            send_force_leave(&outbound, active.peer)?;
                        }
                        capture.allow_all_edges();
                        capture.release();
                    }
                }
            }
            event = inbound.recv() => {
                let Some(Inbound { peer, message }) = event else {
                    injector.end()?;
                    capture.release();
                    anyhow::bail!("cursor transport stopped")
                };
                    message.validate()?;
                    last_seen.insert(peer.clone(), Instant::now());
                    match message {
                        InputMessage::Probe { edge, position, progress } => {
                            if incoming.is_some()
                                || !portal_edge_available(&portal_edges, &peer, edge)
                            {
                                send_force_leave(&outbound, peer)?;
                                continue;
                            }
                            // Confirmed peers may enter directly; do not replay the portal UI.
                            if portal_edge_confirmed(&portal_edges, &peer, edge) {
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
                                beacon: Beacon::show(edge, position, &peer)?,
                            });
                        }
                        if let Some(value) = &mut preview {
                            value.last_probe = Instant::now();
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
                    InputMessage::Enter { edge, position, screen_width, screen_height } => {
                        if takeover.as_ref().is_some_and(|(active, started, _)| {
                            *active == peer && started.elapsed() <= TAKEOVER_REPEAT_TIME
                        }) {
                            send_force_leave(&outbound, peer)?;
                            continue;
                        }
                        let preview_matches = preview.as_ref().is_some_and(|value| {
                            value.peer == peer
                                && value.edge == edge
                                && value.last_probe.elapsed() <= PRESS_GAP
                        });
                        let outgoing_wins = outgoing.is_some() && local_id > peer;
                        let confirmed_portal = portal_edge_confirmed(&portal_edges, &peer, edge);
                        let edge_available = portal_edge_available(&portal_edges, &peer, edge);
                        if !edge_available
                            || (!preview_matches && !confirmed_portal)
                            || incoming.is_some()
                            || outgoing_wins
                        {
                            tracing::debug!(
                                %peer,
                                %edge,
                                edge_available,
                                preview_matches,
                                confirmed_portal,
                                incoming_active = incoming.is_some(),
                                outgoing_wins,
                                "cursor entry rejected"
                            );
                            send_force_leave(&outbound, peer)?;
                            continue;
                        }
                        if let Some(mut value) = preview.take() { value.beacon.confirm(); }
                        if outgoing.is_some() {
                            outgoing = None;
                            capture.release();
                        }
                        capture.set_allowed_edge(Some(edge));
                        injector.begin(edge, position)?;
                        portal_edges.entry(peer.clone()).or_default().insert(edge);
                        incoming = Some(Incoming {
                            peer: peer.clone(),
                            edge,
                            #[cfg(target_os = "linux")]
                            depth: REMOTE_ENTRY_INSET,
                            peer_screen_width: screen_width,
                            peer_screen_height: screen_height,
                        });
                        tracing::debug!(%peer, %edge, "cursor entry accepted");
                        send(&outbound, peer, InputMessage::Ack)?;
                    }
                    InputMessage::Ack => {
                        if let Some(Outgoing::Sending {
                            peer: active,
                            edge,
                            ready,
                            ..
                        }) = &mut outgoing {
                            if *active == peer {
                                *ready = true;
                                tracing::debug!(%peer, source_edge = %edge, "cursor entry acknowledged");
                                portal_edges.entry(peer.clone()).or_default().insert(*edge);
                            }
                        }
                    }
                    InputMessage::Leave => {
                        let rejected_direct_edge = match &outgoing {
                            Some(Outgoing::Sending {
                                peer: active,
                                edge,
                                ready: false,
                                ..
                            }) if *active == peer => Some(*edge),
                            _ => None,
                        };
                        if outgoing.as_ref().is_some_and(|value| outgoing_peer(value) == peer) {
                            outgoing = None;
                            if let Some(edge) = rejected_direct_edge {
                                forget_portal_edge(&mut portal_edges, &peer, edge);
                            }
                            capture.release();
                        }
                        if incoming.as_ref().is_some_and(|value| value.peer == peer) {
                            injector.end()?;
                            incoming = None;
                            capture.allow_all_edges();
                        }
                    }
                    InputMessage::Pointer(pointer) => {
                        let takeover_active = takeover.as_ref().is_some_and(|(active, started, _)| {
                            *active == peer && started.elapsed() <= TAKEOVER_REPEAT_TIME
                        });
                        if takeover_active {
                            send_force_leave(&outbound, peer)?;
                        } else if let Some(incoming_state) = &mut incoming {
                            if incoming_state.peer == peer {
                                // Scale pointer motion based on screen dimensions
                                let scaled_pointer = scale_pointer_motion(
                                    pointer,
                                    capture.screen_dimensions(),
                                    (
                                        incoming_state.peer_screen_width,
                                        incoming_state.peer_screen_height,
                                    ),
                                );
                                injector.apply(scaled_pointer)?;
                                #[cfg(target_os = "macos")]
                                let left_remote = injector.left_entry_edge(
                                    incoming_state.edge,
                                    scaled_pointer,
                                );
                                #[cfg(target_os = "linux")]
                                let left_remote = crosses_remote_entry_edge(
                                    incoming_state.edge,
                                    scaled_pointer,
                                    &mut incoming_state.depth,
                                );
                                if left_remote {
                                    injector.end()?;
                                    capture.allow_all_edges();
                                    incoming = None;
                                    send(&outbound, peer, InputMessage::Leave)?;
                                }
                            }
                        }
                    }
                    InputMessage::Keyboard(keyboard) => {
                        #[cfg(debug_assertions)]
                        update_debug_escape_kill(keyboard, &mut escape_started);
                        let takeover_active = takeover.as_ref().is_some_and(|(active, started, _)| {
                            *active == peer && started.elapsed() <= TAKEOVER_REPEAT_TIME
                        });
                        if takeover_active {
                            send_force_leave(&outbound, peer)?;
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
                let trusted_peers: HashSet<_> = cfg.read().await.peers.keys().cloned().collect();
                if outgoing
                    .as_ref()
                    .is_some_and(|value| !trusted_peers.contains(outgoing_peer(value)))
                {
                    outgoing = None;
                    capture.release();
                }
                if incoming
                    .as_ref()
                    .is_some_and(|value| !trusted_peers.contains(&value.peer))
                {
                    incoming = None;
                    injector.end()?;
                    capture.allow_all_edges();
                }
                portal_edges.retain(|peer, _| trusted_peers.contains(peer));
                if last_ping.elapsed() >= Duration::from_millis(500) {
                    for peer in &trusted_peers {
                        send(&outbound, peer.clone(), InputMessage::Ping)?;
                    }
                    last_ping = now;
                }
                #[cfg(debug_assertions)]
                if escape_started.is_some_and(|started| started.elapsed() >= DEBUG_ESC_KILL_TIME) {
                    tracing::warn!("debug escape kill switch triggered");
                    std::process::exit(0);
                }
                if let Some((peer, started, last_sent)) = &mut takeover {
                    if started.elapsed() > TAKEOVER_REPEAT_TIME {
                        takeover = None;
                    } else if last_sent.elapsed() >= TAKEOVER_REPEAT_GAP {
                        send_force_leave(&outbound, peer.clone())?;
                        *last_sent = Instant::now();
                    }
                }
                let mut confirm = None;
                let mut cancel = None;
                if let Some(Outgoing::Probing { peer, edge, position, started, last_push, acknowledged, screen_width, screen_height }) = &outgoing {
                    if last_push.elapsed() > PRESS_GAP {
                        cancel = Some(peer.clone());
                    } else if *acknowledged && started.elapsed() >= CONFIRM_TIME {
                        confirm = Some((peer.clone(), *edge, *position, *screen_width, *screen_height));
                    }
                }
                if let Some(peer) = cancel {
                    send_force_leave(&outbound, peer)?;
                    outgoing = None;
                    capture.release();
                }
                if let Some((peer, edge, position, screen_width, screen_height)) = confirm {
                    tracing::debug!(%peer, source_edge = %edge, destination_edge = %destination_edge(edge), "cursor handshake confirmed");
                    send(
                        &outbound,
                        peer.clone(),
                        InputMessage::Enter {
                            edge: destination_edge(edge),
                            position,
                            screen_width,
                            screen_height,
                        },
                    )?;
                    outgoing = Some(Outgoing::Sending {
                        peer,
                        edge,
                        depth: REMOTE_ENTRY_INSET,
                        ready: false,
                    });
                }
                if preview.as_ref().is_some_and(|value| value.last_probe.elapsed() > PRESS_GAP) {
                    if let Some(mut value) = preview.take() { value.beacon.cancel(); }
                }
                if let Some(value) = outgoing.as_ref() {
                    let peer = outgoing_peer(value);
                    if peer_timed_out(&last_seen, peer) {
                        let peer = peer.to_owned();
                        outgoing = None;
                        portal_edges.remove(&peer);
                        capture.release();
                        takeover = Some((peer.clone(), Instant::now(), Instant::now()));
                        send_force_leave(&outbound, peer)?;
                    }
                }
                if incoming.as_ref().is_some_and(|value| peer_timed_out(&last_seen, &value.peer)) {
                    if let Some(active) = incoming.take() {
                        portal_edges.remove(&active.peer);
                        takeover = Some((active.peer.clone(), Instant::now(), Instant::now()));
                        send_force_leave(&outbound, active.peer)?;
                    }
                    injector.end()?;
                    capture.allow_all_edges();
                }
            }
        }
    }
}

#[cfg(debug_assertions)]
fn update_debug_escape_kill(
    keyboard: protocol::KeyboardInput,
    escape_started: &mut Option<Instant>,
) {
    if keyboard.key != ESC_KEY {
        return;
    }
    if keyboard.state == 0 {
        *escape_started = None;
    } else if escape_started.is_none() {
        *escape_started = Some(Instant::now());
    }
}

fn peer_timed_out(last_seen: &HashMap<String, Instant>, peer: &str) -> bool {
    last_seen
        .get(peer)
        .is_none_or(|seen| seen.elapsed() > PEER_TIMEOUT)
}

fn select_peer(
    cfg: &Config,
    online: &HashMap<String, Instant>,
    portal_edges: &HashMap<String, HashSet<Edge>>,
    edge: Edge,
    position: f64,
) -> Option<String> {
    let mut peers: Vec<_> = cfg
        .peers
        .keys()
        .filter(|peer| {
            online
                .get(*peer)
                .is_some_and(|seen| seen.elapsed() < Duration::from_secs(2))
                && portal_edge_available(portal_edges, peer, edge)
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

fn portal_edge_available(
    portal_edges: &HashMap<String, HashSet<Edge>>,
    peer: &str,
    edge: Edge,
) -> bool {
    portal_edge_confirmed(portal_edges, peer, edge)
        || !portal_edges
            .iter()
            .any(|(bound_peer, edges)| bound_peer != peer && edges.contains(&edge))
}

fn portal_edge_confirmed(
    portal_edges: &HashMap<String, HashSet<Edge>>,
    peer: &str,
    edge: Edge,
) -> bool {
    portal_edges
        .get(peer)
        .is_some_and(|edges| edges.contains(&edge))
}

fn forget_portal_edge(portal_edges: &mut HashMap<String, HashSet<Edge>>, peer: &str, edge: Edge) {
    let remove_peer = portal_edges.get_mut(peer).is_some_and(|edges| {
        edges.remove(&edge);
        edges.is_empty()
    });
    if remove_peer {
        portal_edges.remove(peer);
    }
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

fn crosses_host_edge(edge: Edge, pointer: protocol::PointerInput, depth: &mut f64) -> bool {
    let Some((outward, _)) = pressure(edge, pointer) else {
        return false;
    };
    *depth += outward;
    *depth <= 0.0
}

fn crosses_remote_entry_edge(
    entry_edge: Edge,
    pointer: protocol::PointerInput,
    depth: &mut f64,
) -> bool {
    crosses_host_edge(entry_edge.opposite(), pointer, depth)
}

fn scale_pointer_motion(
    pointer: PointerInput,
    local_screen: (f64, f64),
    peer_screen: (f64, f64),
) -> PointerInput {
    match pointer {
        PointerInput::Motion { dx, dy } => PointerInput::Motion {
            dx: dx * local_screen.0 / peer_screen.0,
            dy: dy * local_screen.1 / peer_screen.1,
        },
        other => other,
    }
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
            edge: destination_edge(source_edge),
            position,
            progress,
        },
    )
}

fn destination_edge(source_edge: Edge) -> Edge {
    source_edge.opposite()
}

fn send_force_leave(
    sender: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    peer: String,
) -> Result<()> {
    send(sender, peer, InputMessage::Leave)
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

    #[test]
    fn every_source_edge_maps_to_destination_opposite_edge() {
        for (source, destination) in [
            (Edge::Left, Edge::Right),
            (Edge::Right, Edge::Left),
            (Edge::Top, Edge::Bottom),
            (Edge::Bottom, Edge::Top),
        ] {
            assert_eq!(destination_edge(source), destination);

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            send_probe(&tx, "peer".to_owned(), source, 0.5, 0.75).unwrap();
            let outbound = rx.try_recv().unwrap();
            assert_eq!(
                outbound.message,
                InputMessage::Probe {
                    edge: destination,
                    position: 0.5,
                    progress: 0.75,
                }
            );
        }
    }

    #[test]
    fn cursor_returns_only_after_crossing_original_edge() {
        let mut depth = REMOTE_ENTRY_INSET;
        assert!(!crosses_host_edge(
            Edge::Right,
            PointerInput::Motion { dx: 20.0, dy: 0.0 },
            &mut depth,
        ));
        assert_eq!(depth, 23.0);
        assert!(!crosses_host_edge(
            Edge::Right,
            PointerInput::Motion { dx: -10.0, dy: 0.0 },
            &mut depth,
        ));
        assert_eq!(depth, 13.0);
        assert!(crosses_host_edge(
            Edge::Right,
            PointerInput::Motion { dx: -14.0, dy: 0.0 },
            &mut depth,
        ));
    }

    #[test]
    fn non_motion_does_not_change_remote_depth() {
        let mut depth = REMOTE_ENTRY_INSET;
        assert!(!crosses_host_edge(
            Edge::Right,
            PointerInput::Button {
                button: 0x110,
                state: 1
            },
            &mut depth,
        ));
        assert_eq!(depth, REMOTE_ENTRY_INSET);
    }

    #[test]
    fn remote_entry_edge_returns_to_controller() {
        let mut depth = REMOTE_ENTRY_INSET;
        assert!(!crosses_remote_entry_edge(
            Edge::Left,
            PointerInput::Motion { dx: 20.0, dy: 0.0 },
            &mut depth,
        ));
        assert_eq!(depth, 23.0);
        assert!(!crosses_remote_entry_edge(
            Edge::Left,
            PointerInput::Motion { dx: -10.0, dy: 0.0 },
            &mut depth,
        ));
        assert_eq!(depth, 13.0);
        assert!(crosses_remote_entry_edge(
            Edge::Left,
            PointerInput::Motion { dx: -14.0, dy: 0.0 },
            &mut depth,
        ));
    }

    #[test]
    fn portal_binding_allows_same_peer_on_every_unclaimed_edge() {
        let mut portal_edges = HashMap::from([("peer".to_owned(), HashSet::from([Edge::Right]))]);
        assert!(portal_edge_confirmed(&portal_edges, "peer", Edge::Right));
        assert!(!portal_edge_confirmed(&portal_edges, "peer", Edge::Left));
        assert!(portal_edge_available(&portal_edges, "peer", Edge::Right));
        assert!(portal_edge_available(&portal_edges, "peer", Edge::Left));
        assert!(!portal_edge_available(
            &portal_edges,
            "new-peer",
            Edge::Right
        ));
        assert!(portal_edge_available(&portal_edges, "new-peer", Edge::Left));

        portal_edges
            .entry("peer".to_owned())
            .or_default()
            .insert(Edge::Top);
        assert!(portal_edge_confirmed(&portal_edges, "peer", Edge::Right));
        assert!(portal_edge_confirmed(&portal_edges, "peer", Edge::Top));

        forget_portal_edge(&mut portal_edges, "peer", Edge::Right);
        assert!(!portal_edge_confirmed(&portal_edges, "peer", Edge::Right));
        assert!(portal_edge_confirmed(&portal_edges, "peer", Edge::Top));
        assert!(portal_edge_available(&portal_edges, "peer", Edge::Bottom));

        forget_portal_edge(&mut portal_edges, "peer", Edge::Top);
        assert!(!portal_edges.contains_key("peer"));
    }

    #[test]
    fn pointer_motion_scales_for_different_screen_sizes() {
        let PointerInput::Motion { dx, dy } = scale_pointer_motion(
            PointerInput::Motion { dx: 10.0, dy: 10.0 },
            (2560.0, 1080.0),
            (1920.0, 2160.0),
        ) else {
            panic!("motion must remain motion");
        };
        assert!((dx - 13.333333333333334).abs() < f64::EPSILON);
        assert_eq!(dy, 5.0);
        assert_eq!(
            scale_pointer_motion(
                PointerInput::Button { button: 1, state: 1 },
                (2560.0, 1080.0),
                (1920.0, 2160.0),
            ),
            PointerInput::Button { button: 1, state: 1 }
        );
    }

    #[test]
    fn peer_timeout_requires_recent_packet() {
        let mut seen = HashMap::new();
        assert!(peer_timed_out(&seen, "peer"));

        seen.insert("peer".to_owned(), Instant::now());
        assert!(!peer_timed_out(&seen, "peer"));

        seen.insert(
            "peer".to_owned(),
            Instant::now() - PEER_TIMEOUT - Duration::from_millis(1),
        );
        assert!(peer_timed_out(&seen, "peer"));
    }
}
