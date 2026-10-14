//! Minecraft wire primitives: a bounds-checked reader over a packet body and
//! the matching writer methods on `Vec<u8>`.
//!
//! Encodings, for readers of the translation code:
//!
//! - VarInt / VarLong: little-endian base-128, 7 bits per byte, the high bit
//!   set on every byte but the last; at most 5 / 10 bytes. Negative numbers
//!   take the maximum length (two's complement, no zig-zag).
//! - Fixed-width numbers are big-endian; floats are IEEE 754.
//! - A string is a VarInt byte length followed by UTF-8.
//! - A block position is one `i64`: x in the top 26 bits, z in the next 26,
//!   y in the low 12, each signed.
//!
//! The reader never panics: every read checks the remaining length and
//! reports [`WireError::Eof`] instead. Translation code turns any error into a
//! "could not translate" decision for that packet; a malformed body never
//! reaches the other side half-rewritten.

use std::fmt;

/// Why a body could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The body ended in the middle of a value.
    Eof,
    /// A VarInt or VarLong longer than its maximum length.
    VarTooLong,
    /// A length or count that cannot be valid (negative, absurd).
    BadLength,
    /// A structural rule of the format was broken; the static text says which.
    Malformed(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eof => f.write_str("unexpected end of packet"),
            Self::VarTooLong => f.write_str("varint too long"),
            Self::BadLength => f.write_str("invalid length"),
            Self::Malformed(what) => write!(f, "malformed: {what}"),
        }
    }
}

impl std::error::Error for WireError {}

pub type WireResult<T> = Result<T, WireError>;

/// Upper bound on any length prefix the translator will believe. Packets are
/// at most 8 MiB uncompressed, so a larger count is a corrupt body; refusing it
/// up front keeps a hostile backend from making us reserve gigabytes.
pub const MAX_LENGTH: usize = 8 * 1024 * 1024;

/// A cursor over one packet body.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[must_use]
    pub const fn pos(&self) -> usize {
        self.pos
    }

    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The unread part of the body.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// The bytes between `start` (a previous [`pos`](Self::pos)) and here.
    #[must_use]
    pub fn since(&self, start: usize) -> &'a [u8] {
        &self.buf[start.min(self.pos)..self.pos]
    }

    pub fn bytes(&mut self, len: usize) -> WireResult<&'a [u8]> {
        if len > self.remaining() {
            return Err(WireError::Eof);
        }
        let out = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    pub fn skip(&mut self, len: usize) -> WireResult<()> {
        self.bytes(len).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> WireResult<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> WireResult<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn i8(&mut self) -> WireResult<i8> {
        Ok(i8::from_be_bytes(self.array()?))
    }

    pub fn bool(&mut self) -> WireResult<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn i16(&mut self) -> WireResult<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> WireResult<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> WireResult<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    pub fn f32(&mut self) -> WireResult<f32> {
        Ok(f32::from_be_bytes(self.array()?))
    }

    pub fn f64(&mut self) -> WireResult<f64> {
        Ok(f64::from_be_bytes(self.array()?))
    }

    pub fn uuid(&mut self) -> WireResult<[u8; 16]> {
        self.array()
    }

    pub fn varint(&mut self) -> WireResult<i32> {
        let mut value: u32 = 0;
        for i in 0..5 {
            let byte = self.u8()?;
            value |= u32::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        Err(WireError::VarTooLong)
    }

    pub fn varlong(&mut self) -> WireResult<i64> {
        let mut value: u64 = 0;
        for i in 0..10 {
            let byte = self.u8()?;
            value |= u64::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value as i64);
            }
        }
        Err(WireError::VarTooLong)
    }

    /// A VarInt used as a length or count: non-negative and bounded.
    pub fn len(&mut self) -> WireResult<usize> {
        let raw = self.varint()?;
        let len = usize::try_from(raw).map_err(|_| WireError::BadLength)?;
        if len > MAX_LENGTH {
            return Err(WireError::BadLength);
        }
        Ok(len)
    }

    /// A length-prefixed string's raw UTF-8 bytes.
    pub fn str_bytes(&mut self) -> WireResult<&'a [u8]> {
        let len = self.len()?;
        self.bytes(len)
    }

    pub fn string(&mut self) -> WireResult<&'a str> {
        std::str::from_utf8(self.str_bytes()?)
            .map_err(|_| WireError::Malformed("string is not UTF-8"))
    }
}

