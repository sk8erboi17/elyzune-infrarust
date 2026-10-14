//! Chunks, light, block changes and block entities.
//!
//! # Section format (1.21.5 and later)
//!
//! ```text
//! level_chunk_with_light: i32 x, i32 z,
//!   heightmaps  VarInt n × { VarInt type, VarInt len, len × i64 }
//!   VarInt size, sections (size bytes)
//!   block entities  VarInt n × { u8 packed xz, i16 y, VarInt type, NBT }
//!   light data
//! section: i16 non-air blocks, [i16 fluid count, 26.1+], states, biomes
//! paletted container: u8 bits, then
//!   bits == 0        VarInt value, no data
//!   indirect         VarInt n × VarInt palette entry, data
//!   direct           data, entries are global ids
//!   data: ceil(entries / (64 / storage bits)) × i64, entries packed low bits
//!   first, never straddling two longs
//! light data: 4 × BitSet (sky, block, empty sky, empty block masks),
//!   VarInt n × { VarInt 2048, 2048 bytes } sky, the same for block light
//! ```
//!
//! Blocks use indirect palettes for 1-8 bits (1-4 are stored as 4) and the
//! global palette above; biomes use 1-3 and above. A direct container's
//! storage width is the bits byte itself (`Configuration.Global`), so the
//! translator can choose it: it writes the client's global width (15 for
//! 774-776, 16 for 26.3, whose 35723 states no longer fit in 15 bits).
//!
//! The number of sections is the dimension's height / 16, which the
//! translator does not need: sections are self-delimiting, so it reads them
//! until the declared size is consumed.
//!
//! # What changes per client
//!
//! - every block state and biome id is remapped (biomes only when the client
//!   lacks some of the backend's, which shifts the ids after them);
//! - 1.21.11 has no fluid count in sections (added in 26.1);
//! - 26.3 sends light masks as `BitSet.toByteArray()` (little-endian bytes,
//!   trailing zeros trimmed) instead of `long[]`;
//! - 1.21.11 and 26.1 render beds only with a bed block entity (removed in
//!   26.2), so one is added for every bed block, as ViaBackwards does.

use bytes::Bytes;

use crate::generated::Proto;
use crate::generated::registries::{Reg, id_of, name_of};
use crate::layout::{Dir, Xc};
use crate::nbt;
use crate::session::ViaTranslator;
use crate::wire::{Put, Reader, WireError, WireResult, wire_id};

const BLOCK_ENTRIES: usize = 4096;
const BIOME_ENTRIES: usize = 64;

/// Which palette a container belongs to.
#[derive(Clone, Copy)]
enum Kind {
    Blocks,
    Biomes,
}

impl Kind {
    const fn entries(self) -> usize {
        match self {
            Self::Blocks => BLOCK_ENTRIES,
            Self::Biomes => BIOME_ENTRIES,
        }
    }

    const fn max_indirect(self) -> u8 {
        match self {
            Self::Blocks => 8,
            Self::Biomes => 3,
        }
    }

    /// Storage width of an indirect container sent with `bits`.
    const fn indirect_storage(self, bits: u8) -> u8 {
        match self {
            Self::Blocks if bits <= 4 => 4,
            _ => bits,
        }
    }
}

const fn longs_for(entries: usize, bits: u8) -> usize {
    let per_long = 64 / bits as usize;
    entries.div_ceil(per_long)
}

/// Remaps ids of one palette kind.
pub trait IdRemap {
    fn map(&self, id: u32) -> u32;
    /// Width of the client's global palette for this kind.
    fn client_bits(&self) -> u8;
    /// Whether every id maps to itself (lets indirect data be copied).
    fn is_identity(&self) -> bool;
}

/// Positions of interesting ids within a container (beds, for 774/775).
struct Watch<'a> {
    wanted: &'a dyn Fn(u32) -> bool,
    hit: &'a mut dyn FnMut(usize),
}

