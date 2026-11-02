//! Axiom (API 10) for a client of another release than the backend's.
//!
//! The backend speaks Axiom as a 26.2 server (`events/elyzune-build` in the
//! Hyperion tree): a block state there is a 26.2 id, and a direct palette
//! is 15 bits wide. A 1.21.11, 26.1 or 26.3 client's Axiom writes its own
//! ids (26.3's direct palettes are 16 bits wide). This module makes such a
//! client look like a 26.2 one, as `AxiomPaper` does through ViaVersion:
//!
//! - `axiom:hello` (up): the data version and protocol become 26.2's, so the
//!   backend's same-version check holds;
//! - `axiom:set_block` and `axiom:set_buffer` (up, direct or inside
//!   `axiom:tunnel`): every block state goes up through the generated
//!   client-to-26.2 table (`generated/blocks.rs`, ViaBackwards' substitutes
//!   for 26.3's new blocks), each section rewritten at 26.2's width; block
//!   entity data and biome identifiers pass as they are (`AxiomPaper` does
//!   not data-fix them either);
//! - `axiom:response_chunk_data` (down): each section comes down through the
//!   server-to-client table at the client's width.
//!
//! # The tunnel
//!
//! Axiom sends anything over a serverbound payload's 32767 bytes as
//! `axiom:tunnel` fragments (`u8 flags`: FIRST 1, LAST 2, then data) of one
//! frame (`Identifier id, i32 size, u8 flags (ZSTD 1), body`). A frame is
//! reassembled here, its body translated when it is one of the two block
//! packets (decompressed, and sent on uncompressed: the link to the backend
//! is local), and cut again into fragments of at most [`SPLIT`] bytes.
//! Every other frame goes on as it came. The reassembly is bounded by the
//! backend's own limit ([`MAX_FRAME`]).

use bytes::Bytes;

use crate::chunk::{BlockRemap, IdRemap, Kind, container};
use crate::generated::{Proto, blocks};
use crate::session::ViaTranslator;
use crate::wire::{Put, Reader, WireError, WireResult, wire_id};

const HELLO: &str = "axiom:hello";
const TUNNEL: &str = "axiom:tunnel";
const SET_BLOCK: &str = "axiom:set_block";
const SET_BUFFER: &str = "axiom:set_buffer";
const RESPONSE_CHUNK_DATA: &str = "axiom:response_chunk_data";

/// The backend's data version and protocol (26.2).
pub const SERVER_DATA_VERSION: i32 = 4903;
pub const SERVER_PROTOCOL: i32 = 776;
/// `tunnel-split-size` the backend announces: a fragment's largest size,
/// flag included.
pub const SPLIT: usize = 31_000;
/// `maximum-tunnel-packet-size` the backend announces: a frame body's
/// largest size.
pub const MAX_FRAME: usize = 2_097_152;
/// `MIN_POSITION_LONG`, the end of Axiom's position lists.
const MIN_POSITION: i64 = -9_223_371_899_415_820_288;

const FIRST: u8 = 1;
const LAST: u8 = 2;
const ZSTD: u8 = 1;

/// A tunnel frame being reassembled for one connection (module docs).
#[derive(Debug, Default)]
pub struct Tunnel {
    data: Vec<u8>,
    pending: bool,
    skipping: bool,
}

/// Client states up to 26.2, sections written at 26.2's direct width.
struct Up<'a>(&'a ViaTranslator);

impl IdRemap for Up<'_> {
    fn map(&self, id: u32) -> u32 {
        // Total for every id the client has; a broken client's id is air.
        self.0.tables.block_states.c2s(id).unwrap_or(0)
    }

    fn client_bits(&self) -> u8 {
        blocks::STATE_BITS_776
    }

    fn is_identity(&self) -> bool {
        false
    }
}

fn channel_payload(channel: &str, data: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(channel.len() + 3 + data.len());
    out.put_str(channel);
    out.put_bytes(data);
    out.into()
}

