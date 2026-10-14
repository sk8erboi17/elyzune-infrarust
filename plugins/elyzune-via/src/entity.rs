//! Entities: spawns, entity data, and the movement packets 26.3 reshaped.
//!
//! # Entity data
//!
//! `set_entity_data` is `VarInt entity, n × { u8 index, VarInt serializer,
//! value }, 0xFF`. Two things change between releases:
//!
//! - indices, when any ancestor class of the entity gained or lost a field
//!   (generated per entity type in `generated/entity_data.rs`); an entry the
//!   client's layout does not have is dropped;
//! - serializer ids (26.1 inserted the sound-variant serializers) and the ids
//!   inside values (items, block states, particles, variants).
//!
//! The index remap needs the entity's type, which only `add_entity` carries,
//! so the translator remembers each entity's type until `remove_entities` or a
//! respawn/reconfiguration.

use bytes::Bytes;

use crate::generated::Proto;
use crate::generated::entity_data::Ser;
use crate::generated::registries::{Reg, id_of};
use crate::item;
use crate::layout::{Dir, R, W, Xc, Xerr, Xresult};
use crate::session::ViaTranslator;
use crate::tables::serializers;
use crate::wire::{Put, Reader, WireError, WireResult, wire_id};

/// Skips an `LpVec3` (low-precision vector): one zero byte for the zero
/// vector, otherwise six bytes and a VarInt scale extension when bit 2 of the
/// first byte is set.
pub fn copy_lp_vec3(r: &mut Reader<'_>, out: &mut Vec<u8>) -> WireResult<()> {
    let first = r.u8()?;
    out.put_u8(first);
    if first == 0 {
        return Ok(());
    }
    out.put_bytes(r.bytes(5)?);
    if first & 4 != 0 {
        out.put_varint(r.varint()?);
    }
    Ok(())
}

/// Encodes a vector the way `LpVec3.write` does (26.1+).
pub fn write_lp_vec3(out: &mut Vec<u8>, x: f64, y: f64, z: f64) {
    const ABS_MAX: f64 = 1.717_986_918_3E10;
    let sanitize = |v: f64| {
        if v.is_nan() {
            0.0
        } else {
            v.clamp(-ABS_MAX, ABS_MAX)
        }
    };
    let (x, y, z) = (sanitize(x), sanitize(y), sanitize(z));
    let longest = x.abs().max(y.abs()).max(z.abs());
    if longest < 3.051_944_088_384_301E-5 {
        out.put_u8(0);
        return;
    }
    let scale = longest.ceil() as i64;
    let partial = (scale & 3) != scale;
    let markers = if partial { (scale & 3) | 4 } else { scale };
    let pack = |v: f64| ((v / scale as f64 * 0.5 + 0.5) * 32766.0).round() as i64;
    let buffer = markers | (pack(x) << 3) | (pack(y) << 18) | (pack(z) << 33);
    out.put_u8(buffer as u8);
    out.put_u8((buffer >> 8) as u8);
    out.put_i32((buffer >> 16) as i32);
    if partial {
        out.put_varint((scale >> 2) as i32);
    }
}

fn read_u32(r: &mut Reader<'_>) -> WireResult<u32> {
    u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative id"))
}

/// `add_entity`: VarInt id, UUID, VarInt type, f64 x/y/z, LpVec3 motion,
/// i8 pitch/yaw/head yaw, VarInt data. The data of a falling block is its
/// block state and is remapped too.
pub fn add_entity(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let id = r.varint()?;
    let uuid = r.bytes(16)?;
    let server_type = read_u32(&mut r)?;
    let client_type = t
        .tables
        .reg(Reg::EntityType)
        .s2c(server_type)
        .ok_or(WireError::Malformed("entity type without equivalent"))?;
    if t.conn.track_entity(id, server_type) {
        t.drop_unsupported(
            "minecraft:add_entity",
            "too many tracked entities; later entity data keeps server indices",
        );
    }
    let mut out = Vec::with_capacity(body.len() + 2);
    out.put_varint(id);
    out.put_bytes(uuid);
    out.put_varint(wire_id(client_type));
    out.put_bytes(r.bytes(24)?);
    copy_lp_vec3(&mut r, &mut out)?;
    out.put_bytes(r.bytes(3)?);
    let data = r.varint()?;
    let falling_block = id_of(Reg::EntityType, 776, "minecraft:falling_block");
    if Some(server_type) == falling_block && t.client != Proto::V776 {
        let state = u32::try_from(data).unwrap_or(0);
        out.put_varint(wire_id(t.tables.block_states.s2c(state).unwrap_or(0)));
    } else {
        out.put_varint(data);
    }
    Ok(out.into())
}

