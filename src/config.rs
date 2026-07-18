use std::{collections::BTreeMap, fs, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::ordering::VersionVector;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    /// X25519 public key, lowercase hex.
    pub public_key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CursorConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u16,
    pub name: String,
    /// X25519 private key, lowercase hex. Protected by directory and file mode.
    pub private_key: String,
    /// X25519 public key, lowercase hex.
    pub public_key: String,
    pub paused: bool,
    pub peers: BTreeMap<String, Peer>,
    /// Cursor sharing is opt-in because it permits remote input injection.
    #[serde(default)]
    pub cursor: CursorConfig,
    /// Content-free causal metadata. Prevents sequence reuse after restart.
    #[serde(default)]
    pub clock: VersionVector,
}

impl Config {
    pub fn load_or_create() -> Result<Self> {
        let path = config_path()?;
        if path.exists() {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let cfg: Self = serde_json::from_slice(&bytes).context("parse config")?;
            cfg.validate()?;
            if let Some(parent) = path.parent() {
                set_private_dir(parent)?;
            }
            set_private_file(&path)?;
            return Ok(cfg);
        }

        let params: snow::params::NoiseParams = "Noise_NN_25519_ChaChaPoly_BLAKE2s".parse()?;
        let key = snow::Builder::new(params).generate_keypair()?;
        let cfg = Self {
            version: 1,
            name: hostname(),
            private_key: hex::encode(key.private),
            public_key: hex::encode(key.public),
            paused: false,
            peers: BTreeMap::new(),
            cursor: CursorConfig::default(),
            clock: VersionVector::default(),
        };
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let path = config_path()?;
        let parent = path.parent().context("config path has no parent")?;
        fs::create_dir_all(parent)?;
        set_private_dir(parent)?;
        let tmp = path.with_extension("json.tmp");
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn private_key_bytes(&self) -> Result<[u8; 32]> {
        let bytes = hex::decode(&self.private_key)?;
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity key must be 32 bytes"))
    }

    pub fn public_key(&self) -> Result<[u8; 32]> {
        let bytes = hex::decode(&self.public_key)?;
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity public key must be 32 bytes"))
    }

    pub fn set_name(&mut self, name: String) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
            bail!("device name must be 1..63 printable characters");
        }
        self.name = name.to_owned();
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported config version {}", self.version);
        }
        if hex::decode(&self.private_key)?.len() != 32 {
            bail!("identity key must be 32 bytes");
        }
        if hex::decode(&self.public_key)?.len() != 32 {
            bail!("identity public key must be 32 bytes");
        }
        Ok(())
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "lan-cat", "lan-cat")
        .context("cannot determine user config directory")?;
    Ok(dirs.config_dir().join("config.json"))
}

pub fn runtime_socket() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(dir).join("lan-cat.sock"));
        }
    }
    let dirs = ProjectDirs::from("org", "lan-cat", "lan-cat")
        .context("cannot determine user state directory")?;
    Ok(dirs.cache_dir().join("lan-cat.sock"))
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "lan-cat".into())
        .chars()
        .take(63)
        .collect()
}

fn set_private_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_config_defaults_cursor_to_disabled() {
        let json = serde_json::json!({
            "version": 1,
            "name": "test",
            "private_key": "00".repeat(32),
            "public_key": "11".repeat(32),
            "paused": false,
            "peers": {},
            "clock": {}
        });
        let cfg: Config = serde_json::from_value(json).unwrap();
        assert!(!cfg.cursor.enabled);
    }
}