/// Copies one paletted container, remapping its ids, and reports the entry
/// indices whose id `watch` wants. Unpacking only happens when a wanted id
/// can be present, so a section without beds costs a copy.
fn container(
    kind: Kind,
    remap: &dyn IdRemap,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
    mut watch: Option<&mut Watch<'_>>,
) -> WireResult<()> {
    let bits = r.u8()?;
    if bits == 0 {
        let value = read_u32(r)?;
        out.put_u8(0);
        out.put_varint(wire_id(remap.map(value)));
        if let Some(w) = watch.as_deref_mut()
            && (w.wanted)(value)
        {
            for i in 0..kind.entries() {
                (w.hit)(i);
            }
        }
        return Ok(());
    }
    if bits <= kind.max_indirect() {
        let n = r.len()?;
        if n > kind.entries() {
            return Err(WireError::BadLength);
        }
        let mut palette = Vec::with_capacity(n);
        out.put_u8(bits);
        out.put_len(n);
        for _ in 0..n {
            let id = read_u32(r)?;
            palette.push(id);
            out.put_varint(wire_id(remap.map(id)));
        }
        let storage = kind.indirect_storage(bits);
        let data = r.bytes(longs_for(kind.entries(), storage) * 8)?;
        out.put_bytes(data);
        if let Some(w) = watch.as_deref_mut()
            && palette.iter().any(|&id| (w.wanted)(id))
        {
            unpack(data, storage, kind.entries(), |i, index| {
                if palette
                    .get(index as usize)
                    .is_some_and(|&id| (w.wanted)(id))
                {
                    (w.hit)(i);
                }
            });
        }
        return Ok(());
    }
    // Direct: global ids at the sent width, rewritten at the client's width.
    let data = r.bytes(longs_for(kind.entries(), bits) * 8)?;
    let mut values = Vec::with_capacity(kind.entries());
    unpack(data, bits, kind.entries(), |i, id| {
        if let Some(w) = watch.as_deref_mut()
            && (w.wanted)(id)
        {
            (w.hit)(i);
        }
        values.push(remap.map(id));
    });
    let client_bits = remap.client_bits().max(kind.max_indirect() + 1);
    out.put_u8(client_bits);
    pack(&values, client_bits, out);
    Ok(())
}

fn read_u32(r: &mut Reader<'_>) -> WireResult<u32> {
    u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative id"))
}

/// Calls `f(index, value)` for each packed entry.
fn unpack(data: &[u8], bits: u8, entries: usize, mut f: impl FnMut(usize, u32)) {
    let per_long = 64 / bits as usize;
    let mask = (1u64 << bits) - 1;
    for (l, word) in data.as_chunks::<8>().0.iter().enumerate() {
        let word = u64::from_be_bytes(*word);
        for j in 0..per_long {
            let i = l * per_long + j;
            if i >= entries {
                return;
            }
            f(i, ((word >> (j * bits as usize)) & mask) as u32);
        }
    }
}

fn pack(values: &[u32], bits: u8, out: &mut Vec<u8>) {
    let per_long = 64 / bits as usize;
    let mask = (1u64 << bits) - 1;
    for chunk in values.chunks(per_long) {
        let mut word = 0u64;
        for (j, &v) in chunk.iter().enumerate() {
            word |= (u64::from(v) & mask) << (j * bits as usize);
        }
        out.put_i64(word as i64);
    }
    let longs = longs_for(values.len(), bits);
    for _ in values.len().div_ceil(per_long)..longs {
        out.put_i64(0);
    }
}

struct BlockRemap<'a>(&'a ViaTranslator);

impl IdRemap for BlockRemap<'_> {
    fn map(&self, id: u32) -> u32 {
        // The state table is total server->client (fallbacks included); an
        // out-of-range id from a broken backend becomes air.
        self.0.tables.block_states.s2c(id).unwrap_or(0)
    }

    fn client_bits(&self) -> u8 {
        self.0.tables.client_state_bits
    }

    fn is_identity(&self) -> bool {
        false
    }
}

