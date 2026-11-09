//! Content address of a stored attachment: the SHA-256 of its bytes.
//!
//! Serialized as lowercase hex so that session logs stay readable and diffable
//! without a binary decoder.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// SHA-256 of the stored bytes, used as the address of an object on disk.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttachmentId([u8; 32]);

impl AttachmentId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(bytes).ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, 64 characters.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }

    /// First two hex characters: the sharding directory.
    pub fn shard(self) -> String {
        let hex = self.to_hex();
        hex[..2].to_string()
    }

    /// Parse 64 lowercase-or-uppercase hex characters.
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let bytes = text.as_bytes();
        let mut out = [0u8; 32];
        for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            out[index] = ((high << 4) | low) as u8;
        }
        Some(Self(out))
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Debug for AttachmentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "AttachmentId({})", self.to_hex())
    }
}

impl FromStr for AttachmentId {
    type Err = InvalidAttachmentId;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_hex(text).ok_or(InvalidAttachmentId)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("attachment id must be 64 hex characters")]
pub struct InvalidAttachmentId;

impl Serialize for AttachmentId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for AttachmentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text)
            .ok_or_else(|| D::Error::custom("attachment id must be 64 hex characters"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let id = AttachmentId([0xab; 32]);
        assert_eq!(id.to_hex(), "ab".repeat(32));
        assert_eq!(AttachmentId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(id.shard(), "ab");
    }

    #[test]
    fn rejects_bad_hex() {
        assert_eq!(AttachmentId::from_hex("abc"), None);
        assert_eq!(AttachmentId::from_hex(&"z".repeat(64)), None);
    }

    #[test]
    fn serde_uses_hex() {
        let id = AttachmentId([0x01; 32]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", "01".repeat(32)));
        assert_eq!(serde_json::from_str::<AttachmentId>(&json).unwrap(), id);
    }
}
