use std::{
    io::{BufRead, Write},
    process::{Child, ChildStdin, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};

use super::protocol::Edge;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Update {
    position: f64,
    progress: f32,
    confirmed: bool,
    cancel: bool,
}

pub struct Beacon {
    child: Child,
    stdin: ChildStdin,
    position: f64,
    progress: f32,
}

impl Beacon {
    pub fn show(edge: Edge, position: f64, peer: &str) -> Result<Self> {
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .arg("cursor-beacon-ui")
            .arg("--edge")
            .arg(edge.to_string())
            .arg("--position")
            .arg(position.to_string())
            .arg("--peer")
            .arg(peer)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("launch cursor edge portal")?;
        let stdin = child
            .stdin
            .take()
            .context("cursor portal stdin is unavailable")?;
        Ok(Self {
            child,
            stdin,
            position,
            progress: 0.0,
        })
    }

    pub fn update(&mut self, position: f64, progress: f32, confirmed: bool) {
        self.position = position;
        self.progress = progress;
        self.send(Update {
            position,
            progress,
            confirmed,
            cancel: false,
        });
    }

    pub fn confirm(&mut self) {
        self.update(self.position, 1.0, true);
    }

    pub fn cancel(&mut self) {
        self.send(Update {
            position: self.position,
            progress: self.progress,
            confirmed: false,
            cancel: true,
        });
    }

    fn send(&mut self, update: Update) {
        if let Ok(line) = serde_json::to_vec(&update) {
            let _ = self.stdin.write_all(&line);
            let _ = self.stdin.write_all(b"\n");
            let _ = self.stdin.flush();
        }
    }
}

impl Drop for Beacon {
    fn drop(&mut self) {
        let _ = self.child.try_wait();
    }
}

pub fn run_ui(edge: Edge, position: f64, _peer: String) -> Result<()> {
    let (updates_tx, updates_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin().lock();
        for line in stdin.lines().map_while(Result::ok) {
            if let Ok(update) = serde_json::from_str(&line) {
                if updates_tx.send(update).is_err() {
                    break;
                }
            }
        }
        let _ = updates_tx.send(Update {
            position,
            progress: 0.0,
            confirmed: false,
            cancel: true,
        });
    });

    let viewport = egui::ViewportBuilder::default()
        .with_title("lan-cat cursor portal")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_maximized(true)
        .with_taskbar(false);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "lan-cat cursor portal",
        options,
        Box::new(move |_cc| Ok(Box::new(PortalApp::new(edge, position, updates_rx)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

struct PortalApp {
    edge: Edge,
    position: f32,
    displayed_position: f32,
    progress: f32,
    target_progress: f32,
    updates: mpsc::Receiver<Update>,
    transition: Option<(Instant, bool)>,
    last_frame: Instant,
}

impl PortalApp {
    fn new(edge: Edge, position: f64, updates: mpsc::Receiver<Update>) -> Self {
        Self {
            edge,
            position: position as f32,
            displayed_position: position as f32,
            progress: 0.0,
            target_progress: 0.0,
            updates,
            transition: None,
            last_frame: Instant::now(),
        }
    }
}

impl eframe::App for PortalApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.updates.try_recv() {
            self.position = update.position as f32;
            self.target_progress = update.progress;
            if update.confirmed {
                self.transition = Some((Instant::now(), true));
            } else if update.cancel {
                self.transition = Some((Instant::now(), false));
                self.target_progress = 0.0;
            }
        }
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        let smoothing = 1.0 - (-14.0 * dt).exp();
        self.progress += (self.target_progress - self.progress) * smoothing;
        self.displayed_position += (self.position - self.displayed_position) * smoothing;

        let mut snap = 0.0;
        if let Some((started, confirmed)) = self.transition {
            let elapsed = started.elapsed().as_secs_f32();
            if confirmed {
                snap = (elapsed * 22.0).sin() * (-7.0 * elapsed).exp();
                if elapsed > 0.42 {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else if elapsed > 0.28 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        let rect = ctx.screen_rect();
        let depth = 10.0 + self.progress * 74.0 + snap * 9.0;
        let radius = 9.0 + self.progress * 5.0;
        let along = match self.edge {
            Edge::Left | Edge::Right => rect.top() + rect.height() * self.displayed_position,
            Edge::Top | Edge::Bottom => rect.left() + rect.width() * self.displayed_position,
        };
        let (anchor, head) = match self.edge {
            Edge::Left => (
                egui::pos2(rect.left() - 2.0, along),
                egui::pos2(rect.left() + depth, along),
            ),
            Edge::Right => (
                egui::pos2(rect.right() + 2.0, along),
                egui::pos2(rect.right() - depth, along),
            ),
            Edge::Top => (
                egui::pos2(along, rect.top() - 2.0),
                egui::pos2(along, rect.top() + depth),
            ),
            Edge::Bottom => (
                egui::pos2(along, rect.bottom() + 2.0),
                egui::pos2(along, rect.bottom() - depth),
            ),
        };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("cursor-portal"),
        ));
        let shadow = egui::Color32::from_rgba_premultiplied(0, 0, 0, 85);
        let fluid = egui::Color32::from_rgb(238, 242, 247);
        painter.line_segment([anchor, head], egui::Stroke::new(radius * 1.45, shadow));
        painter.circle_filled(head, radius + 2.5, shadow);
        painter.line_segment([anchor, head], egui::Stroke::new(radius, fluid));
        painter.circle_filled(head, radius, fluid);
        painter.circle_filled(
            head + egui::vec2(-radius * 0.22, -radius * 0.25),
            radius * 0.23,
            egui::Color32::WHITE,
        );

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
