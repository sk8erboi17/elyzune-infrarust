//! Debug subscription packets.
//!
//! A client that subscribes (`debug_subscription_request`, sent by the debug
//! renderers and, for operators, the tick-time chart) receives
//! `debug/block_value`, `debug/chunk_value`, `debug/entity_value` and
//! `debug/event`, each ending in a value tagged by its subscription:
//!
//! ```text
//! Update  VarInt subscription, Optional<value>    (block, chunk, entity value)
//! Event   VarInt subscription, value              (debug/event)
//! ```
//!
//! The `minecraft:debug_subscription` registry and every value codec are
//! the same from 1.21.11 to 26.3 (decompiled `DebugSubscriptions` and the
//! `Debug*Info` classes of the four releases; 26.3 rewrote two codecs as
//! composites without changing a byte). Only registry ids inside three
//! values differ and are renumbered:
//!
//! ```text
//! game_events  GameEvent id, Vec3     (26.2 inserted `bounce`)
//! bee_hives    Block id, VarInt, VarInt, Bool
//! pois         BlockPos, PointOfInterestType id, VarInt
//! ```
//!
//! A value naming an entry the client lacks is dropped with its packet
//! (logged once). `debug_sample` (the tick-time chart) is identical in every
//! release and passes through.

use crate::generated::registries::Reg;
use crate::layout::{R, W, Xc, Xerr, Xresult};
use crate::wire::{Put, Reader, WireError};

/// `minecraft:debug_subscription` by id, identical in 1.21.11 to 26.3
/// (`registries.json` of each release).
const SUBSCRIPTIONS: [&str; 16] = [
    "dedicated_server_tick_time",
    "bees",
    "brains",
    "breezes",
    "goal_selectors",
    "entity_paths",
    "entity_block_intersections",
    "bee_hives",
    "pois",
    "redstone_wire_orientations",
    "village_sections",
    "raids",
    "structures",
    "game_event_listeners",
    "neighbor_updates",
    "game_events",
];

/// The rest of a debug value body after a prefix of `prefix` bytes (the
/// block position, chunk position or entity id), with the value rewritten.
/// `optional` is true for updates, false for events.
pub fn value_packet(
    x: &mut Xc<'_>,
    body: &[u8],
    prefix: Prefix,
    optional: bool,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    let mut r = Reader::new(body);
    match prefix {
        Prefix::Long => out.put_bytes(r.bytes(8)?),
        Prefix::VarInt => out.put_varint(r.varint()?),
        Prefix::None => {}
    }
    let subscription = r.varint()?;
    out.put_varint(subscription);
    let name = usize::try_from(subscription)
        .ok()
        .and_then(|i| SUBSCRIPTIONS.get(i))
        .ok_or(Xerr::Wire(WireError::Malformed(
            "unknown debug subscription",
        )))?;
    if optional {
        let present = r.bool()?;
        out.put_bool(present);
        if !present {
            return Ok(());
        }
    }
    value(x, name, &mut r, out)
}

/// What precedes the subscription in a debug value packet.
#[derive(Clone, Copy)]
pub enum Prefix {
    /// `debug/block_value` (BlockPos) and `debug/chunk_value` (ChunkPos).
    Long,
    /// `debug/entity_value`: the entity id.
    VarInt,
    /// `debug/event`.
    None,
}

fn value(x: &mut Xc<'_>, subscription: &str, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    match subscription {
        "game_events" => {
            x.value(&W::Reg(R::Static(Reg::GameEvent)), r, out)?;
            out.put_bytes(r.bytes(24)?);
        }
        "bee_hives" => {
            x.value(&W::Reg(R::Static(Reg::Block)), r, out)?;
            out.put_bytes(r.rest());
        }
        "pois" => {
            out.put_bytes(r.bytes(8)?);
            x.value(&W::Reg(R::Static(Reg::PointOfInterestType)), r, out)?;
            out.put_bytes(r.rest());
        }
        _ => out.put_bytes(r.rest()),
    }
    Ok(())
}
