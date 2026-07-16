use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CHUNK_BYTES: usize = 48 * 1024;
pub const MAX_TRANSFER_BYTES: u64 = 100 * 1024 * 1024 * 1024;
pub const MAX_TRANSFER_FILES: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileManifest {
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TransferMessage {
    Offer {
        id: Uuid,
        files: Vec<FileManifest>,
        total_bytes: u64,
    },
    Response {
        id: Uuid,
        accepted: bool,
    },
    Chunk {
        id: Uuid,
        file_index: u32,
        offset: u64,
        data: Vec<u8>,
    },
    Ack {
        id: Uuid,
        file_index: u32,
        next_offset: u64,
    },
    Complete {
        id: Uuid,
    },
    Finished {
        id: Uuid,
    },
    Cancel {
        id: Uuid,
        reason: String,
    },
}

pub fn validate_manifest(files: &[FileManifest], total_bytes: u64) -> Result<()> {
    if files.is_empty() || files.len() > MAX_TRANSFER_FILES {
        bail!("transfer must contain 1..={MAX_TRANSFER_FILES} files");
    }
    if total_bytes > MAX_TRANSFER_BYTES {
        bail!("transfer exceeds {} bytes", MAX_TRANSFER_BYTES);
    }
    let mut names = std::collections::HashSet::new();
    let mut calculated = 0_u64;
    for file in files {
        if file.name.is_empty()
            || file.name == "."
            || file.name == ".."
            || file.name.contains(['/', '\\', '\0'])
        {
            bail!("invalid transfer filename");
        }
        if !names.insert(&file.name) {
            bail!("duplicate transfer filename {}", file.name);
        }
        calculated = calculated
            .checked_add(file.size)
            .ok_or_else(|| anyhow::anyhow!("transfer size overflow"))?;
    }
    if calculated != total_bytes {
        bail!("transfer manifest size mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_transfer_manifest() {
        let files = vec![
            FileManifest {
                name: "one.txt".into(),
                size: 4,
            },
            FileManifest {
                name: "two.txt".into(),
                size: 6,
            },
        ];
        assert!(validate_manifest(&files, 10).is_ok());
        assert!(validate_manifest(&files, 9).is_err());

        let mut invalid = files;
        invalid[1].name = "../two.txt".into();
        assert!(validate_manifest(&invalid, 10).is_err());
    }
}
