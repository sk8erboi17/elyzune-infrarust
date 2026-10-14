//! Play packets whose layout 26.3 (777) changed, for the packets not already
//! handled in `play.rs` for every client.
//!
//! Sources: the 26.2 and 26.3 server jars; the reshapes match ViaVersion's
//! `Protocol26_2To26_3`, which this module follows where vanilla leaves a
//! choice (the swing animation's duration, the synthesised movement after a
//! teleport confirmation).

use bytes::Bytes;
use infrarust_api::filter::TranslatorOutput;

use crate::generated::packets::{PlayCb, PlaySb};
use crate::session::ViaTranslator;
use crate::wire::{Put, Reader, WireError, WireResult};

/// Swing animation type `whack` and its default duration in ticks.
const SWING_WHACK: i32 = 1;
const SWING_TICKS: i32 = 6;

pub fn clientbound(
    t: &mut ViaTranslator,
    kind: PlayCb,
    body: Bytes,
    out: &mut TranslatorOutput,
) -> WireResult<Option<Bytes>> {
    Ok(match kind {
        // 26.2: VarInt id, i16 dx/dy/dz, Bool on ground. 26.3: VarInt id,
        // VarInt properties (step count << 1 | on ground), then for step
        // count 0 the linear delta (three i16).
        PlayCb::MoveEntityPos => {
            let mut r = Reader::new(&body);
            let mut o = Vec::with_capacity(body.len() + 1);
            o.put_varint(r.varint()?);
            let delta = r.bytes(6)?;
            o.put_varint(i32::from(r.bool()?));
            o.put_bytes(delta);
            Some(o.into())
        }
        // 26.2: VarInt id, i16 × 3, i8 yaw, i8 pitch, Bool on ground.
        // 26.3: VarInt id, VarInt properties, delta, i8 yaw, i8 pitch.
        PlayCb::MoveEntityPosRot => {
            let mut r = Reader::new(&body);
            let mut o = Vec::with_capacity(body.len() + 1);
            o.put_varint(r.varint()?);
            let delta = r.bytes(6)?;
            let rot = r.bytes(2)?;
            o.put_varint(i32::from(r.bool()?));
            o.put_bytes(delta);
            o.put_bytes(rot);
            Some(o.into())
        }
        // 26.2: VarInt id, i8 yaw, i8 pitch, Bool. 26.3: VarInt id, Bool,
        // i8 yaw, i8 pitch.
        PlayCb::MoveEntityRot => {
            let mut r = Reader::new(&body);
            let mut o = Vec::with_capacity(body.len());
            o.put_varint(r.varint()?);
            let rot = r.bytes(2)?;
            o.put_u8(r.u8()?);
            o.put_bytes(rot);
            Some(o.into())
        }
        // 26.2: VarInt id, f64 x/y/z, f64 dx/dy/dz, f32 yaw, f32 pitch, Bool.
        // 26.3: VarInt id, VarInt path type (0 linear), f64 x/y/z, f32 yaw,
        // f32 pitch, Bool. The delta has no 26.3 counterpart and is dropped.
        PlayCb::EntityPositionSync => {
            let mut r = Reader::new(&body);
            let mut o = Vec::with_capacity(body.len());
            o.put_varint(r.varint()?);
            o.put_varint(0);
            o.put_bytes(r.bytes(24)?);
            r.skip(24)?;
            o.put_bytes(r.bytes(9)?);
            Some(o.into())
        }
        // 26.2 animate actions: 0 swing main, 2 wake up, 3 swing offhand,
        // 4 crit, 5 magic crit. 26.3: 0 wake up, 1 crit, 2 magic crit; swings
        // are `swing_animation` (VarInt id, VarInt hand, VarInt type, VarInt
        // duration).
        PlayCb::Animate => {
            let mut r = Reader::new(&body);
            let entity = r.varint()?;
            let action = r.u8()?;
            let swing_hand = match action {
                0 => Some(0),
                3 => Some(1),
                _ => None,
            };
            if let Some(hand) = swing_hand {
                let mut o = Vec::with_capacity(8);
                o.put_varint(entity);
                o.put_varint(hand);
                o.put_varint(SWING_WHACK);
                o.put_varint(SWING_TICKS);
                t.emit_play_cb(PlayCb::SwingAnimation, o.into(), out);
                return Ok(None);
            }
            let mapped = match action {
                2 => 0,
                4 => 1,
                5 => 2,
                _ => {
                    t.drop_unsupported(kind.name(), "an animation 26.3 no longer has");
                    return Ok(None);
                }
            };
            let mut o = Vec::with_capacity(6);
            o.put_varint(entity);
            o.put_u8(mapped);
            Some(o.into())
        }
        _ => Some(body),
    })
}

pub fn serverbound(
    t: &mut ViaTranslator,
    kind: PlaySb,
    body: Bytes,
    out: &mut TranslatorOutput,
) -> WireResult<Option<Bytes>> {
    Ok(match kind {
        // 26.3: VarInt id, f64 x/y/z, f32 yaw, f32 pitch. 26.2 takes the id
        // alone and expects the client's own move packet to follow, which a
        // 26.3 client no longer sends; it is synthesised from the same data.
        PlaySb::AcceptTeleportation => {
            let mut r = Reader::new(&body);
            let id = r.varint()?;
            let position = r.bytes(24)?;
            let rotation = r.bytes(8)?;
            let mut accept = Vec::with_capacity(5);
            accept.put_varint(id);
            t.emit_play_sb(kind, accept.into(), out);
            let mut moved = Vec::with_capacity(33);
            moved.put_bytes(position);
            moved.put_bytes(rotation);
            moved.put_u8(0);
            t.emit_play_sb(PlaySb::MovePlayerPosRot, moved.into(), out);
            None
        }
        // 26.3 `punch` (no body) is 26.2's `swing` with the main hand.
        PlaySb::Punch => {
            let mut o = Vec::with_capacity(1);
            o.put_varint(0);
            t.emit_play_sb(PlaySb::Swing, o.into(), out);
            None
        }
        // 26.3 inserted CHANGE_DESTROY_DIRECTION (1) into the action enum:
        // ids above it are 26.2's plus one, and 1 itself has no 26.2 meaning.
        PlaySb::PlayerAction => {
            let mut r = Reader::new(&body);
            let action = r.varint()?;
            let mapped = match action {
                0 => 0,
                1 => {
                    t.drop_unsupported(
                        kind.name(),
                        "change_destroy_direction has no 26.2 equivalent",
                    );
                    return Ok(None);
                }
                n if n > 1 => n - 1,
                _ => return Err(WireError::Malformed("negative player action")),
            };
            let mut o = Vec::with_capacity(body.len());
            o.put_varint(mapped);
            o.put_bytes(r.rest());
            Some(o.into())
        }
        // 26.3: position, 4 × String, VarInt slot (1 front, 0 back).
        // 26.2: position, Bool front, 4 × String.
        PlaySb::SignUpdate => {
            let mut r = Reader::new(&body);
            let pos = r.bytes(8)?;
            let mut lines = Vec::new();
            for _ in 0..4 {
                lines.put_str_bytes(r.str_bytes()?);
            }
            let front = r.varint()? == 1;
            let mut o = Vec::with_capacity(body.len());
            o.put_bytes(pos);
            o.put_bool(front);
            o.put_bytes(&lines);
            Some(o.into())
        }
        _ => Some(body),
    })
}
