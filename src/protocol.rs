use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ordering::VersionVector;

pub const PROTOCOL_VERSION: u16 = 4;
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FILES: usize = 64;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipboardFile {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipboardPayload {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<String>,
    pub png: Option<Vec<u8>>,
    #[serde(default)]
    pub files: Vec<ClipboardFile>,
}

impl ClipboardPayload {
    #[cfg(test)]
    pub fn text(text: String) -> Self {
        Self {
            text: Some(text),
            ..Self::default()
        }
    }

    pub fn total_bytes(&self) -> usize {
        self.text.as_ref().map_or(0, |value| value.len())
            + self.html.as_ref().map_or(0, |value| value.len())
            + self.rtf.as_ref().map_or(0, |value| value.len())
            + self.png.as_ref().map_or(0, Vec::len)
            + self
                .files
                .iter()
                .map(|file| file.name.len() + file.data.len())
                .sum::<usize>()
    }

    pub fn validate(&self) -> Result<()> {
        if self.text.is_none()
            && self.html.is_none()
            && self.rtf.is_none()
            && self.png.is_none()
            && self.files.is_empty()
        {
            bail!("empty clipboard payload is not synchronized");
        }
        if !self.files.is_empty()
            && (self.text.is_some()
                || self.html.is_some()
                || self.rtf.is_some()
                || self.png.is_some())
        {
            bail!("file clipboard data cannot be mixed with other formats");
        }
        if self.total_bytes() > MAX_PAYLOAD_BYTES {
            bail!("clipboard payload exceeds {MAX_PAYLOAD_BYTES} bytes");
        }
        if self.files.len() > MAX_FILES {
            bail!("clipboard payload exceeds {MAX_FILES} files");
        }
        let mut names = std::collections::HashSet::new();
        for file in &self.files {
            if file.name.is_empty()
                || file.name == "."
                || file.name == ".."
                || file.name.contains(['/', '\\', '\0'])
            {
                bail!("invalid clipboard filename");
            }
            if !names.insert(&file.name) {
                bail!("duplicate clipboard filename {}", file.name);
            }
        }
        for (name, value) in [
            ("text/plain", self.text.as_deref()),
            ("text/html", self.html.as_deref()),
            ("text/rtf", self.rtf.as_deref()),
        ] {
            if value == Some("") {
                bail!("{name} clipboard data is empty");
            }
        }
        if let Some(png) = &self.png {
            if png.len() < PNG_SIGNATURE.len() || &png[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
                bail!("image/png clipboard data has invalid signature");
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        digest_part(
            &mut hasher,
            b"text/plain",
            self.text.as_ref().map(|v| v.as_bytes()),
        );
        digest_part(
            &mut hasher,
            b"text/html",
            self.html.as_ref().map(|v| v.as_bytes()),
        );
        digest_part(
            &mut hasher,
            b"text/rtf",
            self.rtf.as_ref().map(|v| v.as_bytes()),
        );
        digest_part(&mut hasher, b"image/png", self.png.as_deref());
        hasher.update(b"files");
        hasher.update(&(self.files.len() as u64).to_be_bytes());
        for file in &self.files {
            hasher.update(&(file.name.len() as u64).to_be_bytes());
            hasher.update(file.name.as_bytes());
            hasher.update(&(file.data.len() as u64).to_be_bytes());
            hasher.update(&file.data);
        }
        *hasher.finalize().as_bytes()
    }
}

fn digest_part(hasher: &mut blake3::Hasher, tag: &[u8], bytes: Option<&[u8]>) {
    hasher.update(&(tag.len() as u16).to_be_bytes());
    hasher.update(tag);
    match bytes {
        Some(bytes) => {
            hasher.update(&[1]);
            hasher.update(&(bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        }
        None => {
            hasher.update(&[0]);
        }
    };
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardEvent {
    pub id: Uuid,
    pub origin: String,
    pub sequence: u64,
    pub clock: VersionVector,
    pub payload: ClipboardPayload,
    pub digest: [u8; 32],
}

impl ClipboardEvent {
    pub fn new(
        origin: String,
        sequence: u64,
        clock: VersionVector,
        payload: ClipboardPayload,
    ) -> Result<Self> {
        payload.validate()?;
        Ok(Self {
            id: Uuid::new_v4(),
            origin,
            sequence,
            clock,
            digest: payload.digest(),
            payload,
        })
    }

    pub fn validate(&self) -> Result<()> {
        self.payload.validate()?;
        if self.digest != self.payload.digest() {
            bail!("clipboard digest mismatch");
        }
        if self.clock.0.get(&self.origin).copied() != Some(self.sequence) {
            bail!("clipboard sequence does not match version vector");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message {
    Hello { version: u16, device_id: String },
    Clipboard(ClipboardEvent),
    Transfer(crate::transfer::protocol::TransferMessage),
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(b"payload");
        bytes
    }

    #[test]
    fn payload_validation_accepts_supported_formats() {
        let payload = ClipboardPayload {
            text: Some("hello".into()),
            html: Some("<b>hello</b>".into()),
            rtf: Some(r"{\rtf1 hello}".into()),
            png: Some(png()),
            files: Vec::new(),
        };
        assert!(payload.validate().is_ok());
    }

    #[test]
    fn file_payload_validation_and_digest() {
        let payload = ClipboardPayload {
            files: vec![ClipboardFile {
                name: "report.txt".into(),
                data: b"contents".to_vec(),
            }],
            ..Default::default()
        };
        assert!(payload.validate().is_ok());

        let mut renamed = payload.clone();
        renamed.files[0].name = "other.txt".into();
        assert_ne!(payload.digest(), renamed.digest());

        let mut traversal = payload.clone();
        traversal.files[0].name = "../report.txt".into();
        assert!(traversal.validate().is_err());

        let mixed = ClipboardPayload {
            text: Some("text".into()),
            ..payload
        };
        assert!(mixed.validate().is_err());
    }

    #[test]
    fn payload_validation_rejects_empty_large_and_invalid_png() {
        assert!(ClipboardPayload::default().validate().is_err());
        assert!(
            ClipboardPayload::text("x".repeat(MAX_PAYLOAD_BYTES + 1))
                .validate()
                .is_err()
        );
        assert!(
            ClipboardPayload {
                png: Some(b"not png".to_vec()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn event_detects_payload_tamper() {
        let mut clock = VersionVector::default();
        let seq = clock.increment("one");
        let mut event = ClipboardEvent::new(
            "one".into(),
            seq,
            clock,
            ClipboardPayload::text("hello".into()),
        )
        .unwrap();
        assert!(event.validate().is_ok());
        event.payload.text = Some("hello!".into());
        assert!(event.validate().is_err());
    }

    #[test]
    fn digest_covers_format_tags_and_bytes() {
        let text = ClipboardPayload::text("same".into()).digest();
        let html = ClipboardPayload {
            html: Some("same".into()),
            ..Default::default()
        }
        .digest();
        assert_ne!(text, html);
    }
}
