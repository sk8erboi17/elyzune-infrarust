//! Network NBT: skipping, and a small editable tree.
//!
//! Since 1.20.2 the network form of NBT has an unnamed root: one type byte and
//! the payload, with no root name. Text components travel as NBT (often a bare
//! string tag), item custom data as a compound, registry entries as compounds.
//! A root of type `TAG_End` (a single zero byte) means "absent" where the
//! format allows it.
//!
//! Strings are Java's modified UTF-8 (NUL as two bytes, supplementary
//! characters as surrogate pairs). The tree keeps them as raw bytes so a
//! translation that does not touch a string writes back exactly what it read;
//! only ASCII keys and values are ever compared or produced by the translator.
//!
//! Depth is capped at [`MAX_DEPTH`], as vanilla does, so a hostile peer
//! cannot make the translator recurse without bound.

use crate::wire::{Put, Reader, WireError, WireResult};

pub const TAG_END: u8 = 0;
pub const TAG_BYTE: u8 = 1;
pub const TAG_SHORT: u8 = 2;
pub const TAG_INT: u8 = 3;
pub const TAG_LONG: u8 = 4;
pub const TAG_FLOAT: u8 = 5;
pub const TAG_DOUBLE: u8 = 6;
pub const TAG_BYTE_ARRAY: u8 = 7;
pub const TAG_STRING: u8 = 8;
pub const TAG_LIST: u8 = 9;
pub const TAG_COMPOUND: u8 = 10;
pub const TAG_INT_ARRAY: u8 = 11;
pub const TAG_LONG_ARRAY: u8 = 12;

/// Vanilla's `NbtAccounter` depth limit.
pub const MAX_DEPTH: usize = 512;

/// Skips one network NBT value (type byte and payload).
pub fn skip(r: &mut Reader<'_>) -> WireResult<()> {
    let ty = r.u8()?;
    if ty == TAG_END {
        return Ok(());
    }
    skip_payload(r, ty, 0)
}

fn nbt_len(r: &mut Reader<'_>) -> WireResult<usize> {
    let n = r.i32()?;
    let n = usize::try_from(n).map_err(|_| WireError::BadLength)?;
    if n > crate::wire::MAX_LENGTH {
        return Err(WireError::BadLength);
    }
    Ok(n)
}

fn string_len(r: &mut Reader<'_>) -> WireResult<usize> {
    Ok(usize::from(u16::from_be_bytes([r.u8()?, r.u8()?])))
}

fn skip_payload(r: &mut Reader<'_>, ty: u8, depth: usize) -> WireResult<()> {
    if depth > MAX_DEPTH {
        return Err(WireError::Malformed("nbt nested too deeply"));
    }
    match ty {
        TAG_BYTE => r.skip(1),
        TAG_SHORT => r.skip(2),
        TAG_INT | TAG_FLOAT => r.skip(4),
        TAG_LONG | TAG_DOUBLE => r.skip(8),
        TAG_BYTE_ARRAY => {
            let n = nbt_len(r)?;
            r.skip(n)
        }
        TAG_STRING => {
            let n = string_len(r)?;
            r.skip(n)
        }
        TAG_LIST => {
            let elem = r.u8()?;
            let n = nbt_len(r)?;
            if elem == TAG_END && n > 0 {
                return Err(WireError::Malformed("list of TAG_End with elements"));
            }
            for _ in 0..n {
                skip_payload(r, elem, depth + 1)?;
            }
            Ok(())
        }
        TAG_COMPOUND => loop {
            let child = r.u8()?;
            if child == TAG_END {
                return Ok(());
            }
            let n = string_len(r)?;
            r.skip(n)?;
            skip_payload(r, child, depth + 1)?;
        },
        TAG_INT_ARRAY => {
            let n = nbt_len(r)?;
            r.skip(n.checked_mul(4).ok_or(WireError::BadLength)?)
        }
        TAG_LONG_ARRAY => {
            let n = nbt_len(r)?;
            r.skip(n.checked_mul(8).ok_or(WireError::BadLength)?)
        }
        _ => Err(WireError::Malformed("unknown nbt tag type")),
    }
}

