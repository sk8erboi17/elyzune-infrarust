//! Particles: type id and option data.
//!
//! `ParticleTypes.STREAM_CODEC` is a VarInt particle type followed by the
//! option data of that type's `ParticleOptions` class (generated per release
//! in `generated/particles.rs`). The option layouts, from the releases'
//! sources, are identical from 1.21.11 to 26.3 except the item particle's
//! stack, which is an `ItemStack` (count first) in 1.21.11 and an
//! `ItemStackTemplate` (id first) from 26.1:
//!
//! ```text
//! Block (block, falling_dust, ...)  VarInt block state
//! Item                              ItemStack | ItemStackTemplate
//! Dust                              i32 color, f32 scale
//! DustColorTransition               i32 from, i32 to, f32 scale
//! Spell (effect, instant_effect)    i32 color, f32 power
//! Color (entity_effect, flash, ...) i32 color
//! Power (dragon_breath)             f32 power
//! SculkCharge                       f32 roll
//! Shriek                            VarInt delay
//! Trail                             f64 x, y, z, i32 color, VarInt duration
//! Vibration                         PositionSource, VarInt ticks
//! Geyser                            i32 water blocks            (26.2+)
//! GeyserBase                        i32 water blocks, f32 impulse (26.2+)
//! ```
//!
//! A particle the client lacks is shown as its generated fallback; when the
//! fallback takes different option data, the source data is read and dropped
//! and the fallback's is written with neutral values.

use crate::generated::Proto;
use crate::generated::particles::Opt;
use crate::generated::registries::{Reg, name_of};
use crate::layout::{Dir, Xc, Xerr, Xresult};
use crate::tables::particle_options;
use crate::wire::{Put, Reader, WireError, wire_id};

/// One particle: type id and option data.
pub fn particle(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let src_type = u32::try_from(r.varint()?)
        .map_err(|_| Xerr::Wire(WireError::Malformed("negative particle id")))?;
    let src_kind = kind(x.src_proto(), src_type)?;
    let dst_type = x.map_static(Reg::ParticleType, src_type);
    let Some(dst_type) = dst_type else {
        skip_option(x, src_kind, r)?;
        return Err(Xerr::Absent("particle"));
    };
    let dst_kind = kind(x.dst_proto(), dst_type)?;
    out.put_varint(wire_id(dst_type));
    if src_kind == dst_kind {
        option(x, src_kind, r, out)
    } else {
        skip_option(x, src_kind, r)?;
        neutral_option(dst_kind, out);
        Ok(())
    }
}

fn kind(proto: Proto, id: u32) -> Xresult<Opt> {
    particle_options(proto)
        .get(id as usize)
        .copied()
        .ok_or(Xerr::Wire(WireError::Malformed("unknown particle type")))
}

fn skip_option(x: &mut Xc<'_>, kind: Opt, r: &mut Reader<'_>) -> Xresult<()> {
    let mut sink = Vec::new();
    x.with_identity(|x| option(x, kind, r, &mut sink))
}

/// Option data for a fallback particle whose source data does not fit.
fn neutral_option(kind: Opt, out: &mut Vec<u8>) {
    match kind {
        Opt::Simple => {}
        Opt::BlockParticleOption => out.put_varint(1),
        Opt::ItemParticleOption => out.put_varint(0),
        Opt::DustParticleOptions => {
            out.put_i32(0x00FF_FFFF);
            out.put_f32(1.0);
        }
        Opt::DustColorTransitionOptions => {
            out.put_i32(0x00FF_FFFF);
            out.put_i32(0x00FF_FFFF);
            out.put_f32(1.0);
        }
        Opt::SpellParticleOption => {
            out.put_i32(-1);
            out.put_f32(1.0);
        }
        Opt::ColorParticleOption => out.put_i32(-1),
        Opt::PowerParticleOption | Opt::SculkChargeParticleOptions => out.put_f32(0.0),
        Opt::ShriekParticleOption => out.put_varint(0),
        Opt::TrailParticleOption => {
            for _ in 0..3 {
                out.put_f64(0.0);
            }
            out.put_i32(-1);
            out.put_varint(1);
        }
        Opt::VibrationParticleOption => {
            // block position source at the origin, arriving in one tick
            out.put_varint(0);
            out.put_i64(0);
            out.put_varint(1);
        }
        Opt::GeyserParticleOptions => out.put_i32(1),
        Opt::GeyserBaseParticleOptions => {
            out.put_i32(1);
            out.put_f32(1.0);
        }
    }
}

