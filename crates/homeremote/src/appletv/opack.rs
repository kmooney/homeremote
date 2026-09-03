//! OPACK — Apple's undocumented binary serialisation format.
//!
//! Every Companion message is an OPACK-encoded dictionary. There is no
//! specification; the format below is reconstructed from
//! [pyatv](https://github.com/postlund/pyatv)'s `opack.py`, which reverse
//! engineered it.
//!
//! The tag byte carries both the type and, for short values, the length:
//!
//! | Tag         | Meaning                                    |
//! |-------------|--------------------------------------------|
//! | `0x01`/`0x02` | true / false                             |
//! | `0x04`      | null                                       |
//! | `0x05`      | UUID, 16 bytes                             |
//! | `0x06`      | float64, little endian                     |
//! | `0x08`–`0x2F` | integer 0–39, encoded in the tag itself   |
//! | `0x30`–`0x33` | integer in 1, 2, 4 or 8 following bytes   |
//! | `0x40`–`0x60` | string, length in the tag                 |
//! | `0x61`–`0x64` | string, length in 1–4 following bytes     |
//! | `0x70`–`0x90` | bytes, length in the tag                  |
//! | `0x91`–`0x94` | bytes, length in 1–4 following bytes      |
//! | `0xA0`–`0xC2` | back-reference to an earlier value        |
//! | `0xD0`–`0xDE` | array, count in the tag                   |
//! | `0xDF`      | array, terminated by `0x03`                |
//! | `0xE0`–`0xEE` | dictionary, pair count in the tag         |
//! | `0xEF`      | dictionary, terminated by `0x03`           |
//!
//! The back-references are the subtle part: strings and byte strings are added
//! to a table as they are parsed, and later occurrences are encoded as an index
//! into it. A decoder that ignores them fails on almost every real message,
//! because Apple repeats key names constantly.

use std::collections::BTreeMap;

use crate::{Error, Result};

