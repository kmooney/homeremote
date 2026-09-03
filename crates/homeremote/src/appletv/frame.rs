//! Companion framing and the OPACK message envelope.
//!
//! A frame is a one-byte type, a three-byte big-endian payload length, then the
//! payload. Once the session is encrypted the length counts the 16-byte
//! Poly1305 tag as well, and the four header bytes are the associated data — so
//! the header is authenticated, not merely descriptive.

use std::collections::BTreeMap;

use crate::appletv::opack::Value;
use crate::{Error, Result};

pub const HEADER_LEN: usize = 4;

/// The largest payload the three-byte length can describe.
pub const MAX_PAYLOAD: usize = 0xFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Unknown = 0,
    NoOp = 1,
    /// Pair-setup, first message only.
    PsStart = 3,
    /// Pair-setup, every message after the first — including the reply to it.
    PsNext = 4,
    /// Pair-verify, first message only.
    PvStart = 5,
    PvNext = 6,
    /// Unencrypted OPACK.
    UOpack = 7,
    /// Encrypted OPACK, which is everything after pair-verify.
    EOpack = 8,
    POpack = 9,
    PaReq = 10,
    PaRsp = 11,
    SessionStartRequest = 16,
    SessionStartResponse = 17,
    SessionData = 18,
    FamilyIdentityRequest = 32,
    FamilyIdentityResponse = 33,
    FamilyIdentityUpdate = 34,
}

impl FrameType {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::NoOp,
            3 => Self::PsStart,
            4 => Self::PsNext,
            5 => Self::PvStart,
            6 => Self::PvNext,
            7 => Self::UOpack,
            8 => Self::EOpack,
            9 => Self::POpack,
            10 => Self::PaReq,
            11 => Self::PaRsp,
            16 => Self::SessionStartRequest,
            17 => Self::SessionStartResponse,
            18 => Self::SessionData,
            32 => Self::FamilyIdentityRequest,
            33 => Self::FamilyIdentityResponse,
            34 => Self::FamilyIdentityUpdate,
            _ => Self::Unknown,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether frames of this type carry an OPACK body.
    pub fn is_opack(self) -> bool {
        matches!(
            self,
            Self::UOpack
                | Self::EOpack
                | Self::POpack
                | Self::PsStart
                | Self::PsNext
                | Self::PvStart
                | Self::PvNext
        )
    }

    /// Whether this is part of the pairing handshake, which is never encrypted.
    pub fn is_auth(self) -> bool {
        matches!(self, Self::PsStart | Self::PsNext | Self::PvStart | Self::PvNext)
    }
}

/// Build the four header bytes for a payload of `length`.
pub fn header(frame_type: FrameType, length: usize) -> Result<[u8; HEADER_LEN]> {
    if length > MAX_PAYLOAD {
        return Err(Error::Protocol(format!(
            "payload of {length} bytes does not fit a 3-byte length"
        )));
    }
    let len = length as u32;
    Ok([
        frame_type.as_u8(),
        (len >> 16) as u8,
        (len >> 8) as u8,
        len as u8,
    ])
}

/// Read the type and payload length out of a header.
pub fn parse_header(bytes: &[u8; HEADER_LEN]) -> (FrameType, usize) {
    let length =
        ((bytes[1] as usize) << 16) | ((bytes[2] as usize) << 8) | (bytes[3] as usize);
    (FrameType::from_u8(bytes[0]), length)
}

// ---------------------------------------------------------------------------
// Message envelope
// ---------------------------------------------------------------------------

/// Message type, carried as `_t`.
pub const REQUEST: i64 = 1;
pub const RESPONSE: i64 = 2;
pub const EVENT: i64 = 3;

/// Wrap a command in the envelope the device expects.
///
/// `_i` names the command, `_t` distinguishes request from response, `_c` is
/// the content, and `_x` correlates a reply with its request.
pub fn request(identifier: &str, xid: i64, content: Value) -> Value {
    let mut map = BTreeMap::new();
    map.insert("_i".to_string(), Value::Str(identifier.to_string()));
    map.insert("_t".to_string(), Value::Int(REQUEST));
    map.insert("_c".to_string(), content);
    map.insert("_x".to_string(), Value::Int(xid));
    Value::Dict(map)
}