/// `remove_entities`: VarInt n × VarInt id. Forgets their types.
pub fn remove_entities(t: &mut ViaTranslator, body: &[u8]) -> WireResult<()> {
    let mut r = Reader::new(body);
    let n = r.len()?;
    for _ in 0..n {
        t.conn.entities.remove(&r.varint()?);
    }
    Ok(())
}

/// Layout of a value that shares its codec with an item component.
fn component_layout(name: &str) -> Option<&'static W> {
    crate::generated::components::COMPONENTS_776
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, w)| w)
}

/// One entity data value of serializer `ser`, read in 776 and written for the
/// client.
fn value(x: &mut Xc<'_>, ser: Ser, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    use Ser as S;
    match ser {
        S::BYTE | S::BOOLEAN => out.put_u8(r.u8()?),
        S::INT
        | S::DIRECTION
        | S::OPTIONAL_UNSIGNED_INT
        | S::POSE
        | S::SNIFFER_STATE
        | S::ARMADILLO_STATE
        | S::COPPER_GOLEM_STATE
        | S::WEATHERING_COPPER_STATE
        | S::HUMANOID_ARM
        | S::DYE_COLOR => out.put_varint(r.varint()?),
        S::LONG => out.put_varlong(r.varlong()?),
        S::FLOAT => out.put_bytes(r.bytes(4)?),
        S::STRING => out.put_str_bytes(r.str_bytes()?),
        S::COMPONENT => x.value(&W::Text, r, out)?,
        S::OPTIONAL_COMPONENT => x.value(&W::Opt(&W::Text), r, out)?,
        S::ITEM_STACK => item::optional_stack(x, r, out)?,
        S::ROTATIONS | S::VECTOR3 => out.put_bytes(r.bytes(12)?),
        S::QUATERNION => out.put_bytes(r.bytes(16)?),
        S::BLOCK_POS => out.put_bytes(r.bytes(8)?),
        S::OPTIONAL_BLOCK_POS => x.value(&W::Opt(&W::I64), r, out)?,
        S::OPTIONAL_LIVING_ENTITY_REFERENCE => x.value(&W::Opt(&W::Uuid), r, out)?,
        S::BLOCK_STATE => {
            let state = u32::try_from(r.varint()?)
                .map_err(|_| Xerr::Wire(WireError::Malformed("negative block state")))?;
            let mapped =
                crate::particle::block_state(x, state).ok_or(Xerr::Absent("block state"))?;
            out.put_varint(wire_id(mapped));
        }
        S::OPTIONAL_BLOCK_STATE => {
            // 0 is "no state" (air would be the state 0 too, never sent here).
            let state = u32::try_from(r.varint()?)
                .map_err(|_| Xerr::Wire(WireError::Malformed("negative block state")))?;
            let mapped = if state == 0 {
                0
            } else {
                crate::particle::block_state(x, state).ok_or(Xerr::Absent("block state"))?
            };
            out.put_varint(wire_id(mapped));
        }
        S::PARTICLE => crate::particle::particle(x, r, out)?,
        S::PARTICLES => x.value(
            &W::List(&W::Custom(crate::layout::Custom::Particle)),
            r,
            out,
        )?,
        S::VILLAGER_DATA => x.value(
            &W::Struct(&[
                W::Reg(R::Static(Reg::VillagerType)),
                W::Reg(R::Static(Reg::VillagerProfession)),
                W::VarInt,
            ]),
            r,
            out,
        )?,
        S::OPTIONAL_GLOBAL_POS => x.value(&W::Opt(&W::Struct(&[W::Str, W::I64])), r, out)?,
        S::CAT_VARIANT => component(x, "minecraft:cat/variant", r, out)?,
        S::CAT_SOUND_VARIANT => component(x, "minecraft:cat/sound_variant", r, out)?,
        S::COW_VARIANT => component(x, "minecraft:cow/variant", r, out)?,
        S::COW_SOUND_VARIANT => component(x, "minecraft:cow/sound_variant", r, out)?,
        S::WOLF_VARIANT => component(x, "minecraft:wolf/variant", r, out)?,
        S::WOLF_SOUND_VARIANT => component(x, "minecraft:wolf/sound_variant", r, out)?,
        S::FROG_VARIANT => component(x, "minecraft:frog/variant", r, out)?,
        S::PIG_VARIANT => component(x, "minecraft:pig/variant", r, out)?,
        S::PIG_SOUND_VARIANT => component(x, "minecraft:pig/sound_variant", r, out)?,
        S::CHICKEN_VARIANT => component(x, "minecraft:chicken/variant", r, out)?,
        S::CHICKEN_SOUND_VARIANT => component(x, "minecraft:chicken/sound_variant", r, out)?,
        S::ZOMBIE_NAUTILUS_VARIANT => component(x, "minecraft:zombie_nautilus/variant", r, out)?,
        S::PAINTING_VARIANT => component(x, "minecraft:painting/variant", r, out)?,
        S::RESOLVABLE_PROFILE => component(x, "minecraft:profile", r, out)?,
    }
    Ok(())
}