struct BiomeRemap<'a> {
    t: &'a ViaTranslator,
    bits: u8,
    identity: bool,
}

impl<'a> BiomeRemap<'a> {
    fn new(t: &'a ViaTranslator) -> Self {
        let reg = t.conn.dynamic("minecraft:worldgen/biome");
        let (count, identity) = match reg {
            Some(r) => (
                r.s2c.iter().filter(|m| m.is_some()).count(),
                r.s2c
                    .iter()
                    .enumerate()
                    .all(|(i, m)| *m == u32::try_from(i).ok()),
            ),
            None => (64, true),
        };
        let bits = usize::BITS - count.saturating_sub(1).leading_zeros();
        Self {
            t,
            bits: u8::try_from(bits).unwrap_or(7),
            identity,
        }
    }
}

impl IdRemap for BiomeRemap<'_> {
    fn map(&self, id: u32) -> u32 {
        self.t
            .conn
            .dyn_s2c("minecraft:worldgen/biome", id)
            .unwrap_or(0)
    }

    fn client_bits(&self) -> u8 {
        self.bits
    }

    fn is_identity(&self) -> bool {
        self.identity
    }
}

/// Whether the client wants a bed block entity per bed block.
fn wants_bed_entities(t: &ViaTranslator) -> bool {
    matches!(t.client, Proto::V774 | Proto::V775)
}

/// The client's id of the `minecraft:bed` block entity type.
fn bed_entity_type(t: &ViaTranslator) -> Option<u32> {
    id_of(
        Reg::BlockEntityType,
        t.client.number().unsigned_abs(),
        "minecraft:bed",
    )
}

/// Writes the update tag (network NBT `raw`) of a block entity of 776 type
/// `server_type`, rewritten in codec form when its type can carry
/// release-dependent data (`codec::block_entity`). Types whose tag never
/// does, and signs, skulls and beacons whose text names nothing
/// version-specific (no hover), are copied without being parsed: a chunk
/// can hold many block entities, and the copy is what keeps a chunk's cost
/// a memcpy.
fn block_entity_tag(x: &mut Xc<'_>, server_type: u32, raw: &[u8], out: &mut Vec<u8>) {
    let Some(kind) = name_of(Reg::BlockEntityType, 776, server_type) else {
        out.put_bytes(raw);
        return;
    };
    let parse = match kind {
        "minecraft:sign" | "minecraft:hanging_sign" | "minecraft:skull" | "minecraft:beacon" => {
            crate::text::mentions_versioned_data(raw)
        }
        "minecraft:banner"
        | "minecraft:decorated_pot"
        | "minecraft:campfire"
        | "minecraft:shelf"
        | "minecraft:brushable_block"
        | "minecraft:mob_spawner" => true,
        _ => false,
    };
    if !parse {
        out.put_bytes(raw);
        return;
    }
    match nbt::Tag::read(&mut Reader::new(raw)) {
        Ok(Some(mut tag)) => {
            crate::codec::block_entity(x, kind, &mut tag);
            tag.write(out);
        }
        Ok(None) | Err(_) => out.put_bytes(raw),
    }
}