fn option(x: &mut Xc<'_>, kind: Opt, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    match kind {
        Opt::Simple => {}
        Opt::BlockParticleOption => {
            let state = u32::try_from(r.varint()?)
                .map_err(|_| Xerr::Wire(WireError::Malformed("negative block state")))?;
            let mapped = block_state(x, state).ok_or(Xerr::Absent("block state"))?;
            out.put_varint(wire_id(mapped));
        }
        Opt::ItemParticleOption => item_option(x, r, out)?,
        Opt::DustParticleOptions | Opt::SpellParticleOption | Opt::GeyserBaseParticleOptions => {
            out.put_bytes(r.bytes(8)?);
        }
        Opt::DustColorTransitionOptions => out.put_bytes(r.bytes(12)?),
        Opt::ColorParticleOption
        | Opt::PowerParticleOption
        | Opt::SculkChargeParticleOptions
        | Opt::GeyserParticleOptions => out.put_bytes(r.bytes(4)?),
        Opt::ShriekParticleOption => out.put_varint(r.varint()?),
        Opt::TrailParticleOption => {
            out.put_bytes(r.bytes(28)?);
            out.put_varint(r.varint()?);
        }
        Opt::VibrationParticleOption => {
            position_source(x, r, out)?;
            out.put_varint(r.varint()?);
        }
    }
    Ok(())
}

/// Block states only travel server to client; the table is total there.
pub fn block_state(x: &Xc<'_>, state: u32) -> Option<u32> {
    match x.dir {
        Dir::S2c if !x.is_identity() => x.t.tables.block_states.s2c(state),
        _ => Some(state),
    }
}

/// The item of an item particle: a stack in 1.21.11, a template from 26.1.
fn item_option(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let src_is_stack = x.src_proto() == Proto::V774;
    let dst_is_stack = x.dst_proto() == Proto::V774;
    // Read into (id, count, patch bytes) in the destination's numbering.
    let (id, count) = if src_is_stack {
        let count = r.varint()?;
        if count <= 0 {
            return Err(Xerr::Absent("empty item particle"));
        }
        (r.varint()?, count)
    } else {
        let id = r.varint()?;
        (id, r.varint()?)
    };
    let mut patch = Vec::new();
    crate::item::patch(x, r, &mut patch, false, u32::try_from(id).ok())?;
    let id = u32::try_from(id).map_err(|_| Xerr::Wire(WireError::Malformed("negative item id")))?;
    let id = x.map_static(Reg::Item, id).ok_or(Xerr::Absent("item"))?;
    if dst_is_stack {
        out.put_varint(count.max(1));
        out.put_varint(wire_id(id));
    } else {
        out.put_varint(wire_id(id));
        out.put_varint(count);
    }
    out.put_bytes(&patch);
    Ok(())
}

/// `PositionSource`: a `position_source_type` id, then a block position
/// (`minecraft:block`) or an entity id and y offset (`minecraft:entity`).
fn position_source(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let ty = u32::try_from(r.varint()?)
        .map_err(|_| Xerr::Wire(WireError::Malformed("negative position source")))?;
    let name = name_of(
        Reg::PositionSourceType,
        x.src_proto().number().unsigned_abs(),
        ty,
    )
    .ok_or(Xerr::Wire(WireError::Malformed("unknown position source")))?;
    let mapped = x
        .map_static(Reg::PositionSourceType, ty)
        .ok_or(Xerr::Absent("position source"))?;
    out.put_varint(wire_id(mapped));
    match name {
        "minecraft:block" => out.put_bytes(r.bytes(8)?),
        "minecraft:entity" => {
            out.put_varint(r.varint()?);
            out.put_bytes(r.bytes(4)?);
        }
        _ => return Err(Xerr::Absent("position source of an unknown kind")),
    }
    Ok(())
}
