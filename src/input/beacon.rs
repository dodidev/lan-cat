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
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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
        .with_app_id("lan-cat-cursor-portal")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_resizable(false)
        .with_fullscreen(true)
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
    animation_time: f32,
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
            animation_time: 0.0,
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
        self.animation_time += dt;

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

        // Universal Control-style animation parameters
        let base_depth = 8.0;
        let max_depth = 95.0;
        let depth = base_depth + self.progress * max_depth + snap * 12.0;
        let cursor_size = 11.0 + self.progress * 2.0;

        let along = match self.edge {
            Edge::Left | Edge::Right => rect.top() + rect.height() * self.displayed_position,
            Edge::Top | Edge::Bottom => rect.left() + rect.width() * self.displayed_position,
        };

        let (edge_point, cursor_point, is_horizontal) = match self.edge {
            Edge::Left => (
                egui::pos2(rect.left(), along),
                egui::pos2(rect.left() + depth, along),
                false,
            ),
            Edge::Right => (
                egui::pos2(rect.right(), along),
                egui::pos2(rect.right() - depth, along),
                false,
            ),
            Edge::Top => (
                egui::pos2(along, rect.top()),
                egui::pos2(along, rect.top() + depth),
                true,
            ),
            Edge::Bottom => (
                egui::pos2(along, rect.bottom()),
                egui::pos2(along, rect.bottom() - depth),
                true,
            ),
        };

        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("cursor-portal"),
        ));

        // Universal Control portal animation
        if self.progress > 0.01 {
            // Pulsing animation (subtle breathing effect)
            let pulse = (self.animation_time * 2.5).sin() * 0.15 + 1.0;

            // Calculate bezier curve control point for fluid pull effect
            let pull_amount = 80.0 * self.progress * self.progress;
            let control_point = if is_horizontal {
                let side_offset = if self.edge == Edge::Top { 1.0 } else { -1.0 };
                edge_point
                    + egui::vec2(0.0, pull_amount * side_offset)
                    + (cursor_point - edge_point) * 0.5
            } else {
                let side_offset = if self.edge == Edge::Left { 1.0 } else { -1.0 };
                edge_point
                    + egui::vec2(pull_amount * side_offset, 0.0)
                    + (cursor_point - edge_point) * 0.5
            };

            // Draw fluid droplets/particles along the bezier path
            for i in 0..8 {
                let t = (i as f32 / 8.0) * 0.7; // Only first 70% of path
                let offset = i as f32 * 0.3;
                let particle_pulse = (self.animation_time * 3.0 + offset).sin() * 0.5 + 0.5;

                // Bezier position
                let pos = edge_point * (1.0 - t).powi(2)
                    + control_point.to_vec2() * (2.0 * (1.0 - t) * t)
                    + cursor_point.to_vec2() * t.powi(2);

                let particle_size = (8.0 + self.progress * 6.0) * (1.0 - t * 0.5) * particle_pulse;
                let particle_alpha = self.progress * (1.0 - t * 0.6);

                // Particle glow
                painter.circle_filled(
                    egui::pos2(pos.x, pos.y),
                    particle_size * 1.5,
                    egui::Color32::from_rgba_premultiplied(
                        (100.0 * particle_alpha * 0.7) as u8,
                        (140.0 * particle_alpha * 0.7) as u8,
                        (255.0 * particle_alpha * 0.7) as u8,
                        (50.0 * particle_alpha) as u8,
                    ),
                );

                // Particle core
                painter.circle_filled(
                    egui::pos2(pos.x, pos.y),
                    particle_size,
                    egui::Color32::from_rgba_premultiplied(
                        (180.0 * particle_alpha) as u8,
                        (210.0 * particle_alpha) as u8,
                        (255.0 * particle_alpha) as u8,
                        (140.0 * particle_alpha) as u8,
                    ),
                );
            }

            // Main fluid portal at edge - stretched elliptical shape
            let portal_base_size = 30.0 + self.progress * 40.0;

            // Stretch factor based on pull direction
            let stretch_major = portal_base_size * (1.5 + self.progress * 0.8) * pulse;
            let stretch_minor = portal_base_size * (0.8 - self.progress * 0.2) * pulse;

            // Draw stretched portal as ellipse
            let portal_rect = if is_horizontal {
                egui::Rect::from_center_size(
                    edge_point,
                    egui::vec2(stretch_major * 1.8, stretch_minor * 0.6),
                )
            } else {
                egui::Rect::from_center_size(
                    edge_point,
                    egui::vec2(stretch_minor * 0.6, stretch_major * 1.8),
                )
            };

            // Outermost fluid layer (very soft purple glow)
            painter.rect_filled(
                portal_rect,
                portal_base_size,
                egui::Color32::from_rgba_premultiplied(
                    (100.0 * self.progress) as u8,
                    (100.0 * self.progress) as u8,
                    (200.0 * self.progress) as u8,
                    (35.0 * self.progress) as u8,
                ),
            );

            // Middle fluid layer (blue)
            let mid_rect = if is_horizontal {
                egui::Rect::from_center_size(
                    edge_point,
                    egui::vec2(stretch_major * 1.3, stretch_minor * 0.5),
                )
            } else {
                egui::Rect::from_center_size(
                    edge_point,
                    egui::vec2(stretch_minor * 0.5, stretch_major * 1.3),
                )
            };
            painter.rect_filled(
                mid_rect,
                portal_base_size * 0.8,
                egui::Color32::from_rgba_premultiplied(
                    (120.0 * self.progress) as u8,
                    (160.0 * self.progress) as u8,
                    (255.0 * self.progress) as u8,
                    (90.0 * self.progress) as u8,
                ),
            );

            // Inner bright core
            let core_rect = if is_horizontal {
                egui::Rect::from_center_size(
                    edge_point,
                    egui::vec2(stretch_major * 0.7, stretch_minor * 0.35),
                )
            } else {
                egui::Rect::from_center_size(
                    edge_point,
                    egui::vec2(stretch_minor * 0.35, stretch_major * 0.7),
                )
            };
            painter.rect_filled(
                core_rect,
                portal_base_size * 0.5,
                egui::Color32::from_rgba_premultiplied(
                    (200.0 * self.progress) as u8,
                    (220.0 * self.progress) as u8,
                    (255.0 * self.progress) as u8,
                    (180.0 * self.progress) as u8,
                ),
            );

            // Draw smooth fluid stream with bezier curve
            let segments = 25;
            for i in 0..segments {
                let t = i as f32 / segments as f32;
                let next_t = (i + 1) as f32 / segments as f32;

                // Bezier curve points
                let p1 = edge_point * (1.0 - t).powi(2)
                    + control_point.to_vec2() * (2.0 * (1.0 - t) * t)
                    + cursor_point.to_vec2() * t.powi(2);

                let p2 = edge_point * (1.0 - next_t).powi(2)
                    + control_point.to_vec2() * (2.0 * (1.0 - next_t) * next_t)
                    + cursor_point.to_vec2() * next_t.powi(2);

                // Width varies along curve for organic fluid look
                let width_base = 1.0 - t * 0.6;
                let width_variation = (self.animation_time * 4.0 + t * 8.0).sin() * 0.1 + 1.0;
                let width_mult = width_base * width_variation;
                let alpha_mult = 1.0 - t * 0.4;

                // Outer fluid stream (soft glow)
                let stream_width_outer = (9.0 + self.progress * 7.0) * width_mult;
                painter.line_segment(
                    [egui::pos2(p1.x, p1.y), egui::pos2(p2.x, p2.y)],
                    egui::Stroke::new(
                        stream_width_outer,
                        egui::Color32::from_rgba_premultiplied(
                            (100.0 * self.progress * alpha_mult) as u8,
                            (140.0 * self.progress * alpha_mult) as u8,
                            (255.0 * self.progress * alpha_mult) as u8,
                            (50.0 * self.progress * alpha_mult) as u8,
                        ),
                    ),
                );

                // Inner fluid stream (bright core)
                let stream_width_inner = (4.5 + self.progress * 3.5) * width_mult;
                painter.line_segment(
                    [egui::pos2(p1.x, p1.y), egui::pos2(p2.x, p2.y)],
                    egui::Stroke::new(
                        stream_width_inner,
                        egui::Color32::from_rgba_premultiplied(
                            (180.0 * self.progress * alpha_mult) as u8,
                            (210.0 * self.progress * alpha_mult) as u8,
                            (255.0 * self.progress * alpha_mult) as u8,
                            (150.0 * self.progress * alpha_mult) as u8,
                        ),
                    ),
                );
            }
        }

        // Draw cursor with macOS-style appearance
        // Soft outer glow when active
        if self.progress > 0.1 {
            let glow_radius = cursor_size + 8.0;
            let cursor_glow = egui::Color32::from_rgba_premultiplied(
                (100.0 * self.progress) as u8,
                (140.0 * self.progress) as u8,
                (255.0 * self.progress) as u8,
                (40.0 * self.progress) as u8,
            );
            painter.circle_filled(cursor_point, glow_radius, cursor_glow);
        }

        // Shadow layer
        let shadow_offset = egui::vec2(0.5, 1.5);
        let shadow = egui::Color32::from_rgba_premultiplied(0, 0, 0, 120);
        painter.circle_filled(cursor_point + shadow_offset, cursor_size + 1.0, shadow);

        // Outer cursor ring (white)
        painter.circle_filled(cursor_point, cursor_size, egui::Color32::WHITE);

        // Inner cursor (dark gray/black with slight blue tint when active)
        let cursor_inner_size = cursor_size - 2.2;
        let cursor_inner = if self.progress > 0.1 {
            egui::Color32::from_rgb(
                (30.0 + 50.0 * self.progress) as u8,
                (30.0 + 70.0 * self.progress) as u8,
                (30.0 + 110.0 * self.progress) as u8,
            )
        } else {
            egui::Color32::from_rgb(35, 35, 40)
        };
        painter.circle_filled(cursor_point, cursor_inner_size, cursor_inner);

        // Highlight on cursor for 3D effect
        let highlight_offset = egui::vec2(-cursor_size * 0.28, -cursor_size * 0.32);
        let highlight_size = cursor_size * 0.4;
        let highlight = egui::Color32::from_rgba_premultiplied(255, 255, 255, 170);
        painter.circle_filled(cursor_point + highlight_offset, highlight_size, highlight);

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