/// `level_chunk_with_light`. Translation notes (dropped or approximated
/// data) are appended to `notes` for the caller's once-per-connection log.
pub fn level_chunk(
    t: &ViaTranslator,
    body: &[u8],
    notes: &mut Vec<&'static str>,
) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let mut out = Vec::with_capacity(body.len() + 64);
    out.put_bytes(r.bytes(8)?);
    heightmaps(&mut r, &mut out)?;

    let size = r.len()?;
    let mut sections = Reader::new(r.bytes(size)?);
    let mut data = Vec::with_capacity(size + 64);
    let mut beds: Vec<(u8, i16)> = Vec::new();
    let blocks = BlockRemap(t);
    let biomes = BiomeRemap::new(t);
    let watch_beds = wants_bed_entities(t);
    let is_bed = |state: u32| t.tables.is_bed(state);
    let min_y = i32::from(t.conn.min_y);
    let mut section_index = 0i32;
    while !sections.is_empty() {
        data.put_bytes(sections.bytes(2)?);
        let fluid = sections.bytes(2)?;
        if t.client != Proto::V774 {
            data.put_bytes(fluid);
        }
        let base_y = min_y + section_index * 16;
        let mut hit = |i: usize| {
            let x = (i & 15) as u8;
            let z = ((i >> 4) & 15) as u8;
            let y = base_y + (i >> 8) as i32;
            if let Ok(y) = i16::try_from(y) {
                beds.push(((x << 4) | z, y));
            }
        };
        let mut watch = Watch {
            wanted: &is_bed,
            hit: &mut hit,
        };
        container(
            Kind::Blocks,
            &blocks,
            &mut sections,
            &mut data,
            watch_beds.then_some(&mut watch),
        )?;
        container(Kind::Biomes, &biomes, &mut sections, &mut data, None)?;
        section_index += 1;
    }
    out.put_len(data.len());
    out.put_bytes(&data);

    // Block entities.
    let n = r.len()?;
    let mut entities = Vec::new();
    let mut kept = 0usize;
    let mut x = Xc::new(t, Dir::S2c);
    for _ in 0..n {
        let xz = r.u8()?;
        let y = r.bytes(2)?;
        let server_type = read_u32(&mut r)?;
        let start = r.pos();
        nbt::skip(&mut r)?;
        let tag = r.since(start);
        if let Some(ty) = t.tables.reg(Reg::BlockEntityType).s2c(server_type) {
            entities.put_u8(xz);
            entities.put_bytes(y);
            entities.put_varint(wire_id(ty));
            block_entity_tag(&mut x, server_type, tag, &mut entities);
            kept += 1;
        } else {
            x.notes
                .push("a block entity type the client lacks was left out of a chunk");
        }
    }
    notes.append(&mut x.notes);
    if let Some(bed) = bed_entity_type(t).filter(|_| watch_beds) {
        for (xz, y) in &beds {
            entities.put_u8(*xz);
            entities.put_i16(*y);
            entities.put_varint(wire_id(bed));
            empty_compound(&mut entities);
            kept += 1;
        }
    }
    out.put_len(kept);
    out.put_bytes(&entities);

    light_data(t, &mut r, &mut out)?;
    if !r.is_empty() {
        return Err(WireError::Malformed("bytes after the light data"));
    }
    Ok(out.into())
}

fn empty_compound(out: &mut Vec<u8>) {
    out.put_u8(nbt::TAG_COMPOUND);
    out.put_u8(nbt::TAG_END);
}

fn heightmaps(r: &mut Reader<'_>, out: &mut Vec<u8>) -> WireResult<()> {
    let n = r.len()?;
    out.put_len(n);
    for _ in 0..n {
        out.put_varint(r.varint()?);
        let longs = r.len()?;
        out.put_len(longs);
        out.put_bytes(r.bytes(longs.checked_mul(8).ok_or(WireError::BadLength)?)?);
    }
    Ok(())
}

/// Light data: four masks and two lists of 2048-byte arrays.
pub fn light_data(t: &ViaTranslator, r: &mut Reader<'_>, out: &mut Vec<u8>) -> WireResult<()> {
    for _ in 0..4 {
        let longs = r.len()?;
        let raw = r.bytes(longs.checked_mul(8).ok_or(WireError::BadLength)?)?;
        if t.client == Proto::V777 {
            bitset_bytes(raw, out);
        } else {
            out.put_len(longs);
            out.put_bytes(raw);
        }
    }
    for _ in 0..2 {
        let arrays = r.len()?;
        out.put_len(arrays);
        for _ in 0..arrays {
            let len = r.len()?;
            out.put_len(len);
            out.put_bytes(r.bytes(len)?);
        }
    }
    Ok(())
}

