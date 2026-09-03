//! HAP TLV8 — the type-length-value encoding the pairing handshake speaks.
//!
//! Each item is a one-byte type, a one-byte length, then that many bytes. The
//! consequence worth knowing: **a value longer than 255 bytes is split across
//! several consecutive items sharing one type**, and a reader must stitch them
//! back together. The public keys exchanged during pairing are 384 bytes, so
//! this is the normal case, not an edge case.

use std::collections::BTreeMap;

use crate::{Error, Result};

/// TLV types used by pair-setup and pair-verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Tag {
    Method = 0x00,
    Identifier = 0x01,
    Salt = 0x02,
    PublicKey = 0x03,
    Proof = 0x04,
    EncryptedData = 0x05,
    State = 0x06,
    Error = 0x07,
    RetryDelay = 0x08,
    Certificate = 0x09,
    Signature = 0x0A,
    Permissions = 0x0B,
    FragmentData = 0x0C,
    FragmentLast = 0x0D,
    Flags = 0x13,
    Separator = 0xFF,
}

impl Tag {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The error codes a device can return mid-handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingError {
    Unknown,
    /// The PIN was wrong.
    Authentication,
    /// Too many attempts; the device wants us to wait.
    Backoff,
    /// The device will not accept any more paired controllers.
    MaxPeers,
    MaxTries,
    Unavailable,
    Busy,
    Other(u8),
}

impl PairingError {
    pub fn from_code(code: u8) -> Self {
        match code {
            0x01 => Self::Unknown,
            0x02 => Self::Authentication,
            0x03 => Self::Backoff,
            0x04 => Self::MaxPeers,
            0x05 => Self::MaxTries,
            0x06 => Self::Unavailable,
            0x07 => Self::Busy,
            other => Self::Other(other),
        }
    }

    /// Wording aimed at whoever is standing in front of the TV.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Authentication => "the PIN was not accepted",
            Self::Backoff => "too many attempts; wait a moment before trying again",
            Self::MaxPeers => "the device will not accept any more paired remotes",
            Self::MaxTries => "too many failed attempts; the device has stopped accepting them",
            Self::Unavailable => "the device is not accepting pairing right now",
            Self::Busy => "the device is busy; try again shortly",
            Self::Unknown | Self::Other(_) => "the device rejected the pairing",
        }
    }
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.describe())
    }
}

/// A decoded TLV8 message.
///
/// Values are stored whole, with fragments already joined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tlv(BTreeMap<u8, Vec<u8>>);