/// A value whose codec is the same as an item component's: the variant
/// serializers and the resolvable profile.
fn component(x: &mut Xc<'_>, name: &str, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let layout = component_layout(name).ok_or(Xerr::Absent("serializer without a layout"))?;
    x.value(layout, r, out)
}

/// `set_entity_data`.
pub fn set_entity_data(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Option<Bytes>> {
    let mut r = Reader::new(body);
    let entity = r.varint()?;
    let index_map = t
        .conn
        .entities
        .get(&entity)
        .and_then(|ty| t.tables.entity_index.get(ty))
        .copied();
    let server_sers = serializers(Proto::V776);
    let mut out = Vec::with_capacity(body.len());
    out.put_varint(entity);
    let mut x = Xc::new(t, Dir::S2c);
    let mut written = 0usize;
    loop {
        let index = r.u8()?;
        if index == 0xFF {
            break;
        }
        let ser_id = usize::try_from(r.varint()?)
            .map_err(|_| WireError::Malformed("negative serializer"))?;
        let ser = *server_sers
            .get(ser_id)
            .ok_or(WireError::Malformed("unknown serializer"))?;
        let client_index = match index_map {
            Some(map) => map.get(usize::from(index)).copied().unwrap_or(index),
            None => index,
        };
        let client_ser = t.tables.serializer_s2c.get(ser_id).copied().flatten();
        let mut val = Vec::new();
        let mut probe = r.clone();
        match value(&mut x, ser, &mut probe, &mut val) {
            Ok(()) => r = probe,
            Err(Xerr::Wire(e)) => return Err(e),
            Err(Xerr::Absent(_)) => {
                // Step over the value without remapping, then drop it.
                let mut sink = Vec::new();
                x.with_identity(|x| value(x, ser, &mut r, &mut sink))
                    .map_err(|e| match e {
                        Xerr::Wire(e) => e,
                        Xerr::Absent(_) => WireError::Malformed("entity data value"),
                    })?;
                continue;
            }
        }
        let (Some(client_ser), true) = (client_ser, client_index != 0xFF) else {
            continue;
        };
        out.put_u8(client_index);
        out.put_varint(wire_id(client_ser));
        out.put_bytes(&val);
        written += 1;
    }
    let notes = std::mem::take(&mut x.notes);
    for note in notes {
        t.drop_unsupported("minecraft:set_entity_data", note);
    }
    if written == 0 {
        return Ok(None);
    }
    out.put_u8(0xFF);
    Ok(Some(out.into()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lp_vec3_write_matches_its_own_skip() {
        for v in [
            (0.0, 0.0, 0.0),
            (0.1, -0.2, 0.3),
            (5.0, 0.0, -7.5),
            (123.0, 4.0, 0.0),
        ] {
            let mut buf = Vec::new();
            write_lp_vec3(&mut buf, v.0, v.1, v.2);
            buf.push(0xAB);
            let mut r = Reader::new(&buf);
            let mut copy = Vec::new();
            copy_lp_vec3(&mut r, &mut copy).unwrap();
            assert_eq!(r.rest(), &[0xAB]);
        }
    }

    #[test]
    fn lp_vec3_matches_vanilla_for_a_unit_vector() {
        // LpVec3.write(Vec3(1, 0, 0)): scale 1, x=32766, y=z=16383.
        let mut buf = Vec::new();
        write_lp_vec3(&mut buf, 1.0, 0.0, 0.0);
        let buffer: i64 = 1 | (32766 << 3) | (16383 << 18) | (16383i64 << 33);
        let mut expected = vec![buffer as u8, (buffer >> 8) as u8];
        expected.extend_from_slice(&((buffer >> 16) as i32).to_be_bytes());
        assert_eq!(buf, expected);
    }
}
