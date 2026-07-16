use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use eframe::egui;
use uuid::Uuid;

use crate::{
    ipc::{self, PeerView, Request, ResponseData},
    transfer::{Direction, TransferState, TransferView},
};

pub fn share(paths: Vec<PathBuf>, preferred_peer: Option<String>) -> Result<()> {
    open_sender(paths, preferred_peer, false)
}

pub fn copy_prompt(paths: Vec<PathBuf>) -> Result<()> {
    run_copy_prompt(
        "Files copied",
        TransferApp::sender(paths, Vec::new(), None, true),
    )
}

fn open_sender(
    paths: Vec<PathBuf>,
    preferred_peer: Option<String>,
    copied_prompt: bool,
) -> Result<()> {
    let response = ipc::call_blocking(&Request::PeerList)?;
    let Some(ResponseData::Peers { peers }) = response.data else {
        anyhow::bail!("daemon returned no peer list");
    };
    let selected = preferred_peer
        .and_then(|wanted| peers.iter().position(|peer| peer.id.starts_with(&wanted)))
        .or_else(|| peers.iter().position(|peer| peer.connected));
    let app = TransferApp::sender(paths, peers, selected, copied_prompt);
    run(
        if copied_prompt {
            "Files copied"
        } else {
            "Share with lan-cat"
        },
        app,
    )
}

pub fn receive(id: Uuid) -> Result<()> {
    let transfer = get_transfer(id)?;
    if transfer.direction != Direction::Receive {
        anyhow::bail!("transfer is not incoming");
    }
    run("Incoming lan-cat transfer", TransferApp::receiver(transfer))
}

pub fn transfers() -> Result<()> {
    let response = ipc::call_blocking(&Request::TransferList)?;
    let Some(ResponseData::Transfers { transfers }) = response.data else {
        anyhow::bail!("daemon returned no transfer list");
    };
    run("lan-cat transfers", TransferApp::list(transfers))
}

fn run(title: &str, app: TransferApp) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 360.0])
            .with_min_inner_size([420.0, 280.0]),
        ..Default::default()
    };
    run_native(title, options, app)
}

fn run_copy_prompt(title: &str, app: TransferApp) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("lan-cat-copy-prompt")
            .with_inner_size([420.0, 260.0])
            .with_min_inner_size([420.0, 260.0])
            .with_max_inner_size([420.0, 260.0])
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(),
        ..Default::default()
    };
    run_native(title, options, app)
}