/// A serverbound `custom_payload` body of a translated client: what goes to
/// the backend, zero or more payloads (a tunnel fragment that does not end
/// a frame sends nothing yet).
///
/// # Errors
/// A malformed Axiom payload; the caller drops it.
pub fn serverbound(t: &mut ViaTranslator, body: &Bytes) -> WireResult<Vec<Bytes>> {
    let mut r = Reader::new(body);
    let channel = r.string()?;
    let data = r.rest();
    Ok(match channel {
        HELLO => vec![channel_payload(channel, &hello(data)?)],
        SET_BLOCK => vec![channel_payload(channel, &set_block(t, data)?)],
        SET_BUFFER => vec![channel_payload(channel, &set_buffer(t, data)?)],
        TUNNEL => tunnel(t, data)?
            .into_iter()
            .map(|fragment| channel_payload(TUNNEL, &fragment))
            .collect(),
        _ => vec![body.clone()],
    })
}

/// A clientbound `custom_payload` body for a translated client.
///
/// # Errors
/// A malformed chunk response; the caller drops it.
pub fn clientbound(t: &ViaTranslator, body: Bytes) -> WireResult<Bytes> {
    let mut r = Reader::new(&body);
    let channel = r.string()?;
    if channel != RESPONSE_CHUNK_DATA {
        return Ok(body);
    }
    let data = response_chunk_data(t, r.rest())?;
    Ok(channel_payload(channel, &data))
}

fn hello(data: &[u8]) -> WireResult<Vec<u8>> {
    let mut r = Reader::new(data);
    let api = r.varint()?;
    let _data_version = r.varint()?;
    let _protocol = r.varint()?;
    let mut out = Vec::with_capacity(data.len() + 4);
    out.put_varint(api);
    out.put_varint(SERVER_DATA_VERSION);
    out.put_varint(SERVER_PROTOCOL);
    out.put_bytes(r.rest());
    Ok(out)
}

fn set_block(t: &ViaTranslator, data: &[u8]) -> WireResult<Vec<u8>> {
    let mut r = Reader::new(data);
    let n = r.len()?;
    let mut out = Vec::with_capacity(data.len());
    out.put_len(n);
    for _ in 0..n {
        out.put_i64(r.i64()?);
        let state =
            u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative state"))?;
        out.put_varint(wire_id(Up(t).map(state)));
    }
    out.put_bytes(r.rest());
    Ok(out)
}

/// Copies one `CompressedBlockEntity` (VarInt size, u8 dictionary,
/// ByteArray).
fn copy_block_entity(r: &mut Reader<'_>, out: &mut Vec<u8>) -> WireResult<()> {
    out.put_varint(r.varint()?);
    out.put_u8(r.u8()?);
    let n = r.len()?;
    out.put_len(n);
    out.put_bytes(r.bytes(n)?);
    Ok(())
}

fn set_buffer(t: &ViaTranslator, data: &[u8]) -> WireResult<Vec<u8>> {
    let mut r = Reader::new(data);
    let mut out = Vec::with_capacity(data.len());
    out.put_str(r.string()?);
    out.put_bytes(r.bytes(16)?);
    let kind = r.u8()?;
    out.put_u8(kind);
    if kind == 0 {
        loop {
            let key = r.i64()?;
            out.put_i64(key);
            if key == MIN_POSITION {
                break;
            }
            container(Kind::Blocks, &Up(t), &mut r, &mut out, None)?;
            let n = r.varint()?;
            out.put_varint(n);
            for _ in 0..n.clamp(0, 4096) {
                out.put_i16(r.i16()?);
                copy_block_entity(&mut r, &mut out)?;
            }
        }
    }
    // Biomes are identifiers; the trailing credit is a number.
    out.put_bytes(r.rest());
    Ok(out)
}

fn response_chunk_data(t: &ViaTranslator, data: &[u8]) -> WireResult<Vec<u8>> {
    let mut r = Reader::new(data);
    let mut out = Vec::with_capacity(data.len() + data.len() / 16);
    out.put_i64(r.i64()?);
    loop {
        let at = r.i64()?;
        out.put_i64(at);
        if at == MIN_POSITION {
            break;
        }
        copy_block_entity(&mut r, &mut out)?;
    }
    loop {
        let at = r.i64()?;
        out.put_i64(at);
        if at == MIN_POSITION {
            break;
        }
        let present = r.u8()?;
        out.put_u8(present);
        if present != 0 {
            container(Kind::Blocks, &BlockRemap(t), &mut r, &mut out, None)?;
        }
    }
    out.put_bytes(r.rest());
    Ok(out)
}

