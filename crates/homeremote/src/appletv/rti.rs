//! Text input ("RTI") payloads.
//!
//! When a text field takes focus, the Apple TV describes it in `_tiD`: a binary
//! plist laid out by NSKeyedArchiver, where objects live in a flat `$objects`
//! table and refer to each other by UID. Typing goes back the same way, as an
//! archived `RTITextOperations`.
//!
//! There is no general archiver here, only the two shapes this needs: reading
//! the field's session id and text, and writing the operations that clear it
//! and insert text. The layouts match pyatv's, which match what the device
//! accepts.

use std::io::Cursor;

use plist::{Dictionary, Uid, Value};

use crate::{Error, Result};

/// The text field focused on the Apple TV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextField {
    /// Identifies this focus of this field. Operations must name it, and the
    /// device issues a new one each time a field takes focus.
    pub session: Vec<u8>,
    /// What the field holds before the cursor, which in practice is all of it.
    pub text: String,
}

/// Read the focused field out of a `_tiD` archive.
pub fn read_field(archive: &[u8]) -> Result<TextField> {
    let root = Value::from_reader(Cursor::new(archive))
        .map_err(|e| Error::Protocol(format!("unreadable text-input archive: {e}")))?;
    let objects = root
        .as_dictionary()
        .and_then(|d| d.get("$objects"))
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("text-input archive has no object table".into()))?;
    let top = root
        .as_dictionary()
        .and_then(|d| d.get("$top"))
        .ok_or_else(|| Error::Protocol("text-input archive has no root object".into()))?;

    let lookup = |from, key| lookup(objects, from, key);

    // Seen both as raw bytes and as an archived NSUUID wrapping them.
    let session = match lookup(Some(top), "sessionUUID") {
        Some(Value::Data(bytes)) => bytes.clone(),
        uuid @ Some(Value::Dictionary(_)) => lookup(uuid, "NS.uuidbytes")
            .and_then(Value::as_data)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::Protocol("text-input session id has no bytes".into()))?,
        _ => return Err(Error::Protocol("text-input archive names no session".into())),
    };

    let document = lookup(Some(top), "documentState");
    let text = lookup(lookup(document, "docSt"), "contextBeforeInput")
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_string();

    Ok(TextField { session, text })
}

/// Read `key` from an archived object, following it through the object table.
fn lookup<'a>(objects: &'a [Value], from: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match from?.as_dictionary()?.get(key)? {
        // UID 0 is the archiver's "$null".
        Value::Uid(uid) if uid.get() == 0 => None,
        Value::Uid(uid) => objects.get(uid.get() as usize),
        inline => Some(inline),
    }
}

/// Operations that empty the field.
pub fn clear(session: &[u8]) -> Result<Vec<u8>> {
    archive_operations(session, Operation::Clear)
}

/// Operations that type `text` at the cursor.
pub fn insert(session: &[u8], text: &str) -> Result<Vec<u8>> {
    edit(session, 0, text)
}

/// Operations that delete `deletions` characters before the cursor, as
/// backspace does, then type `insertion`.
pub fn edit(session: &[u8], deletions: usize, insertion: &str) -> Result<Vec<u8>> {
    archive_operations(session, Operation::Edit { deletions, insertion })
}

enum Operation<'a> {
    Clear,
    Edit { deletions: usize, insertion: &'a str },
}

/// Archive an `RTITextOperations` aimed at `session`.
fn archive_operations(session: &[u8], operation: Operation) -> Result<Vec<u8>> {
    // The object table, by UID: 0 "$null", 1 the operations, 2 the keyboard
    // output, 3 its class, 4 the session id, 5 its class, 6 the operations'
    // class, 7 the text.
    let mut operations = Dictionary::new();
    operations.insert("$class".into(), uid(6));
    operations.insert("targetSessionUUID".into(), uid(4));
    operations.insert("keyboardOutput".into(), uid(2));
    let mut output = Dictionary::new();
    output.insert("$class".into(), uid(3));

    let text = match operation {
        // A clear is expressed as asserting that the field holds nothing.
        Operation::Clear => {
            operations.insert("textToAssert".into(), uid(7));
            ""
        }
        Operation::Edit { deletions, insertion } => {
            if deletions > 0 {
                output.insert("deletionCount".into(), Value::Integer((deletions as u64).into()));
            }
            if !insertion.is_empty() {
                output.insert("insertionText".into(), uid(7));
            }
            insertion
        }
    };

    let objects = vec![
        Value::String("$null".into()),
        Value::Dictionary(operations),
        Value::Dictionary(output),
        class("TIKeyboardOutput"),
        dictionary([("NS.uuidbytes", Value::Data(session.to_vec())), ("$class", uid(5))]),
        class("NSUUID"),
        class("RTITextOperations"),
        Value::String(text.to_string()),
    ];

    let archive = dictionary([
        ("$version", Value::Integer(100_000.into())),
        ("$archiver", Value::String("RTIKeyedArchiver".into())),
        ("$top", dictionary([("textOperations", uid(1))])),
        ("$objects", Value::Array(objects)),
    ]);

    let mut out = Vec::new();
    archive
        .to_writer_binary(&mut out)
        .map_err(|e| Error::Protocol(format!("could not archive text input: {e}")))?;
    Ok(out)
}

fn uid(index: u64) -> Value {
    Value::Uid(Uid::new(index))
}

fn class(name: &str) -> Value {
    dictionary([
        ("$classname", Value::String(name.into())),
        (
            "$classes",
            Value::Array(vec![Value::String(name.into()), Value::String("NSObject".into())]),
        ),
    ])
}