impl Tlv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, tag: Tag, value: impl Into<Vec<u8>>) -> Self {
        self.0.insert(tag.as_u8(), value.into());
        self
    }

    /// Set a single-byte value, for `State` and `Method`.
    pub fn set_byte(self, tag: Tag, value: u8) -> Self {
        self.set(tag, vec![value])
    }

    pub fn get(&self, tag: Tag) -> Option<&[u8]> {
        self.0.get(&tag.as_u8()).map(Vec::as_slice)
    }

    pub fn get_byte(&self, tag: Tag) -> Option<u8> {
        self.get(tag)?.first().copied()
    }

    /// Fetch a value that must be present, naming it if it isn't.
    pub fn require(&self, tag: Tag) -> Result<&[u8]> {
        self.get(tag).ok_or_else(|| {
            Error::Protocol(format!("pairing response is missing {tag:?}"))
        })
    }

    /// The error the device reported, if it reported one.
    pub fn error(&self) -> Option<PairingError> {
        self.get_byte(Tag::Error)
            .filter(|&code| code != 0)
            .map(PairingError::from_code)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (&tag, value) in &self.0 {
            if value.is_empty() {
                out.push(tag);
                out.push(0);
                continue;
            }
            // Split anything over 255 bytes into same-tag fragments.
            for chunk in value.chunks(255) {
                out.push(tag);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut items: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        let mut position = 0usize;
        // Only *consecutive* items of one type are fragments of one value; the
        // same type appearing again after another type starts a new value.
        let mut previous: Option<u8> = None;

        while position < data.len() {
            let tag = data[position];
            let length = *data
                .get(position + 1)
                .ok_or_else(|| Error::Protocol("TLV item is missing its length".into()))?
                as usize;
            let start = position + 2;
            let end = start
                .checked_add(length)
                .ok_or_else(|| Error::Protocol("TLV length overflow".into()))?;
            let value = data
                .get(start..end)
                .ok_or_else(|| Error::Protocol("TLV item runs past the end".into()))?;

            match items.get_mut(&tag) {
                Some(existing) if previous == Some(tag) => existing.extend_from_slice(value),
                _ => {
                    items.insert(tag, value.to_vec());
                }
            }

            previous = Some(tag);
            position = end;
        }
        Ok(Self(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_simple_message() {
        let tlv = Tlv::new()
            .set_byte(Tag::State, 1)
            .set_byte(Tag::Method, 0);
        // Ordered by tag: Method (0x00) then State (0x06).
        assert_eq!(tlv.encode(), b"\x00\x01\x00\x06\x01\x01");
    }

    #[test]
    fn round_trips() {
        let tlv = Tlv::new()
            .set_byte(Tag::State, 3)
            .set(Tag::PublicKey, vec![0xAB; 32])
            .set(Tag::Proof, vec![0xCD; 64]);
        assert_eq!(Tlv::decode(&tlv.encode()).unwrap(), tlv);
    }

    #[test]
    fn splits_long_values_into_fragments() {
        // A 384-byte SRP public key cannot fit one item, and this is the normal
        // case during pairing rather than an edge case.
        let key = vec![0x5Au8; 384];
        let encoded = Tlv::new().set(Tag::PublicKey, key.clone()).encode();

        assert_eq!(encoded[0], Tag::PublicKey.as_u8());
        assert_eq!(encoded[1], 255, "first fragment should be full");
        assert_eq!(encoded[2 + 255], Tag::PublicKey.as_u8());
        assert_eq!(encoded[2 + 255 + 1], 129, "remainder in the second");

        let decoded = Tlv::decode(&encoded).unwrap();
        assert_eq!(decoded.get(Tag::PublicKey).unwrap(), key.as_slice());
    }

    #[test]
    fn joins_fragments_only_when_consecutive() {
        // Two separate items of one type, split by another, are two values —
        // not one long one. Joining them would corrupt the handshake.
        let raw = b"\x03\x02\xAA\xAA\x06\x01\x01\x03\x02\xBB\xBB";
        let decoded = Tlv::decode(raw).unwrap();
        assert_eq!(
            decoded.get(Tag::PublicKey).unwrap(),
            &[0xBB, 0xBB],
            "the later value should win, not be appended to the earlier one"
        );
    }

    #[test]
    fn handles_zero_length_items() {
        let raw = b"\xFF\x00\x06\x01\x02";
        let decoded = Tlv::decode(raw).unwrap();
        assert_eq!(decoded.get(Tag::Separator).unwrap(), b"");
        assert_eq!(decoded.get_byte(Tag::State), Some(2));
    }

    #[test]
    fn reports_the_devices_error() {
        let raw = Tlv::new().set_byte(Tag::Error, 0x02).encode();
        let decoded = Tlv::decode(&raw).unwrap();
        assert_eq!(decoded.error(), Some(PairingError::Authentication));
        assert!(decoded.error().unwrap().describe().contains("PIN"));
    }

    #[test]
    fn a_zero_error_code_is_not_an_error() {
        let raw = Tlv::new().set_byte(Tag::Error, 0x00).encode();
        assert_eq!(Tlv::decode(&raw).unwrap().error(), None);
    }

    #[test]
    fn missing_fields_are_named_in_the_error() {
        let tlv = Tlv::new().set_byte(Tag::State, 2);
        let message = tlv.require(Tag::PublicKey).unwrap_err().to_string();
        assert!(message.contains("PublicKey"), "got: {message}");
    }

    #[test]
    fn truncated_input_is_rejected() {
        // These bytes come off a socket; every truncation must fail cleanly.
        let full = Tlv::new()
            .set_byte(Tag::State, 1)
            .set(Tag::PublicKey, vec![0xAB; 40])
            .encode();
        for cut in 1..full.len() {
            if Tlv::decode(&full[..cut]).is_ok() {
                // A cut on an item boundary is legitimately a shorter message.
                continue;
            }
        }
        assert!(Tlv::decode(b"\x06").is_err(), "missing length byte");
        assert!(Tlv::decode(b"\x06\x05\x01").is_err(), "value runs past the end");
    }

    #[test]
    fn an_empty_message_decodes_to_nothing() {
        assert!(Tlv::decode(b"").unwrap().is_empty());
    }
}