/// One serverbound tunnel fragment: the fragments to send on (module docs).
fn tunnel(t: &mut ViaTranslator, data: &[u8]) -> WireResult<Vec<Vec<u8>>> {
    let Some((&flags, chunk)) = data.split_first() else {
        return Err(WireError::Malformed("an empty axiom tunnel fragment"));
    };
    let state = &mut t.conn.axiom_tunnel;
    let frame = if flags & FIRST != 0 && flags & LAST != 0 {
        chunk.to_vec()
    } else {
        if flags & FIRST != 0 {
            state.data.clear();
            state.pending = true;
            state.skipping = false;
        } else if !state.pending || state.skipping {
            state.skipping = true;
            return Ok(Vec::new());
        }
        if state.data.len() + chunk.len() > MAX_FRAME + 64 {
            state.data = Vec::new();
            state.pending = false;
            state.skipping = true;
            return Err(WireError::Malformed("an axiom tunnel frame past 2 MiB"));
        }
        state.data.extend_from_slice(chunk);
        if flags & LAST == 0 {
            return Ok(Vec::new());
        }
        state.pending = false;
        std::mem::take(&mut state.data)
    };
    let frame = translate_frame(t, frame)?;
    Ok(fragments(&frame))
}

/// A frame with its block packet translated, or as it came.
fn translate_frame(t: &ViaTranslator, frame: Vec<u8>) -> WireResult<Vec<u8>> {
    let mut r = Reader::new(&frame);
    let id = r.string()?;
    if id != SET_BLOCK && id != SET_BUFFER {
        return Ok(frame);
    }
    let size = usize::try_from(r.i32()?).map_err(|_| WireError::BadLength)?;
    if size > MAX_FRAME {
        return Err(WireError::BadLength);
    }
    let flags = r.u8()?;
    let rest = r.rest();
    let body = if flags & ZSTD != 0 {
        let body = zstd::bulk::decompress(rest, size)
            .map_err(|_| WireError::Malformed("an axiom frame that does not decompress"))?;
        if body.len() != size {
            return Err(WireError::Malformed("an axiom frame of the wrong size"));
        }
        body
    } else {
        rest.to_vec()
    };
    let translated = if id == SET_BLOCK {
        set_block(t, &body)?
    } else {
        set_buffer(t, &body)?
    };
    let mut out = Vec::with_capacity(id.len() + 6 + translated.len());
    out.put_str(id);
    out.put_i32(i32::try_from(translated.len()).map_err(|_| WireError::BadLength)?);
    out.put_u8(0);
    out.put_bytes(&translated);
    Ok(out)
}

/// A frame cut as Axiom cuts it: `SPLIT - 1` data bytes a fragment.
fn fragments(frame: &[u8]) -> Vec<Vec<u8>> {
    let pieces: Vec<&[u8]> = frame.chunks(SPLIT - 1).collect();
    if pieces.len() <= 1 {
        let mut one = vec![FIRST | LAST];
        one.extend_from_slice(frame);
        return vec![one];
    }
    let last = pieces.len() - 1;
    pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| {
            let flags = match i {
                0 => FIRST,
                i if i == last => LAST,
                _ => 0,
            };
            let mut fragment = Vec::with_capacity(piece.len() + 1);
            fragment.push(flags);
            fragment.extend_from_slice(piece);
            fragment
        })
        .collect()
}