fn dictionary<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut d = Dictionary::new();
    for (key, value) in pairs {
        d.insert(key.to_string(), value);
    }
    Value::Dictionary(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: [u8; 16] = *b"0123456789abcdef";

    /// An archive shaped like the one the device sends with `_tiStarted`.
    fn focused(session: Value, text: Option<&str>) -> Vec<u8> {
        let mut objects = vec![
            Value::String("$null".into()),
            session,
            dictionary([("docSt", uid(3))]),
        ];
        objects.push(match text {
            Some(_) => dictionary([("contextBeforeInput", uid(4))]),
            None => dictionary([]),
        });
        if let Some(text) = text {
            objects.push(Value::String(text.into()));
        }
        let archive = dictionary([
            ("$top", dictionary([("sessionUUID", uid(1)), ("documentState", uid(2))])),
            ("$objects", Value::Array(objects)),
        ]);
        let mut out = Vec::new();
        archive.to_writer_binary(&mut out).unwrap();
        out
    }

    /// Follow a UID through the object table.
    fn deref<'a>(objects: &'a [Value], value: &'a Value) -> &'a Value {
        match value {
            Value::Uid(uid) => &objects[uid.get() as usize],
            other => other,
        }
    }

    fn decode(archive: &[u8]) -> (Value, Vec<Value>) {
        let root = Value::from_reader(Cursor::new(archive)).unwrap();
        let objects = root.as_dictionary().unwrap()["$objects"].as_array().unwrap().clone();
        (root, objects)
    }

    #[test]
    fn reads_the_session_and_text() {
        let field = read_field(&focused(Value::Data(SESSION.to_vec()), Some("star w"))).unwrap();
        assert_eq!(field, TextField { session: SESSION.to_vec(), text: "star w".into() });
    }

    #[test]
    fn an_empty_field_reads_as_empty_text() {
        let field = read_field(&focused(Value::Data(SESSION.to_vec()), None)).unwrap();
        assert_eq!(field.text, "");
    }

    #[test]
    fn reads_a_session_archived_as_an_nsuuid() {
        let nsuuid = dictionary([("NS.uuidbytes", Value::Data(SESSION.to_vec()))]);
        let field = read_field(&focused(nsuuid, Some("x"))).unwrap();
        assert_eq!(field.session, SESSION);
    }

    #[test]
    fn rejects_an_archive_without_a_session() {
        assert!(read_field(&focused(Value::String("nope".into()), None)).is_err());
        assert!(read_field(b"not a plist").is_err());
    }

    #[test]
    fn insert_targets_the_session_and_carries_the_text() {
        let (root, objects) = decode(&insert(&SESSION, "hello").unwrap());
        let top = root.as_dictionary().unwrap()["$top"].as_dictionary().unwrap();
        let operations = deref(&objects, &top["textOperations"]).as_dictionary().unwrap();

        let class = deref(&objects, &operations["$class"]).as_dictionary().unwrap();
        assert_eq!(class["$classname"].as_string(), Some("RTITextOperations"));

        let target = deref(&objects, &operations["targetSessionUUID"]).as_dictionary().unwrap();
        assert_eq!(target["NS.uuidbytes"].as_data(), Some(&SESSION[..]));
        let class = deref(&objects, &target["$class"]).as_dictionary().unwrap();
        assert_eq!(class["$classname"].as_string(), Some("NSUUID"));

        let output = deref(&objects, &operations["keyboardOutput"]).as_dictionary().unwrap();
        assert_eq!(deref(&objects, &output["insertionText"]).as_string(), Some("hello"));
        let class = deref(&objects, &output["$class"]).as_dictionary().unwrap();
        assert_eq!(class["$classname"].as_string(), Some("TIKeyboardOutput"));

        assert!(!operations.contains_key("textToAssert"));
    }

    #[test]
    fn an_edit_carries_its_deletions_inline() {
        let (root, objects) = decode(&edit(&SESSION, 3, "ek").unwrap());
        let top = root.as_dictionary().unwrap()["$top"].as_dictionary().unwrap();
        let operations = deref(&objects, &top["textOperations"]).as_dictionary().unwrap();
        let output = deref(&objects, &operations["keyboardOutput"]).as_dictionary().unwrap();
        assert_eq!(output["deletionCount"].as_unsigned_integer(), Some(3));
        assert_eq!(deref(&objects, &output["insertionText"]).as_string(), Some("ek"));
    }

    #[test]
    fn a_pure_deletion_types_nothing() {
        let (root, objects) = decode(&edit(&SESSION, 1, "").unwrap());
        let top = root.as_dictionary().unwrap()["$top"].as_dictionary().unwrap();
        let operations = deref(&objects, &top["textOperations"]).as_dictionary().unwrap();
        let output = deref(&objects, &operations["keyboardOutput"]).as_dictionary().unwrap();
        assert!(!output.contains_key("insertionText"));
    }

    #[test]
    fn clear_asserts_empty_text() {
        let (root, objects) = decode(&clear(&SESSION).unwrap());
        let top = root.as_dictionary().unwrap()["$top"].as_dictionary().unwrap();
        let operations = deref(&objects, &top["textOperations"]).as_dictionary().unwrap();
        assert_eq!(deref(&objects, &operations["textToAssert"]).as_string(), Some(""));

        let output = deref(&objects, &operations["keyboardOutput"]).as_dictionary().unwrap();
        assert!(!output.contains_key("insertionText"));
    }

    #[test]
    fn archives_name_the_keyed_archiver_the_device_expects() {
        let (root, _) = decode(&clear(&SESSION).unwrap());
        let root = root.as_dictionary().unwrap();
        assert_eq!(root["$archiver"].as_string(), Some("RTIKeyedArchiver"));
        assert_eq!(root["$version"].as_signed_integer(), Some(100_000));
    }
}