/// Wrap pairing data, which uses `_pd` rather than the command envelope.
pub fn pairing_message(pairing_data: Vec<u8>, extra: &[(&str, Value)]) -> Value {
    let mut map = BTreeMap::new();
    map.insert("_pd".to_string(), Value::Bytes(pairing_data));
    for (key, value) in extra {
        map.insert((*key).to_string(), value.clone());
    }
    Value::Dict(map)
}

/// Pull the `_pd` blob out of a pairing reply.
pub fn pairing_data(message: &Value) -> Result<&[u8]> {
    message
        .get("_pd")
        .and_then(Value::as_bytes)
        .ok_or_else(|| Error::Protocol("reply carried no pairing data".into()))
}

/// The device reports failures as `_em`; surface that rather than a timeout.
pub fn error_message(message: &Value) -> Option<String> {
    match message.get("_em") {
        Some(Value::Str(text)) => Some(text.clone()),
        Some(other) => Some(format!("{other:?}")),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        for (frame_type, length) in [
            (FrameType::PsStart, 0usize),
            (FrameType::EOpack, 1),
            (FrameType::PvNext, 255),
            (FrameType::UOpack, 256),
            (FrameType::EOpack, 0xFF_FFFF),
        ] {
            let bytes = header(frame_type, length).unwrap();
            assert_eq!(parse_header(&bytes), (frame_type, length));
        }
    }

    #[test]
    fn length_is_big_endian() {
        // Getting this backwards produces frames the device silently drops.
        assert_eq!(header(FrameType::EOpack, 0x010203).unwrap(), [8, 1, 2, 3]);
    }

    #[test]
    fn an_oversized_payload_is_rejected() {
        assert!(header(FrameType::EOpack, MAX_PAYLOAD + 1).is_err());
    }

    #[test]
    fn unknown_frame_types_decode_rather_than_fail() {
        // The device sends types we don't model; they must not kill the link.
        assert_eq!(FrameType::from_u8(200), FrameType::Unknown);
        assert_eq!(parse_header(&[200, 0, 0, 5]), (FrameType::Unknown, 5));
    }

    #[test]
    fn frame_type_classification() {
        assert!(FrameType::PsStart.is_auth() && FrameType::PsStart.is_opack());
        assert!(FrameType::PvNext.is_auth());
        assert!(FrameType::EOpack.is_opack() && !FrameType::EOpack.is_auth());
        assert!(!FrameType::SessionData.is_opack());
    }

    #[test]
    fn request_carries_the_envelope_fields() {
        let message = request("_hidC", 7, crate::appletv::opack::dict([("_hidC", Value::Int(6))]));
        assert_eq!(message.get("_i").unwrap().as_str(), Some("_hidC"));
        assert_eq!(message.get("_t").unwrap().as_int(), Some(REQUEST));
        assert_eq!(message.get("_x").unwrap().as_int(), Some(7));
        assert_eq!(
            message.get("_c").unwrap().get("_hidC").unwrap().as_int(),
            Some(6)
        );
    }

    #[test]
    fn pairing_messages_carry_pd_and_extras() {
        let message = pairing_message(vec![1, 2, 3], &[("_pwTy", Value::Int(1))]);
        assert_eq!(pairing_data(&message).unwrap(), &[1, 2, 3]);
        assert_eq!(message.get("_pwTy").unwrap().as_int(), Some(1));
    }

    #[test]
    fn missing_pairing_data_is_an_error() {
        assert!(pairing_data(&crate::appletv::opack::dict([("_x", Value::Int(1))])).is_err());
    }

    #[test]
    fn device_errors_are_surfaced() {
        let message = crate::appletv::opack::dict([("_em", Value::from("bad request"))]);
        assert_eq!(error_message(&message).as_deref(), Some("bad request"));
        assert_eq!(error_message(&crate::appletv::opack::dict([("_x", Value::Int(1))])), None);
    }
}