/// `long[]` (big-endian longs, bit k in long k/64) to `BitSet.toByteArray()`
/// (bit k in byte k/8, low bit first, trailing zero bytes trimmed).
pub fn bitset_bytes(longs: &[u8], out: &mut Vec<u8>) {
    let mut bytes = Vec::with_capacity(longs.len());
    for word in longs.as_chunks::<8>().0 {
        bytes.extend_from_slice(&u64::from_be_bytes(*word).to_le_bytes());
    }
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    out.put_len(bytes.len());
    out.put_bytes(&bytes);
}

/// `light_update`: VarInt x, VarInt z, light data.
pub fn light_update(t: &ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    if t.client != Proto::V777 {
        return Ok(Bytes::copy_from_slice(body));
    }
    let mut r = Reader::new(body);
    let mut out = Vec::with_capacity(body.len());
    out.put_varint(r.varint()?);
    out.put_varint(r.varint()?);
    light_data(t, &mut r, &mut out)?;
    Ok(out.into())
}

/// `chunks_biomes`: VarInt n × { i64 chunk pos, VarInt size, biome containers }.
pub fn chunks_biomes(t: &ViaTranslator, body: &Bytes) -> WireResult<Bytes> {
    let biomes = BiomeRemap::new(t);
    if biomes.is_identity() {
        return Ok(body.clone());
    }
    let mut r = Reader::new(body);
    let mut out = Vec::with_capacity(body.len());
    let n = r.len()?;
    out.put_len(n);
    for _ in 0..n {
        out.put_bytes(r.bytes(8)?);
        let size = r.len()?;
        let mut sub = Reader::new(r.bytes(size)?);
        let mut data = Vec::with_capacity(size);
        while !sub.is_empty() {
            container(Kind::Biomes, &biomes, &mut sub, &mut data, None)?;
        }
        out.put_len(data.len());
        out.put_bytes(&data);
    }
    Ok(out.into())
}

/// `block_update`: i64 position, VarInt state. Returns the body and, for a
/// bed on a 774/775 client, a `block_entity_data` body to send after it.
pub fn block_update(t: &ViaTranslator, body: &[u8]) -> WireResult<(Bytes, Option<Bytes>)> {
    let mut r = Reader::new(body);
    let pos = r.i64()?;
    let state = read_u32(&mut r)?;
    let mut out = Vec::with_capacity(12);
    out.put_i64(pos);
    out.put_varint(wire_id(BlockRemap(t).map(state)));
    let bed = (wants_bed_entities(t) && t.tables.is_bed(state))
        .then(|| bed_entity_data(t, pos))
        .flatten();
    Ok((out.into(), bed))
}

/// `block_entity_data` body for an empty bed block entity at `pos`.
fn bed_entity_data(t: &ViaTranslator, pos: i64) -> Option<Bytes> {
    let ty = bed_entity_type(t)?;
    let mut out = Vec::with_capacity(16);
    out.put_i64(pos);
    out.put_varint(wire_id(ty));
    empty_compound(&mut out);
    Some(out.into())
}

/// `section_blocks_update`: i64 section position, VarInt n × VarLong
/// `(state << 12) | (x << 8 | z << 4 | y)`.
pub fn section_blocks_update(t: &ViaTranslator, body: &[u8]) -> WireResult<(Bytes, Vec<Bytes>)> {
    let mut r = Reader::new(body);
    let section = r.i64()?;
    let n = r.len()?;
    let mut out = Vec::with_capacity(body.len() + 8);
    out.put_i64(section);
    out.put_len(n);
    let mut beds = Vec::new();
    let remap = BlockRemap(t);
    for _ in 0..n {
        let entry = r.varlong()?;
        let state =
            u32::try_from(entry >> 12).map_err(|_| WireError::Malformed("negative block state"))?;
        let local = entry & 0xFFF;
        out.put_varlong((i64::from(remap.map(state)) << 12) | local);
        if wants_bed_entities(t) && t.tables.is_bed(state) {
            // Section position: x 22 bits, z 22 bits, y 20 bits (signed).
            let sx = section >> 42;
            let sz = (section << 22) >> 42;
            let sy = (section << 44) >> 44;
            let x = (sx << 4) + ((local >> 8) & 15);
            let z = (sz << 4) + ((local >> 4) & 15);
            let y = (sy << 4) + (local & 15);
            if let Some(b) = bed_entity_data(t, block_pos(x, y, z)) {
                beds.push(b);
            }
        }
    }
    Ok((out.into(), beds))
}