/// Writer half: encoders on a byte vector.
pub trait Put {
    fn put_u8(&mut self, v: u8);
    fn put_bytes(&mut self, v: &[u8]);

    fn put_i8(&mut self, v: i8) {
        self.put_bytes(&v.to_be_bytes());
    }

    fn put_bool(&mut self, v: bool) {
        self.put_u8(u8::from(v));
    }

    fn put_i16(&mut self, v: i16) {
        self.put_bytes(&v.to_be_bytes());
    }

    fn put_i32(&mut self, v: i32) {
        self.put_bytes(&v.to_be_bytes());
    }

    fn put_i64(&mut self, v: i64) {
        self.put_bytes(&v.to_be_bytes());
    }

    fn put_f32(&mut self, v: f32) {
        self.put_bytes(&v.to_be_bytes());
    }

    fn put_f64(&mut self, v: f64) {
        self.put_bytes(&v.to_be_bytes());
    }

    fn put_varint(&mut self, v: i32) {
        let mut value = v as u32;
        loop {
            if value & !0x7F == 0 {
                self.put_u8(value as u8);
                return;
            }
            self.put_u8((value as u8 & 0x7F) | 0x80);
            value >>= 7;
        }
    }

    fn put_varlong(&mut self, v: i64) {
        let mut value = v as u64;
        loop {
            if value & !0x7F == 0 {
                self.put_u8(value as u8);
                return;
            }
            self.put_u8((value as u8 & 0x7F) | 0x80);
            value >>= 7;
        }
    }

    /// A length prefix; lengths come from slices, so they always fit.
    fn put_len(&mut self, len: usize) {
        self.put_varint(i32::try_from(len).unwrap_or(i32::MAX));
    }

    fn put_str(&mut self, v: &str) {
        self.put_len(v.len());
        self.put_bytes(v.as_bytes());
    }

    fn put_str_bytes(&mut self, v: &[u8]) {
        self.put_len(v.len());
        self.put_bytes(v);
    }
}

impl Put for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }

    fn put_bytes(&mut self, v: &[u8]) {
        self.extend_from_slice(v);
    }
}

/// Number of bytes `v` takes as a VarInt.
#[must_use]
pub const fn varint_len(v: i32) -> usize {
    let v = v as u32;
    match v {
        0..=0x7F => 1,
        0x80..=0x3FFF => 2,
        0x4000..=0x001F_FFFF => 3,
        0x0020_0000..=0x0FFF_FFFF => 4,
        _ => 5,
    }
}

/// `u32` to the `i32` a VarInt carries. Registry ids never exceed `i32::MAX`.
#[must_use]
pub fn wire_id(id: u32) -> i32 {
    i32::try_from(id).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn varint_round_trips_at_every_length() {
        for v in [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            i32::MAX,
            -1,
            i32::MIN,
        ] {
            let mut buf = Vec::new();
            buf.put_varint(v);
            assert_eq!(buf.len(), varint_len(v), "{v}");
            let mut r = Reader::new(&buf);
            assert_eq!(r.varint().unwrap(), v);
            assert!(r.is_empty());
        }
    }

    #[test]
    fn varlong_round_trips() {
        for v in [0i64, 300, i64::MAX, -1, i64::MIN] {
            let mut buf = Vec::new();
            buf.put_varlong(v);
            assert_eq!(Reader::new(&buf).varlong().unwrap(), v);
        }
    }

    #[test]
    fn reads_report_eof_instead_of_panicking() {
        let mut r = Reader::new(&[0x80, 0x80]);
        assert_eq!(r.varint(), Err(WireError::Eof));
        let mut r = Reader::new(&[0, 0, 0]);
        assert_eq!(r.i32(), Err(WireError::Eof));
        let mut r = Reader::new(&[0xFF; 6]);
        assert_eq!(r.varint(), Err(WireError::VarTooLong));
    }

    #[test]
    fn lengths_are_bounded() {
        let mut buf = Vec::new();
        buf.put_varint(-5);
        assert_eq!(Reader::new(&buf).len(), Err(WireError::BadLength));
        let mut buf = Vec::new();
        buf.put_varint(i32::MAX);
        assert_eq!(Reader::new(&buf).len(), Err(WireError::BadLength));
    }
}
