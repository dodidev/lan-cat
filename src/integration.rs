use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::IntegrationAction;

#[cfg(target_os = "linux")]
const ACTION_ID: &str = "lan-cat-share-action";

pub fn run(action: IntegrationAction) -> Result<()> {
    match action {
        IntegrationAction::Install => install(),
        IntegrationAction::Uninstall => uninstall(),
    }
}

#[cfg(target_os = "linux")]
fn install() -> Result<()> {
    let path = thunar_config_path()?;
    let mut contents = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n".into()
    };
    if contents.contains(&format!("<unique-id>{ACTION_ID}</unique-id>")) {
        println!("Thunar action already installed at {}.", path.display());
        return Ok(());
    }
    let executable = shell_quote(&std::env::current_exe()?.to_string_lossy());
    let command = xml_escape(&format!("{executable} share -- %F"));
    let action = format!(
        r#"<action>
  <icon>folder-remote</icon>
  <name>Share with lan-cat</name>
  <submenu></submenu>
  <unique-id>{ACTION_ID}</unique-id>
  <command>{command}</command>
  <description>Securely share selected files with a paired lan-cat device</description>
  <range>*</range>
  <patterns>*</patterns>
  <audio-files/>
  <image-files/>
  <other-files/>
  <text-files/>
  <video-files/>
</action>
"#
    );
    let end = contents
        .rfind("</actions>")
        .context("Thunar uca.xml has no closing actions element")?;
    contents.insert_str(end, &action);
    fs::create_dir_all(path.parent().context("Thunar config path has no parent")?)?;
    fs::write(&path, contents)?;
    println!("Installed Thunar action at {}.", path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall() -> Result<()> {
    let path = thunar_config_path()?;
    if !path.exists() {
        return Ok(());
    }
    let contents = fs::read_to_string(&path)?;
    let Some(contents) = remove_action(&contents) else {
        return Ok(());
    };
    fs::write(&path, contents)?;
    println!("Removed Thunar action from {}.", path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn thunar_config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("cannot determine config directory")?;
    Ok(base.join("Thunar/uca.xml"))
}

#[cfg(target_os = "linux")]
fn remove_action(contents: &str) -> Option<String> {
    let marker = format!("<unique-id>{ACTION_ID}</unique-id>");
    let marker_at = contents.find(&marker)?;
    let start = contents[..marker_at].rfind("<action>")?;
    let end_relative = contents[marker_at..].find("</action>")?;
    let end = marker_at + end_relative + "</action>".len();
    let mut updated = contents.to_owned();
    updated.replace_range(start..end, "");
    Some(updated)
}

#[cfg(target_os = "macos")]
fn install() -> Result<()> {
    let path = macos_workflow_path()?;
    let contents = path.join("Contents");
    fs::create_dir_all(&contents)?;
    let executable = shell_quote(&std::env::current_exe()?.to_string_lossy());
    let command = xml_escape(&format!("exec {executable} share -- \"$@\""));
    fs::write(contents.join("document.wflow"), workflow(&command))?;
    println!("Installed Finder Quick Action at {}.", path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<()> {
    let path = macos_workflow_path()?;
    if path.exists() {
        fs::remove_dir_all(&path)?;
        println!("Removed Finder Quick Action at {}.", path.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_workflow_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Services/Share with lan-cat.workflow"))
}

#[cfg(target_os = "macos")]
fn workflow(command: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>AMApplicationBuild</key><string>521</string>
<key>AMApplicationVersion</key><string>2.10</string>
<key>AMDocumentVersion</key><string>2</string>
<key>actions</key><array><dict><key>action</key><dict>
<key>ActionBundlePath</key><string>/System/Library/Automator/Run Shell Script.action</string>
<key>ActionName</key><string>Run Shell Script</string>
<key>ActionParameters</key><dict>
<key>COMMAND_STRING</key><string>{command}</string>
<key>CheckedForUserDefaultShell</key><true/>
<key>inputMethod</key><integer>1</integer>
<key>shell</key><string>/bin/zsh</string>
</dict>
</dict></dict></array>
<key>connectors</key><dict/>
<key>workflowMetaData</key><dict>
<key>serviceApplicationBundleID</key><string>com.apple.finder</string>
<key>serviceApplicationPath</key><string>/System/Library/CoreServices/Finder.app</string>
<key>serviceInputTypeIdentifier</key><string>com.apple.Automator.fileSystemObject</string>
<key>serviceOutputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
<key>serviceProcessesInput</key><integer>0</integer>
</dict></dict></plist>
"#
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_is_safe_for_shell_and_xml() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(xml_escape("a&<b>"), "a&amp;&lt;b&gt;");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn workflow_is_valid_plist() {
        let value = workflow("/tmp/lan-cat share -- &quot;$@&quot;");
        assert!(value.contains("com.apple.Automator.fileSystemObject"));
        assert!(value.contains("Run Shell Script.action"));
    }
}