/// Packs a block position: x 26 bits, z 26 bits, y 12 bits.
#[must_use]
pub fn block_pos(x: i64, y: i64, z: i64) -> i64 {
    ((x & 0x3FF_FFFF) << 38) | ((z & 0x3FF_FFFF) << 12) | (y & 0xFFF)
}

/// `block_entity_data`: i64 position, VarInt type, NBT. A type the client
/// does not have makes the whole packet go away (`Ok(None)`, noted).
pub fn block_entity_data(
    t: &ViaTranslator,
    body: &[u8],
    notes: &mut Vec<&'static str>,
) -> WireResult<Option<Bytes>> {
    let mut r = Reader::new(body);
    let pos = r.i64()?;
    let server_type = read_u32(&mut r)?;
    let Some(ty) = t.tables.reg(Reg::BlockEntityType).s2c(server_type) else {
        notes.push("a block entity type the client lacks");
        return Ok(None);
    };
    let mut out = Vec::with_capacity(body.len());
    out.put_i64(pos);
    out.put_varint(wire_id(ty));
    let start = r.pos();
    nbt::skip(&mut r)?;
    let mut x = Xc::new(t, Dir::S2c);
    block_entity_tag(&mut x, server_type, r.since(start), &mut out);
    notes.append(&mut x.notes);
    Ok(Some(out.into()))
}

/// `block_event`: i64 position, u8 action, u8 parameter, VarInt block type.
pub fn block_event(t: &ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let mut out = Vec::with_capacity(body.len());
    out.put_bytes(r.bytes(10)?);
    let block = read_u32(&mut r)?;
    let block = t
        .tables
        .reg(Reg::Block)
        .s2c(block)
        .ok_or(WireError::Malformed("unmapped block"))?;
    out.put_varint(wire_id(block));
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn pack_and_unpack_agree_at_every_width() {
        for bits in [1u8, 4, 5, 8, 15, 16] {
            let values: Vec<u32> = (0..4096u32)
                .map(|i| (i * 7919) & ((1 << bits) - 1))
                .collect();
            let mut buf = Vec::new();
            pack(&values, bits, &mut buf);
            assert_eq!(buf.len(), longs_for(4096, bits) * 8, "{bits}");
            let mut back = Vec::new();
            unpack(&buf, bits, 4096, |_, v| back.push(v));
            assert_eq!(back, values, "{bits}");
        }
    }

    #[test]
    fn bitsets_are_little_endian_and_trimmed() {
        let mut longs = Vec::new();
        longs.put_i64(0b1010_0000_0001);
        longs.put_i64(0);
        let mut out = Vec::new();
        bitset_bytes(&longs, &mut out);
        assert_eq!(out, vec![2, 0x01, 0x0A]);
        let mut empty = Vec::new();
        bitset_bytes(&[0u8; 16], &mut empty);
        assert_eq!(empty, vec![0]);
    }

    #[test]
    fn block_positions_pack_like_vanilla() {
        // x 26 bits, z 26 bits, y 12 bits.
        assert_eq!(block_pos(1, 2, 3), (1 << 38) | (3 << 12) | 2);
        assert_eq!(block_pos(-1, -1, -1) & 0xFFF, 0xFFF);
    }
}