/// Whether `client` needs this module (every translated release).
#[must_use]
pub const fn applies(client: Proto) -> bool {
    !matches!(client, Proto::V776)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // State ids from the releases' blocks.json: 26.3's poplar planks (27)
    // and a top waterlogged black concrete slab (18397) go up as birch
    // planks (17) and the same blackstone slab (22236); stone is 1 in all.
    const POPLAR_PLANKS_777: u32 = 27;
    const BIRCH_PLANKS_776: u32 = 17;
    const CONCRETE_SLAB_777: u32 = 18397;
    const BLACKSTONE_SLAB_776: u32 = 22236;

    fn payload(channel: &str, data: &[u8]) -> Bytes {
        channel_payload(channel, data)
    }

    fn split(body: &Bytes) -> (String, Vec<u8>) {
        let mut r = Reader::new(body);
        let channel = r.string().unwrap().to_owned();
        (channel, r.rest().to_vec())
    }

    /// A direct section at `bits`: entry i holds `values[i % len]`.
    fn direct_section(values: &[u32], bits: u8) -> Vec<u8> {
        let mut out = vec![bits];
        let per_long = 64 / usize::from(bits);
        let all: Vec<u32> = (0..4096).map(|i| values[i % values.len()]).collect();
        for chunk in all.chunks(per_long) {
            let mut word = 0u64;
            for (j, &v) in chunk.iter().enumerate() {
                word |= u64::from(v) << (j * usize::from(bits));
            }
            out.put_i64(word.cast_signed());
        }
        for _ in all.len().div_ceil(per_long)..4096usize.div_ceil(per_long) {
            out.put_i64(0);
        }
        out
    }

    /// The entries of a direct section at `bits`.
    fn read_direct(r: &mut Reader<'_>, bits: u8) -> Vec<u32> {
        assert_eq!(r.u8().unwrap(), bits);
        let per_long = 64 / usize::from(bits);
        let mask = (1u64 << bits) - 1;
        let mut out = Vec::new();
        for _ in 0..4096usize.div_ceil(per_long) {
            let word = r.i64().unwrap().cast_unsigned();
            for j in 0..per_long {
                if out.len() < 4096 {
                    out.push(u32::try_from((word >> (j * usize::from(bits))) & mask).unwrap());
                }
            }
        }
        out
    }

    fn set_buffer_body(section: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.put_str("minecraft:overworld");
        body.put_bytes(&[7; 16]);
        body.put_u8(0);
        body.put_i64(42);
        body.put_bytes(section);
        body.put_varint(1);
        body.put_i16(0x123);
        body.put_varint(3);
        body.put_u8(0);
        body.put_len(2);
        body.put_bytes(&[0xAB, 0xCD]);
        body.put_i64(MIN_POSITION);
        body.put_varint(9);
        body
    }

    fn check_set_buffer(body: &[u8]) {
        let mut r = Reader::new(body);
        assert_eq!(r.string().unwrap(), "minecraft:overworld");
        assert_eq!(r.bytes(16).unwrap(), &[7; 16]);
        assert_eq!(r.u8().unwrap(), 0);
        assert_eq!(r.i64().unwrap(), 42);
        let cells = read_direct(&mut r, 15);
        assert_eq!(&cells[..3], &[BIRCH_PLANKS_776, BLACKSTONE_SLAB_776, 1]);
        assert_eq!(r.varint().unwrap(), 1);
        assert_eq!(r.i16().unwrap(), 0x123);
        assert_eq!(r.varint().unwrap(), 3);
        assert_eq!(r.u8().unwrap(), 0);
        assert_eq!(r.len().unwrap(), 2);
        assert_eq!(r.bytes(2).unwrap(), &[0xAB, 0xCD]);
        assert_eq!(r.i64().unwrap(), MIN_POSITION);
        assert_eq!(r.varint().unwrap(), 9);
        assert!(r.is_empty());
    }

    #[test]
    fn the_hello_names_the_backends_release() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        let mut data = Vec::new();
        data.put_varint(10);
        data.put_varint(4550);
        data.put_varint(777);
        data.put_i64(-77);
        let out = serverbound(&mut t, &payload(HELLO, &data)).unwrap();
        let (channel, body) = split(&out[0]);
        assert_eq!(channel, HELLO);
        let mut r = Reader::new(&body);
        assert_eq!(r.varint().unwrap(), 10);
        assert_eq!(r.varint().unwrap(), SERVER_DATA_VERSION);
        assert_eq!(r.varint().unwrap(), SERVER_PROTOCOL);
        assert_eq!(r.i64().unwrap(), -77);
        assert!(r.is_empty());
    }

    #[test]
    fn set_block_states_go_up() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        let mut data = Vec::new();
        data.put_len(2);
        data.put_i64(5);
        data.put_varint(wire_id(POPLAR_PLANKS_777));
        data.put_i64(6);
        data.put_varint(1);
        data.put_bytes(&[1, 2, 3]);
        let out = serverbound(&mut t, &payload(SET_BLOCK, &data)).unwrap();
        let (_, body) = split(&out[0]);
        let mut r = Reader::new(&body);
        assert_eq!(r.len().unwrap(), 2);
        assert_eq!(r.i64().unwrap(), 5);
        assert_eq!(r.varint().unwrap(), wire_id(BIRCH_PLANKS_776));
        assert_eq!(r.i64().unwrap(), 6);
        assert_eq!(r.varint().unwrap(), 1);
        assert_eq!(r.rest(), &[1, 2, 3]);
    }

    #[test]
    fn a_sixteen_bit_section_goes_up_at_fifteen() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        let section = direct_section(&[POPLAR_PLANKS_777, CONCRETE_SLAB_777, 1], 16);
        let data = set_buffer_body(&section);
        let out = serverbound(&mut t, &payload(SET_BUFFER, &data)).unwrap();
        let (channel, body) = split(&out[0]);
        assert_eq!(channel, SET_BUFFER);
        check_set_buffer(&body);
    }

    #[test]
    fn a_compressed_tunnel_frame_is_translated_and_cut_again() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        let section = direct_section(&[POPLAR_PLANKS_777, CONCRETE_SLAB_777, 1], 16);
        let body = set_buffer_body(&section);
        let compressed = zstd::bulk::compress(&body, 3).unwrap();
        let mut frame = Vec::new();
        frame.put_str(SET_BUFFER);
        frame.put_i32(i32::try_from(body.len()).unwrap());
        frame.put_u8(ZSTD);
        frame.put_bytes(&compressed);
        // Cut it in three, as a client with a small split would.
        let third = frame.len() / 3 + 1;
        let pieces: Vec<&[u8]> = frame.chunks(third).collect();
        let mut sent = Vec::new();
        for (i, piece) in pieces.iter().enumerate() {
            let flags = match i {
                0 => FIRST,
                i if i + 1 == pieces.len() => LAST,
                _ => 0,
            };
            let mut fragment = vec![flags];
            fragment.extend_from_slice(piece);
            let out = serverbound(&mut t, &payload(TUNNEL, &fragment)).unwrap();
            if i + 1 < pieces.len() {
                assert!(out.is_empty(), "nothing goes on before the frame ends");
            }
            sent.extend(out);
        }
        // Reassemble what went to the backend.
        let mut whole = Vec::new();
        for payload in &sent {
            let (channel, data) = split(payload);
            assert_eq!(channel, TUNNEL);
            assert!(data.len() <= SPLIT);
            whole.extend_from_slice(&data[1..]);
        }
        let mut r = Reader::new(&whole);
        assert_eq!(r.string().unwrap(), SET_BUFFER);
        let size = usize::try_from(r.i32().unwrap()).unwrap();
        assert_eq!(r.u8().unwrap(), 0, "sent on uncompressed");
        assert_eq!(r.rest().len(), size);
        check_set_buffer(r.rest());
    }

    #[test]
    fn other_payloads_pass() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        let body = payload("axiom:set_gamemode", &[1]);
        assert_eq!(serverbound(&mut t, &body).unwrap(), vec![body.clone()]);
        let other = payload("minecraft:brand", b"x");
        assert_eq!(clientbound(&t, other.clone()).unwrap(), other);
    }

    #[test]
    fn a_chunk_response_comes_down_at_the_clients_width() {
        let t = ViaTranslator::new(Proto::V777, 1);
        // 26.2 stone (1) and birch planks (17) in a direct section.
        let section = direct_section(&[1, BIRCH_PLANKS_776], 15);
        let mut data = Vec::new();
        data.put_i64(99);
        data.put_i64(MIN_POSITION);
        data.put_i64(3);
        data.put_u8(1);
        data.put_bytes(&section);
        data.put_i64(MIN_POSITION);
        data.put_u8(1);
        let out = clientbound(&t, payload(RESPONSE_CHUNK_DATA, &data)).unwrap();
        let (_, body) = split(&out);
        let mut r = Reader::new(&body);
        assert_eq!(r.i64().unwrap(), 99);
        assert_eq!(r.i64().unwrap(), MIN_POSITION);
        assert_eq!(r.i64().unwrap(), 3);
        assert_eq!(r.u8().unwrap(), 1);
        let cells = read_direct(&mut r, blocks::STATE_BITS_777);
        // Stone stays 1; birch planks come down as 26.3's birch planks.
        let birch_777 = t.tables.block_states.s2c(BIRCH_PLANKS_776).unwrap();
        assert_eq!(&cells[..2], &[1, birch_777]);
        assert_eq!(r.i64().unwrap(), MIN_POSITION);
        assert_eq!(r.u8().unwrap(), 1);
        assert!(r.is_empty());
    }
}