fn run_native(title: &str, options: eframe::NativeOptions, app: TransferApp) -> Result<()> {
    eframe::run_native(title, options, Box::new(move |_| Ok(Box::new(app))))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

enum Mode {
    Send {
        paths: Vec<PathBuf>,
        peers: Vec<PeerView>,
        selected: Option<usize>,
        copied_prompt: bool,
        copy_choice: usize,
    },
    Receive,
    List(Vec<TransferView>),
}

struct TransferApp {
    mode: Mode,
    transfer: Option<TransferView>,
    destination: String,
    last_poll: Instant,
    speed_sample: Instant,
    speed_bytes: u64,
    bytes_per_second: f64,
    error: Option<String>,
}

impl TransferApp {
    fn sender(
        paths: Vec<PathBuf>,
        peers: Vec<PeerView>,
        selected: Option<usize>,
        copied_prompt: bool,
    ) -> Self {
        Self {
            mode: Mode::Send {
                paths,
                peers,
                selected,
                copied_prompt,
                copy_choice: 0,
            },
            transfer: None,
            destination: String::new(),
            last_poll: Instant::now(),
            speed_sample: Instant::now(),
            speed_bytes: 0,
            bytes_per_second: 0.0,
            error: None,
        }
    }

    fn receiver(transfer: TransferView) -> Self {
        Self {
            destination: transfer
                .destination
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            mode: Mode::Receive,
            transfer: Some(transfer),
            last_poll: Instant::now(),
            speed_sample: Instant::now(),
            speed_bytes: 0,
            bytes_per_second: 0.0,
            error: None,
        }
    }

    fn list(transfers: Vec<TransferView>) -> Self {
        Self {
            mode: Mode::List(transfers),
            transfer: None,
            destination: String::new(),
            last_poll: Instant::now(),
            speed_sample: Instant::now(),
            speed_bytes: 0,
            bytes_per_second: 0.0,
            error: None,
        }
    }

    fn poll(&mut self) {
        let Some(id) = self.transfer.as_ref().map(|value| value.id) else {
            return;
        };
        if self.last_poll.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.last_poll = Instant::now();
        match get_transfer(id) {
            Ok(transfer) => {
                let elapsed = self.speed_sample.elapsed().as_secs_f64();
                if elapsed >= 0.75 {
                    self.bytes_per_second =
                        transfer.transferred_bytes.saturating_sub(self.speed_bytes) as f64
                            / elapsed;
                    self.speed_bytes = transfer.transferred_bytes;
                    self.speed_sample = Instant::now();
                }
                self.transfer = Some(transfer);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn send_ui(&mut self, ui: &mut egui::Ui) {
        let Mode::Send {
            paths,
            peers,
            selected,
            copied_prompt,
            copy_choice,
        } = &mut self.mode
        else {
            return;
        };
        if *copied_prompt {
            let move_up = ui.input(|input| input.key_pressed(egui::Key::ArrowUp));
            let move_down = ui.input(|input| input.key_pressed(egui::Key::ArrowDown));
            let tab = ui.input(|input| input.key_pressed(egui::Key::Tab));
            let confirm = ui.input(|input| input.key_pressed(egui::Key::Enter));
            let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
            if escape {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            if move_up {
                *copy_choice = copy_choice.saturating_sub(1);
            }
            if move_down {
                *copy_choice = (*copy_choice + 1).min(1);
            }
            if tab {
                *copy_choice = 1 - *copy_choice;
            }

            ui.vertical_centered(|ui| {
                ui.heading("Files copied");
                ui.label("Choose what lan-cat should do.");
                ui.add_space(20.0);
                let size = egui::vec2(360.0, 64.0);
                let normal = ui.add_sized(
                    size,
                    egui::Button::new(egui::RichText::new("Normal copy only").size(20.0))
                        .selected(*copy_choice == 0),
                );
                ui.add_space(10.0);
                let sync = ui.add_sized(
                    size,
                    egui::Button::new(egui::RichText::new("Sync clipboard").size(20.0))
                        .selected(*copy_choice == 1),
                );
                ui.add_space(14.0);
                ui.label("↑/↓ select  ·  Enter confirm  ·  Esc normal copy");

                if normal.clicked() || (confirm && *copy_choice == 0) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if sync.clicked() || (confirm && *copy_choice == 1) {
                    match ipc::call_blocking(&Request::ClipboardSyncFiles {
                        paths: paths.clone(),
                    }) {
                        Ok(_) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
            });
            return;
        }
        ui.heading("Share files with lan-cat");
        ui.add_space(8.0);
        egui::ComboBox::from_label("Destination peer")
            .selected_text(
                selected
                    .and_then(|index| peers.get(index))
                    .map(|peer| {
                        format!(
                            "{}{}",
                            peer.name,
                            if peer.connected { "" } else { " (offline)" }
                        )
                    })
                    .unwrap_or_else(|| "Select peer".into()),
            )
            .show_ui(ui, |ui| {
                for (index, peer) in peers.iter().enumerate() {
                    ui.selectable_value(
                        selected,
                        Some(index),
                        format!(
                            "{}{}",
                            peer.name,
                            if peer.connected { "" } else { " (offline)" }
                        ),
                    );
                }
            });
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(160.0)
            .show(ui, |ui| {
                for path in paths.iter() {
                    ui.label(path.display().to_string());
                }
            });
        let enabled = selected
            .and_then(|index| peers.get(index))
            .is_some_and(|peer| peer.connected)
            && !paths.is_empty();
        let mut share_clicked = false;
        ui.horizontal(|ui| {
            share_clicked = ui
                .add_enabled(enabled, egui::Button::new("Share with lan-cat"))
                .clicked();
        });
        if share_clicked {
            let peer = peers[selected.expect("enabled selection")].id.clone();
            match ipc::call_blocking(&Request::TransferStart {
                peer,
                paths: paths.clone(),
            }) {
                Ok(response) => match response.data {
                    Some(ResponseData::Started { id }) => match get_transfer(id) {
                        Ok(transfer) => self.transfer = Some(transfer),
                        Err(error) => self.error = Some(error.to_string()),
                    },
                    _ => self.error = Some("daemon returned no transfer ID".into()),
                },
                Err(error) => self.error = Some(error.to_string()),
            }
        }
    }

    fn receive_ui(&mut self, ui: &mut egui::Ui) {
        let Some(transfer) = self.transfer.clone() else {
            return;
        };
        ui.heading(format!("Incoming files from {}", transfer.peer));
        ui.label(format!(
            "{} files, {}",
            transfer.files.len(),
            format_bytes(transfer.total_bytes)
        ));
        egui::ScrollArea::vertical()
            .max_height(120.0)
            .show(ui, |ui| {
                for file in &transfer.files {
                    ui.label(format!("{}  ({})", file.name, format_bytes(file.size)));
                }
            });
        if transfer.state == TransferState::Waiting {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Save to:");
                ui.text_edit_singleline(&mut self.destination);
            });
            ui.horizontal(|ui| {
                if ui.button("Accept").clicked() {
                    let request = Request::TransferAccept {
                        id: transfer.id,
                        destination: PathBuf::from(&self.destination),
                    };
                    if let Err(error) = ipc::call_blocking(&request) {
                        self.error = Some(error.to_string());
                    }
                }
                if ui.button("Reject").clicked() {
                    if let Err(error) =
                        ipc::call_blocking(&Request::TransferReject { id: transfer.id })
                    {
                        self.error = Some(error.to_string());
                    }
                }
            });
        }
    }

    fn progress_ui(&mut self, ui: &mut egui::Ui) {
        let Some(transfer) = self.transfer.clone() else {
            return;
        };
        ui.separator();
        ui.label(format!("Status: {:?}", transfer.state));
        let fraction = if transfer.total_bytes == 0 {
            if transfer.state == TransferState::Completed {
                1.0
            } else {
                0.0
            }
        } else {
            transfer.transferred_bytes as f32 / transfer.total_bytes as f32
        };
        ui.add(
            egui::ProgressBar::new(fraction)
                .show_percentage()
                .text(format!(
                    "{} / {}",
                    format_bytes(transfer.transferred_bytes),
                    format_bytes(transfer.total_bytes)
                )),
        );
        if transfer.state == TransferState::Transferring {
            ui.label(format!("{}/s", format_bytes(self.bytes_per_second as u64)));
            if ui.button("Cancel").clicked() {
                if let Err(error) = ipc::call_blocking(&Request::TransferCancel { id: transfer.id })
                {
                    self.error = Some(error.to_string());
                }
            }
        }
        if let Some(error) = transfer.error {
            ui.colored_label(egui::Color32::RED, error);
        }
    }
}

impl eframe::App for TransferApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        egui::CentralPanel::default().show(ctx, |ui| {
            match &self.mode {
                Mode::Send { .. } if self.transfer.is_none() => self.send_ui(ui),
                Mode::Receive => self.receive_ui(ui),
                Mode::List(transfers) => {
                    ui.heading("lan-cat transfers");
                    for transfer in transfers {
                        ui.label(format!(
                            "{} · {:?} · {:?} · {}",
                            transfer.peer,
                            transfer.direction,
                            transfer.state,
                            format_bytes(transfer.total_bytes)
                        ));
                    }
                }
                _ => {}
            }
            self.progress_ui(ui);
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::RED, error);
            }
        });
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn get_transfer(id: Uuid) -> Result<TransferView> {
    let response = ipc::call_blocking(&Request::TransferGet { id })?;
    let Some(ResponseData::Transfer { transfer }) = response.data else {
        anyhow::bail!("daemon returned no transfer status");
    };
    Ok(transfer)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