/// An editable NBT value. Byte strings are modified UTF-8, kept verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(Vec<u8>),
    /// Element type and elements; an empty list may carry any element type.
    List(u8, Vec<Tag>),
    Compound(Vec<(Vec<u8>, Tag)>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    #[must_use]
    pub const fn type_id(&self) -> u8 {
        match self {
            Self::Byte(_) => TAG_BYTE,
            Self::Short(_) => TAG_SHORT,
            Self::Int(_) => TAG_INT,
            Self::Long(_) => TAG_LONG,
            Self::Float(_) => TAG_FLOAT,
            Self::Double(_) => TAG_DOUBLE,
            Self::ByteArray(_) => TAG_BYTE_ARRAY,
            Self::String(_) => TAG_STRING,
            Self::List(..) => TAG_LIST,
            Self::Compound(_) => TAG_COMPOUND,
            Self::IntArray(_) => TAG_INT_ARRAY,
            Self::LongArray(_) => TAG_LONG_ARRAY,
        }
    }

    #[must_use]
    pub fn string(s: &str) -> Self {
        Self::String(s.as_bytes().to_vec())
    }

    /// The value as a string, when it is an ASCII-compatible string tag.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Tag> {
        match self {
            Self::Compound(entries) => entries
                .iter()
                .find(|(k, _)| k.as_slice() == key.as_bytes())
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Tag> {
        match self {
            Self::Compound(entries) => entries
                .iter_mut()
                .find(|(k, _)| k.as_slice() == key.as_bytes())
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Removes `key` from a compound, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Tag> {
        match self {
            Self::Compound(entries) => {
                let index = entries
                    .iter()
                    .position(|(k, _)| k.as_slice() == key.as_bytes())?;
                Some(entries.remove(index).1)
            }
            _ => None,
        }
    }

    /// Sets `key` in a compound, replacing an existing value in place.
    pub fn insert(&mut self, key: &str, value: Tag) {
        if let Self::Compound(entries) = self {
            if let Some(slot) = entries
                .iter_mut()
                .find(|(k, _)| k.as_slice() == key.as_bytes())
            {
                slot.1 = value;
            } else {
                entries.push((key.as_bytes().to_vec(), value));
            }
        }
    }

    /// Reads one network NBT value; `None` for a `TAG_End` root.
    pub fn read(r: &mut Reader<'_>) -> WireResult<Option<Tag>> {
        let ty = r.u8()?;
        if ty == TAG_END {
            return Ok(None);
        }
        read_payload(r, ty, 0).map(Some)
    }

    /// Writes this value in network form (type byte, then payload).
    pub fn write(&self, w: &mut Vec<u8>) {
        w.put_u8(self.type_id());
        self.write_payload(w);
    }

    fn write_payload(&self, w: &mut Vec<u8>) {
        match self {
            Self::Byte(v) => w.put_i8(*v),
            Self::Short(v) => w.put_i16(*v),
            Self::Int(v) => w.put_i32(*v),
            Self::Long(v) => w.put_i64(*v),
            Self::Float(v) => w.put_f32(*v),
            Self::Double(v) => w.put_f64(*v),
            Self::ByteArray(v) => {
                w.put_i32(len_i32(v.len()));
                w.put_bytes(v);
            }
            Self::String(v) => put_nbt_string(w, v),
            Self::List(elem, items) => {
                let elem = items.first().map_or(*elem, Tag::type_id);
                w.put_u8(elem);
                w.put_i32(len_i32(items.len()));
                for item in items {
                    item.write_payload(w);
                }
            }
            Self::Compound(entries) => {
                for (key, value) in entries {
                    w.put_u8(value.type_id());
                    put_nbt_string(w, key);
                    value.write_payload(w);
                }
                w.put_u8(TAG_END);
            }
            Self::IntArray(v) => {
                w.put_i32(len_i32(v.len()));
                for x in v {
                    w.put_i32(*x);
                }
            }
            Self::LongArray(v) => {
                w.put_i32(len_i32(v.len()));
                for x in v {
                    w.put_i64(*x);
                }
            }
        }
    }
}

fn len_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn put_nbt_string(w: &mut Vec<u8>, bytes: &[u8]) {
    // Modified UTF-8 strings are at most 65535 bytes by construction; the
    // tree only ever holds strings that were read from the wire or are short
    // translator constants.
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    w.put_bytes(&len.to_be_bytes());
    w.put_bytes(&bytes[..usize::from(len)]);
}

fn read_payload(r: &mut Reader<'_>, ty: u8, depth: usize) -> WireResult<Tag> {
    if depth > MAX_DEPTH {
        return Err(WireError::Malformed("nbt nested too deeply"));
    }
    Ok(match ty {
        TAG_BYTE => Tag::Byte(r.i8()?),
        TAG_SHORT => Tag::Short(r.i16()?),
        TAG_INT => Tag::Int(r.i32()?),
        TAG_LONG => Tag::Long(r.i64()?),
        TAG_FLOAT => Tag::Float(r.f32()?),
        TAG_DOUBLE => Tag::Double(r.f64()?),
        TAG_BYTE_ARRAY => {
            let n = nbt_len(r)?;
            Tag::ByteArray(r.bytes(n)?.to_vec())
        }
        TAG_STRING => {
            let n = string_len(r)?;
            Tag::String(r.bytes(n)?.to_vec())
        }
        TAG_LIST => {
            let elem = r.u8()?;
            let n = nbt_len(r)?;
            if elem == TAG_END && n > 0 {
                return Err(WireError::Malformed("list of TAG_End with elements"));
            }
            let mut items = Vec::with_capacity(n.min(r.remaining()));
            for _ in 0..n {
                items.push(read_payload(r, elem, depth + 1)?);
            }
            Tag::List(elem, items)
        }
        TAG_COMPOUND => {
            let mut entries = Vec::new();
            loop {
                let child = r.u8()?;
                if child == TAG_END {
                    break;
                }
                let n = string_len(r)?;
                let key = r.bytes(n)?.to_vec();
                let value = read_payload(r, child, depth + 1)?;
                entries.push((key, value));
            }
            Tag::Compound(entries)
        }
        TAG_INT_ARRAY => {
            let n = nbt_len(r)?;
            let mut v = Vec::with_capacity(n.min(r.remaining() / 4));
            for _ in 0..n {
                v.push(r.i32()?);
            }
            Tag::IntArray(v)
        }
        TAG_LONG_ARRAY => {
            let n = nbt_len(r)?;
            let mut v = Vec::with_capacity(n.min(r.remaining() / 8));
            for _ in 0..n {
                v.push(r.i64()?);
            }
            Tag::LongArray(v)
        }
        _ => return Err(WireError::Malformed("unknown nbt tag type")),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn sample() -> Tag {
        Tag::Compound(vec![
            (b"text".to_vec(), Tag::string("hi")),
            (
                b"extra".to_vec(),
                Tag::List(
                    TAG_COMPOUND,
                    vec![Tag::Compound(vec![(b"n".to_vec(), Tag::Int(3))])],
                ),
            ),
            (b"longs".to_vec(), Tag::LongArray(vec![1, -2])),
            (b"empty".to_vec(), Tag::List(TAG_END, vec![])),
        ])
    }

    #[test]
    fn tree_round_trips_and_skip_agrees_with_read() {
        let mut buf = Vec::new();
        sample().write(&mut buf);
        buf.extend_from_slice(b"tail");

        let mut r = Reader::new(&buf);
        let tag = Tag::read(&mut r).unwrap().unwrap();
        assert_eq!(tag, sample());
        assert_eq!(r.rest(), b"tail");

        let mut r = Reader::new(&buf);
        skip(&mut r).unwrap();
        assert_eq!(r.rest(), b"tail");
    }

    #[test]
    fn compound_editing() {
        let mut tag = sample();
        assert_eq!(tag.get("text").and_then(Tag::as_str), Some("hi"));
        tag.insert("text", Tag::string("yo"));
        tag.insert("new", Tag::Byte(1));
        assert_eq!(tag.remove("longs"), Some(Tag::LongArray(vec![1, -2])));
        assert_eq!(tag.get("text").and_then(Tag::as_str), Some("yo"));
        assert_eq!(tag.get("new"), Some(&Tag::Byte(1)));
    }

    #[test]
    fn end_root_is_absent_and_garbage_is_rejected() {
        assert_eq!(Tag::read(&mut Reader::new(&[0])).unwrap(), None);
        assert!(Tag::read(&mut Reader::new(&[99])).is_err());
        assert!(skip(&mut Reader::new(&[TAG_STRING, 0, 5, b'a'])).is_err());
    }

    #[test]
    fn depth_is_capped() {
        let mut buf = vec![TAG_LIST];
        for _ in 0..600 {
            buf.extend_from_slice(&[TAG_LIST, 0, 0, 0, 1]);
        }
        assert!(skip(&mut Reader::new(&buf)).is_err());
    }
}