/// A decoded OPACK value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
    Array(Vec<Value>),
    /// Ordered so encoding is deterministic and tests can compare bytes.
    Dict(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Look up a key, for the dictionaries every Companion message is.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(map) => map.get(key),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(b)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

/// Build a `Value::Dict` from key/value pairs.
pub fn dict<K: Into<String>, V: Into<Value>>(pairs: impl IntoIterator<Item = (K, V)>) -> Value {
    Value::Dict(
        pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
    )
}

fn bad(message: impl Into<String>) -> Error {
    Error::Protocol(format!("OPACK: {}", message.into()))
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encode a value as OPACK.
///
/// Back-references are emitted for repeated strings and byte strings, matching
/// what the Apple TV itself produces.
pub fn encode(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut seen: Vec<Value> = Vec::new();
    encode_value(value, &mut out, &mut seen)?;
    Ok(out)
}

fn encode_value(value: &Value, out: &mut Vec<u8>, seen: &mut Vec<Value>) -> Result<()> {
    // Strings and byte strings participate in the back-reference table.
    if matches!(value, Value::Str(_) | Value::Bytes(_))
        && let Some(index) = seen.iter().position(|v| v == value)
    {
        if index > 0x22 {
            // Beyond the single-byte pointer range; emit the value again.
            return encode_literal(value, out, seen);
        }
        out.push(0xA0 + index as u8);
        return Ok(());
    }
    encode_literal(value, out, seen)
}

fn encode_literal(value: &Value, out: &mut Vec<u8>, seen: &mut Vec<Value>) -> Result<()> {
    match value {
        Value::Null => out.push(0x04),
        Value::Bool(true) => out.push(0x01),
        Value::Bool(false) => out.push(0x02),

        Value::Uuid(bytes) => {
            out.push(0x05);
            out.extend_from_slice(bytes);
        }

        Value::Float(f) => {
            out.push(0x06);
            out.extend_from_slice(&f.to_le_bytes());
        }

        Value::Int(i) => {
            let i = *i;
            if (0..0x28).contains(&i) {
                // Small integers live entirely in the tag byte.
                out.push(0x08 + i as u8);
            } else if (0..=0xFF).contains(&i) {
                out.push(0x30);
                out.push(i as u8);
            } else if (0..=0xFFFF).contains(&i) {
                out.push(0x31);
                out.extend_from_slice(&(i as u16).to_le_bytes());
            } else if (0..=0xFFFF_FFFF).contains(&i) {
                out.push(0x32);
                out.extend_from_slice(&(i as u32).to_le_bytes());
            } else {
                out.push(0x33);
                out.extend_from_slice(&i.to_le_bytes());
            }
        }

        Value::Str(s) => {
            let bytes = s.as_bytes();
            let len = bytes.len();
            if len <= 0x20 {
                out.push(0x40 + len as u8);
            } else {
                push_sized(out, 0x60, len)?;
            }
            out.extend_from_slice(bytes);
            seen.push(value.clone());
        }

        Value::Bytes(b) => {
            let len = b.len();
            if len <= 0x20 {
                out.push(0x70 + len as u8);
            } else {
                push_sized(out, 0x90, len)?;
            }
            out.extend_from_slice(b);
            seen.push(value.clone());
        }

        Value::Array(items) => {
            if items.len() < 0xF {
                out.push(0xD0 + items.len() as u8);
                for item in items {
                    encode_value(item, out, seen)?;
                }
            } else {
                out.push(0xDF);
                for item in items {
                    encode_value(item, out, seen)?;
                }
                out.push(0x03);
            }
        }

        Value::Dict(map) => {
            if map.len() < 0xF {
                out.push(0xE0 + map.len() as u8);
                for (key, item) in map {
                    encode_value(&Value::Str(key.clone()), out, seen)?;
                    encode_value(item, out, seen)?;
                }
            } else {
                out.push(0xEF);
                for (key, item) in map {
                    encode_value(&Value::Str(key.clone()), out, seen)?;
                    encode_value(item, out, seen)?;
                }
                out.push(0x03);
            }
        }
    }
    Ok(())
}

/// Emit a length-prefixed tag: `base + n` where the length occupies `n` bytes.
fn push_sized(out: &mut Vec<u8>, base: u8, len: usize) -> Result<()> {
    if len <= 0xFF {
        out.push(base + 1);
        out.push(len as u8);
    } else if len <= 0xFFFF {
        out.push(base + 2);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    } else if len <= 0xFF_FFFF {
        out.push(base + 3);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
    } else if len <= 0xFFFF_FFFF {
        out.push(base + 4);
        out.extend_from_slice(&(len as u32).to_le_bytes());
    } else {
        return Err(bad("value is too long to encode"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode one OPACK value, ignoring any trailing bytes.
pub fn decode(data: &[u8]) -> Result<Value> {
    let mut cursor = Cursor {
        data,
        position: 0,
        seen: Vec::new(),
    };
    cursor.value()
}

struct Cursor<'a> {
    data: &'a [u8],
    position: usize,
    /// Values eligible as back-reference targets, in the order encountered.
    seen: Vec<Value>,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.position)
            .ok_or_else(|| bad("ran off the end of the message"))?;
        self.position += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| bad("length overflow"))?;
        let slice = self
            .data
            .get(self.position..end)
            .ok_or_else(|| bad(format!("wanted {len} bytes, message is too short")))?;
        self.position = end;
        Ok(slice)
    }

    /// Read a little-endian length of `size` bytes.
    fn length(&mut self, size: usize) -> Result<usize> {
        let bytes = self.take(size)?;
        let mut buf = [0u8; 8];
        buf[..size].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(buf) as usize)
    }

    fn value(&mut self) -> Result<Value> {
        let tag = self.byte()?;
        match tag {
            0x01 => Ok(Value::Bool(true)),
            0x02 => Ok(Value::Bool(false)),
            0x04 => Ok(Value::Null),

            0x05 => {
                let bytes = self.take(16)?;
                let mut uuid = [0u8; 16];
                uuid.copy_from_slice(bytes);
                Ok(Value::Uuid(uuid))
            }

            0x06 => {
                let bytes = self.take(8)?;
                Ok(Value::Float(f64::from_le_bytes(bytes.try_into().unwrap())))
            }

            // Small integers carried in the tag.
            0x08..=0x2F => Ok(Value::Int((tag - 0x08) as i64)),

            // Integers of 1, 2, 4 or 8 bytes.
            0x30..=0x33 => {
                let size = 1usize << (tag - 0x30);
                let bytes = self.take(size)?;
                let mut buf = [0u8; 8];
                buf[..size].copy_from_slice(bytes);
                Ok(Value::Int(i64::from_le_bytes(buf)))
            }

            0x40..=0x60 => {
                let len = (tag - 0x40) as usize;
                self.string(len)
            }
            0x61..=0x64 => {
                let len = self.length((tag - 0x60) as usize)?;
                self.string(len)
            }

            0x70..=0x90 => {
                let len = (tag - 0x70) as usize;
                self.byte_string(len)
            }
            0x91..=0x94 => {
                let len = self.length((tag - 0x90) as usize)?;
                self.byte_string(len)
            }

            // A back-reference to a value seen earlier in this message.
            0xA0..=0xC2 => {
                let index = (tag - 0xA0) as usize;
                self.seen
                    .get(index)
                    .cloned()
                    .ok_or_else(|| bad(format!("back-reference {index} points at nothing")))
            }

            0xD0..=0xDE => {
                let count = (tag - 0xD0) as usize;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.value()?);
                }
                Ok(Value::Array(items))
            }
            0xDF => {
                let mut items = Vec::new();
                while !self.at_terminator()? {
                    items.push(self.value()?);
                }
                self.position += 1; // consume the 0x03
                Ok(Value::Array(items))
            }

            0xE0..=0xEE => {
                let count = (tag - 0xE0) as usize;
                let mut map = BTreeMap::new();
                for _ in 0..count {
                    let (key, value) = self.pair()?;
                    map.insert(key, value);
                }
                Ok(Value::Dict(map))
            }
            0xEF => {
                let mut map = BTreeMap::new();
                while !self.at_terminator()? {
                    let (key, value) = self.pair()?;
                    map.insert(key, value);
                }
                self.position += 1; // consume the 0x03
                Ok(Value::Dict(map))
            }

            other => Err(bad(format!("unknown tag 0x{other:02x}"))),
        }
    }

    fn pair(&mut self) -> Result<(String, Value)> {
        let key = self.value()?;
        let value = self.value()?;
        match key {
            Value::Str(key) => Ok((key, value)),
            // Apple only ever uses string keys; anything else means we've lost
            // sync with the stream rather than found an exotic message.
            other => Err(bad(format!("dictionary key is not a string: {other:?}"))),
        }
    }

    fn at_terminator(&mut self) -> Result<bool> {
        match self.data.get(self.position) {
            Some(0x03) => Ok(true),
            Some(_) => Ok(false),
            None => Err(bad("unterminated array or dictionary")),
        }
    }

    fn string(&mut self, len: usize) -> Result<Value> {
        let bytes = self.take(len)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| bad("string is not valid UTF-8"))?
            .to_string();
        let value = Value::Str(text);
        self.seen.push(value.clone());
        Ok(value)
    }

    fn byte_string(&mut self, len: usize) -> Result<Value> {
        let value = Value::Bytes(self.take(len)?.to_vec());
        self.seen.push(value.clone());
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: Value) -> Vec<u8> {
        encode(&v).expect("should encode")
    }

    // -- Encoding, against vectors from pyatv's opack tests -----------------

    #[test]
    fn encodes_singletons() {
        assert_eq!(enc(Value::Bool(true)), b"\x01");
        assert_eq!(enc(Value::Bool(false)), b"\x02");
        assert_eq!(enc(Value::Null), b"\x04");
    }

    #[test]
    fn encodes_uuid() {
        let uuid = [
            0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x12, 0x34,
            0x56, 0x78,
        ];
        assert_eq!(enc(Value::Uuid(uuid)), [b"\x05".as_slice(), &uuid].concat());
    }

    #[test]
    fn small_integers_live_in_the_tag_byte() {
        assert_eq!(enc(Value::Int(0)), b"\x08");
        assert_eq!(enc(Value::Int(0xF)), b"\x17");
        assert_eq!(enc(Value::Int(0x27)), b"\x2F");
    }

    #[test]
    fn larger_integers_take_a_length_tag() {
        // 0x28 is the first value that no longer fits in the tag.
        assert_eq!(enc(Value::Int(0x28)), b"\x30\x28");
        assert_eq!(enc(Value::Int(0x1FF)), b"\x31\xFF\x01");
        assert_eq!(enc(Value::Int(0x1FF_FFFF)), b"\x32\xFF\xFF\xFF\x01");
        assert_eq!(
            enc(Value::Int(0x1FF_FFFF_FFFF_FFFF)),
            b"\x33\xFF\xFF\xFF\xFF\xFF\xFF\xFF\x01"
        );
    }

    #[test]
    fn encodes_float64_little_endian() {
        assert_eq!(enc(Value::Float(1.0)), b"\x06\x00\x00\x00\x00\x00\x00\xF0\x3F");
    }

    #[test]
    fn encodes_strings_by_length_class() {
        assert_eq!(enc(Value::from("a")), b"\x41a");
        assert_eq!(enc(Value::from("abc")), b"\x43abc");
        // 64 bytes is past the in-tag limit, so a 1-byte length follows.
        let long = "a".repeat(64);
        assert_eq!(enc(Value::from(long.clone())), [b"\x61\x40".as_slice(), long.as_bytes()].concat());
        // 256 needs two.
        let longer = "a".repeat(256);
        assert_eq!(
            enc(Value::from(longer.clone())),
            [b"\x62\x00\x01".as_slice(), longer.as_bytes()].concat()
        );
    }

    #[test]
    fn encodes_byte_strings() {
        assert_eq!(enc(Value::Bytes(vec![0xAC])), b"\x71\xAC");
        let long = vec![0xACu8; 256];
        assert_eq!(
            enc(Value::Bytes(long.clone())),
            [b"\x92\x00\x01".as_slice(), &long].concat()
        );
    }

    #[test]
    fn encodes_arrays() {
        let array = Value::Array(vec![Value::Int(1), Value::from("test"), Value::Bool(false)]);
        assert_eq!(enc(array), b"\xD3\x09\x44test\x02");
    }

    #[test]
    fn encodes_dictionaries() {
        assert_eq!(enc(dict([("a", Value::Int(12))])), b"\xE1\x41\x61\x14");
    }

    #[test]
    fn repeated_values_become_back_references() {
        // Apple repeats key names constantly; without pointers our output
        // would not match what the device produces.
        assert_eq!(
            enc(Value::Array(vec![Value::from("a"), Value::from("a")])),
            b"\xD2\x41\x61\xA0"
        );
        assert_eq!(
            enc(Value::Array(vec![
                Value::from("foo"),
                Value::from("bar"),
                Value::from("foo"),
                Value::from("bar"),
            ])),
            b"\xD4\x43foo\x43bar\xA0\xA1"
        );
    }

    // -- Decoding ----------------------------------------------------------

    #[test]
    fn decodes_what_it_encodes() {
        let cases = vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(0x27),
            Value::Int(0x28),
            Value::Int(0x1FF),
            Value::Int(0x1FF_FFFF),
            Value::Int(0x1FF_FFFF_FFFF_FFFF),
            Value::Float(1.0),
            Value::Float(-273.15),
            Value::Uuid([7u8; 16]),
            Value::from("a"),
            Value::from("a".repeat(64)),
            Value::from("a".repeat(300)),
            Value::Bytes(vec![0xAC]),
            Value::Bytes(vec![0xACu8; 300]),
            Value::Array(vec![Value::Int(1), Value::from("test"), Value::Bool(false)]),
            dict([("a", Value::Int(12))]),
        ];
        for case in cases {
            let bytes = encode(&case).expect("encode");
            assert_eq!(decode(&bytes).expect("decode"), case, "round trip failed");
        }
    }

    #[test]
    fn decodes_back_references() {
        let value = Value::Array(vec![
            Value::from("foo"),
            Value::from("bar"),
            Value::from("foo"),
        ]);
        let bytes = encode(&value).expect("encode");
        assert!(bytes.contains(&0xA0), "should have emitted a pointer");
        assert_eq!(decode(&bytes).expect("decode"), value);
    }

    #[test]
    fn decodes_length_terminated_collections() {
        // 0xDF / 0xEF forms end with 0x03 instead of carrying a count. We only
        // emit them for big collections, but the device uses them freely.
        assert_eq!(
            decode(b"\xDF\x08\x09\x03").unwrap(),
            Value::Array(vec![Value::Int(0), Value::Int(1)])
        );
        assert_eq!(
            decode(b"\xEF\x41\x61\x14\x03").unwrap(),
            dict([("a", Value::Int(12))])
        );
    }

    #[test]
    fn decodes_a_large_collection() {
        // 15 or more entries cannot use the in-tag count.
        let items: Vec<Value> = (0..20).map(Value::Int).collect();
        let value = Value::Array(items.clone());
        let bytes = encode(&value).unwrap();
        assert_eq!(bytes[0], 0xDF, "should use the terminated form");
        assert_eq!(decode(&bytes).unwrap(), value);

        let big = dict((0..20).map(|i| (format!("k{i:02}"), Value::Int(i))));
        let bytes = encode(&big).unwrap();
        assert_eq!(bytes[0], 0xEF);
        assert_eq!(decode(&bytes).unwrap(), big);
    }

    #[test]
    fn nested_structures_survive() {
        let value = dict([
            ("_i", Value::from("_systemInfo")),
            ("_x", Value::Int(1234)),
            (
                "_c",
                dict([
                    ("_pubID", Value::from("aa:bb:cc:dd:ee:ff")),
                    ("_flags", Value::Int(1)),
                    ("nested", Value::Array(vec![Value::from("_i"), Value::Null])),
                ]),
            ),
        ]);
        let bytes = encode(&value).unwrap();
        assert_eq!(decode(&bytes).unwrap(), value);
    }

    // -- Malformed input ---------------------------------------------------

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        // Everything here arrives over a socket from a device we don't control.
        let full = encode(&dict([("key", Value::from("value"))])).unwrap();
        for cut in 1..full.len() {
            let result = decode(&full[..cut]);
            assert!(result.is_err(), "truncation at {cut} should fail cleanly");
        }
    }

    #[test]
    fn unknown_tags_are_rejected() {
        assert!(decode(b"\x00").is_err());
        assert!(decode(b"\xFF").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn a_dangling_back_reference_is_rejected() {
        // Pointing past the table would otherwise index out of bounds.
        assert!(decode(b"\xA5").is_err());
        assert!(decode(b"\xD1\xA0").is_err());
    }

    #[test]
    fn invalid_utf8_in_a_string_is_rejected() {
        assert!(decode(b"\x42\xFF\xFE").is_err());
    }

    #[test]
    fn unterminated_collections_are_rejected() {
        assert!(decode(b"\xDF\x08").is_err());
        assert!(decode(b"\xEF\x41\x61\x14").is_err());
    }

    #[test]
    fn non_string_dictionary_keys_are_rejected() {
        // Losing sync with the stream shows up here first.
        assert!(decode(b"\xE1\x08\x08").is_err());
    }

    #[test]
    fn a_huge_declared_length_does_not_allocate() {
        // 0x64 declares a 4-byte length; claiming 4GB must fail on the bounds
        // check rather than trying to reserve it.
        assert!(decode(b"\x64\xFF\xFF\xFF\xFF").is_err());
        assert!(decode(b"\x94\xFF\xFF\xFF\xFF").is_err());
    }
}
