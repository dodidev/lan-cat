use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ordering::VersionVector;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardEvent {
    pub id: Uuid,
    pub origin: String,
    pub sequence: u64,
    pub clock: VersionVector,
    pub text: String,
    pub digest: [u8; 32],
}

impl ClipboardEvent {
    pub fn new(origin: String, sequence: u64, clock: VersionVector, text: String) -> Result<Self> {
        validate_text(&text)?;
        Ok(Self {
            id: Uuid::new_v4(),
            origin,
            sequence,
            clock,
            digest: *blake3::hash(text.as_bytes()).as_bytes(),
            text,
        })
    }

    pub fn validate(&self) -> Result<()> {
        validate_text(&self.text)?;
        if self.digest != *blake3::hash(self.text.as_bytes()).as_bytes() {
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
    Ping,
}

pub fn validate_text(text: &str) -> Result<()> {
    if text.is_empty() {
        bail!("empty clipboard text is not synchronized");
    }
    if text.len() > MAX_TEXT_BYTES {
        bail!("clipboard text exceeds {MAX_TEXT_BYTES} bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_detects_tamper() {
        let mut clock = VersionVector::default();
        let seq = clock.increment("one");
        let mut event = ClipboardEvent::new("one".into(), seq, clock, "hello".into()).unwrap();
        assert!(event.validate().is_ok());
        event.text.push('!');
        assert!(event.validate().is_err());
    }

    #[test]
    fn text_limit_enforced() {
        assert!(validate_text("").is_err());
        assert!(validate_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }
}
