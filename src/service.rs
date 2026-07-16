use std::{fs, path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use directories::ProjectDirs;
use tokio::process::Command;

use crate::ServiceAction;

pub async fn run(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(),
        ServiceAction::Start => start().await,
        ServiceAction::Stop => stop().await,
        ServiceAction::Status => status().await,
        ServiceAction::Uninstall => uninstall().await,
    }
}

#[cfg(target_os = "linux")]
fn service_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("cannot determine config home")?;
    Ok(base.join("systemd/user/lan-cat.service"))
}

#[cfg(target_os = "macos")]
fn service_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "lan-cat", "lan-cat")
        .context("cannot determine config directory")?;
    Ok(dirs.config_dir().join("org.lan-cat.daemon.plist"))
}

fn install() -> Result<()> {
    let executable = std::env::current_exe()?;
    let path = service_path()?;
    fs::create_dir_all(path.parent().context("service path has no parent")?)?;
    #[cfg(target_os = "linux")]
    let contents = format!(
        r#"[Unit]
Description=Secure LAN clipboard synchronization
After=graphical-session.target network-online.target

[Service]
ExecStart={} daemon
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#,
        executable.display()
    );
    #[cfg(target_os = "macos")]
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>org.lan-cat.daemon</string>
<key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
<key>KeepAlive</key><true/>
<key>RunAtLoad</key><true/>
</dict></plist>
"#,
        xml_escape(&executable.to_string_lossy())
    );
    fs::write(&path, contents)?;
    println!(
        "Installed {}. Service remains manual-start.",
        path.display()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
async fn start() -> Result<()> {
    run_command(
        "systemctl",
        &[
            "--user",
            "import-environment",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
        ],
    )
    .await?;
    run_command("systemctl", &["--user", "daemon-reload"]).await?;
    run_command("systemctl", &["--user", "start", "lan-cat.service"]).await
}

#[cfg(target_os = "linux")]
async fn stop() -> Result<()> {
    run_command("systemctl", &["--user", "stop", "lan-cat.service"]).await
}

#[cfg(target_os = "linux")]
async fn status() -> Result<()> {
    run_command(
        "systemctl",
        &["--user", "status", "--no-pager", "lan-cat.service"],
    )
    .await
}

#[cfg(target_os = "macos")]
async fn start() -> Result<()> {
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    run_command_owned(
        "launchctl",
        vec!["bootstrap".into(), domain, service_path()?.into_os_string()],
    )
    .await
}

#[cfg(target_os = "macos")]
async fn stop() -> Result<()> {
    let target = format!("gui/{}/org.lan-cat.daemon", unsafe { libc::geteuid() });
    run_command_owned("launchctl", vec!["bootout".into(), target.into()]).await
}

#[cfg(target_os = "macos")]
async fn status() -> Result<()> {
    let target = format!("gui/{}/org.lan-cat.daemon", unsafe { libc::geteuid() });
    run_command_owned("launchctl", vec!["print".into(), target.into()]).await
}

async fn uninstall() -> Result<()> {
    let _ = stop().await;
    let path = service_path()?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    #[cfg(target_os = "linux")]
    run_command("systemctl", &["--user", "daemon-reload"]).await?;
    println!("Removed {}.", path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
async fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("{program} failed with {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn run_command_owned(program: &str, args: Vec<std::ffi::OsString>) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("{program} failed with {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
