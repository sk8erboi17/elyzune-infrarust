//! Play state.
//!
//! Every packet is first identified by name in the sending protocol. The
//! packets below whose body is identical in the four releases and carries no
//! registry id are only re-identified (the `_ =>` arms); the others are
//! rewritten by the functions in this module or in `chunk`, `entity`, `item`
//! and `particle`. Layouts are quoted from the releases' sources next to each
//! handler.

use bytes::Bytes;
use infrarust_api::filter::TranslatorOutput;

use crate::chunk;
use crate::entity;
use crate::generated::Proto;
use crate::generated::packets::{PlayCb, PlaySb};
use crate::generated::registries::{Reg, id_of, name_of};
use crate::item;
use crate::layout::{Dir, R, W, Xc, Xerr};
use crate::session::ViaTranslator;
use crate::wire::{Put, Reader, WireError, WireResult, wire_id};

use super::text_packets;

/// Inline `ChatType`: chat and narration decorations (translation key,
/// parameter list, style).
static CHAT_TYPE: W = W::Holder(
    R::Dynamic("minecraft:chat_type"),
    &W::Struct(&[
        W::Struct(&[W::Str, W::List(&W::VarInt), W::Text]),
        W::Struct(&[W::Str, W::List(&W::VarInt), W::Text]),
    ]),
);

static SOUND: W = W::Holder(
    R::Static(Reg::SoundEvent),
    &W::Struct(&[W::Str, W::Opt(&W::F32)]),
);

/// `update_attributes` entries: attribute, base value, modifiers (id, amount,
/// operation).
static ATTRIBUTE_SNAPSHOTS: W = W::List(&W::Struct(&[
    W::Reg(R::Static(Reg::Attribute)),
    W::F64,
    W::List(&W::Struct(&[W::Str, W::F64, W::VarInt])),
]));

/// `explode` block particles: particle, scaling, speed, weight.
static BLOCK_PARTICLES: W = W::List(&W::Struct(&[
    W::Custom(crate::layout::Custom::Particle),
    W::F32,
    W::F32,
    W::VarInt,
]));

/// `ItemCost`: item, count, exact component predicate.
static ITEM_COST: W = W::Struct(&[
    W::Reg(R::Static(Reg::Item)),
    W::VarInt,
    W::List(&W::Custom(crate::layout::Custom::TypedComponent)),
]);
static OPTIONAL_ITEM_COST: W = W::Opt(&ITEM_COST);

/// Runs `f` over a fresh transcoder and turns the outcome into a packet body:
/// `Absent` (an id with no equivalent at the top level) drops the packet.
pub(crate) fn transcode(
    t: &mut ViaTranslator,
    kind: &'static str,
    dir: Dir,
    f: impl FnOnce(&mut Xc<'_>, &mut Vec<u8>) -> Result<(), Xerr>,
) -> WireResult<Option<Bytes>> {
    let mut out = Vec::new();
    let mut x = Xc::new(t, dir);
    let result = f(&mut x, &mut out);
    let notes = std::mem::take(&mut x.notes);
    for note in notes {
        t.drop_unsupported(kind, note);
    }
    match result {
        Ok(()) => Ok(Some(out.into())),
        Err(Xerr::Absent(why)) => {
            t.drop_unsupported(kind, why);
            Ok(None)
        }
        Err(Xerr::Wire(e)) => Err(e),
    }
}

pub fn clientbound(
    t: &mut ViaTranslator,
    kind: PlayCb,
    body: Bytes,
    out: &mut TranslatorOutput,
) -> WireResult<()> {
    let client = t.client;
    let body: Option<Bytes> = match kind {
        PlayCb::Login => Some(login(t, &body)?),
        PlayCb::Respawn => Some(respawn(t, &body)?),
        PlayCb::GameEvent => {
            track_game_event(t, &body);
            Some(body)
        }
        PlayCb::LevelChunkWithLight => {
            let mut notes = Vec::new();
            let chunk = chunk::level_chunk(t, &body, &mut notes);
            for note in notes {
                t.drop_unsupported(kind.name(), note);
            }
            Some(chunk?)
        }
        PlayCb::LightUpdate => Some(chunk::light_update(t, &body)?),
        PlayCb::ChunksBiomes => Some(chunk::chunks_biomes(t, &body)?),
        PlayCb::BlockUpdate => {
            let (update, bed) = chunk::block_update(t, &body)?;
            t.emit_play_cb(kind, update, out);
            if let Some(bed) = bed {
                t.emit_play_cb(PlayCb::BlockEntityData, bed, out);
            }
            return Ok(());
        }
        PlayCb::SectionBlocksUpdate => {
            let (update, beds) = chunk::section_blocks_update(t, &body)?;
            t.emit_play_cb(kind, update, out);
            for bed in beds {
                t.emit_play_cb(PlayCb::BlockEntityData, bed, out);
            }
            return Ok(());
        }
        PlayCb::BlockEntityData => {
            let mut notes = Vec::new();
            let data = chunk::block_entity_data(t, &body, &mut notes);
            for note in notes {
                t.drop_unsupported(kind.name(), note);
            }
            data?
        }
        PlayCb::BlockEvent => Some(chunk::block_event(t, &body)?),
        PlayCb::LevelEvent => Some(level_event(t, &body)?),
        PlayCb::AddEntity => Some(entity::add_entity(t, &body)?),
        PlayCb::RemoveEntities => {
            entity::remove_entities(t, &body)?;
            Some(body)
        }
        PlayCb::SetEntityData => entity::set_entity_data(t, &body)?,
        // Slot stacks: translated, and recorded when the client's claims
        // about them will need rewriting (`hashed.rs`).
        PlayCb::ContainerSetContent
        | PlayCb::ContainerSetSlot
        | PlayCb::SetCursorItem
        | PlayCb::SetPlayerInventory => {
            let mut records = Vec::new();
            let translated = transcode(t, kind.name(), Dir::S2c, |x, o| {
                let mut r = Reader::new(&body);
                match kind {
                    PlayCb::ContainerSetContent => {
                        o.put_varint(r.varint()?);
                        o.put_varint(r.varint()?);
                        let n = r.len()?;
                        o.put_len(n);
                        for _ in 0..n {
                            item::slot_stack(x, &mut r, o, &mut records)?;
                        }
                    }
                    PlayCb::ContainerSetSlot => {
                        o.put_varint(r.varint()?);
                        o.put_varint(r.varint()?);
                        o.put_bytes(r.bytes(2)?);
                    }
                    PlayCb::SetPlayerInventory => o.put_varint(r.varint()?),
                    _ => {}
                }
                item::slot_stack(x, &mut r, o, &mut records)
            })?;
            for (key, record) in records {
                t.conn.sent_stacks.record(key, record);
            }
            translated
        }
        PlayCb::SetEquipment => transcode(t, kind.name(), Dir::S2c, |x, o| {
            // VarInt entity, then { u8 slot (bit 7 = more follow), stack }.
            let mut r = Reader::new(&body);
            o.put_varint(r.varint()?);
            loop {
                let slot = r.u8()?;
                o.put_u8(slot);
                item::optional_stack(x, &mut r, o)?;
                if slot & 0x80 == 0 {
                    return Ok(());
                }
            }
        })?,
        PlayCb::MerchantOffers => transcode(t, kind.name(), Dir::S2c, |x, o| {
            merchant_offers(x, &body, o)
        })?,
        PlayCb::LevelParticles => transcode(t, kind.name(), Dir::S2c, |x, o| {
            level_particles(x, &body, o)
        })?,
        PlayCb::Explode => transcode(t, kind.name(), Dir::S2c, |x, o| explode(x, &body, o))?,
        PlayCb::Sound | PlayCb::SoundEntity => transcode(t, kind.name(), Dir::S2c, |x, o| {
            let mut r = Reader::new(&body);
            x.value(&SOUND, &mut r, o)?;
            o.put_bytes(r.rest());
            Ok(())
        })?,
        PlayCb::UpdateAttributes => transcode(t, kind.name(), Dir::S2c, |x, o| {
            // VarInt entity, list of { VarInt attribute, f64 base,
            // list of { Identifier id, f64 amount, VarInt operation } }.
            let mut r = Reader::new(&body);
            o.put_varint(r.varint()?);
            x.value(&ATTRIBUTE_SNAPSHOTS, &mut r, o)
        })?,
        PlayCb::DamageEvent => transcode(t, kind.name(), Dir::S2c, |x, o| {
            let mut r = Reader::new(&body);
            o.put_varint(r.varint()?);
            x.value(&W::Reg(R::Dynamic("minecraft:damage_type")), &mut r, o)?;
            o.put_bytes(r.rest());
            Ok(())
        })?,
        PlayCb::AwardStats => transcode(t, kind.name(), Dir::S2c, |x, o| award_stats(x, &body, o))?,
        PlayCb::PlayerChat => Some(player_chat(t, &body)?).flatten(),
        // Text, chat type, sender name, optional target name.
        PlayCb::DisguisedChat => transcode(t, kind.name(), Dir::S2c, |x, o| {
            let mut r = Reader::new(&body);
            x.value(&W::Text, &mut r, o)?;
            x.value(&CHAT_TYPE, &mut r, o)?;
            x.value(&W::Text, &mut r, o)?;
            x.value(&W::Opt(&W::Text), &mut r, o)
        })?,
        PlayCb::SetPlayerTeam => set_player_team(t, &body)?,
        PlayCb::SetTime => set_time(t, &body)?,
        PlayCb::ShowDialog => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::show_dialog(x, &body, o)
        })?,
        PlayCb::UpdateTags => Some(super::tags::update_tags(t, &body)?),
        PlayCb::Commands => commands(t, &body)?,
        PlayCb::UpdateAdvancements => transcode(t, kind.name(), Dir::S2c, |x, o| {
            crate::recipe::advancements(x, &body, o)
        })?,
        PlayCb::UpdateRecipes => transcode(t, kind.name(), Dir::S2c, |x, o| {
            crate::recipe::update_recipes(x, &body, o)
        })?,
        PlayCb::RecipeBookAdd => transcode(t, kind.name(), Dir::S2c, |x, o| {
            crate::recipe::recipe_book_add(x, &body, o)
        })?,
        PlayCb::PlaceGhostRecipe => transcode(t, kind.name(), Dir::S2c, |x, o| {
            crate::recipe::place_ghost_recipe(x, &body, o)
        })?,
        PlayCb::OpenScreen => {
            let Some(body) = transcode(t, kind.name(), Dir::S2c, |x, o| {
                text_packets::open_screen(x, &body, o)
            })?
            else {
                return Ok(());
            };
            if client == Proto::V777 {
                open_screen_777(t, &body, out);
            } else {
                t.emit_play_cb(kind, body, out);
            }
            return Ok(());
        }
        PlayCb::SystemChat
        | PlayCb::SetActionBarText
        | PlayCb::SetTitleText
        | PlayCb::SetSubtitleText
        | PlayCb::Disconnect
        | PlayCb::ServerData
        | PlayCb::TestInstanceBlockStatus => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::leading_text(x, &body, o)
        })?,
        PlayCb::TabList => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::tab_list(x, &body, o)
        })?,
        PlayCb::PlayerCombatKill => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::player_combat_kill(x, &body, o)
        })?,
        PlayCb::CommandSuggestions => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::command_suggestions(x, &body, o)
        })?,
        PlayCb::MapItemData => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::map_item_data(x, &body, o)
        })?,
        PlayCb::BossEvent => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::boss_event(x, &body, o)
        })?,
        PlayCb::SetObjective => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::set_objective(x, &body, o)
        })?,
        PlayCb::SetScore => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::set_score(x, &body, o)
        })?,
        PlayCb::PlayerInfoUpdate => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::player_info_update(x, &body, o)
        })?,
        PlayCb::ResourcePackPush => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::resource_pack_push(x, &body, o)
        })?,
        PlayCb::ServerLinks => transcode(t, kind.name(), Dir::S2c, |x, o| {
            text_packets::server_links(x, &body, o)
        })?,
        // Debug subscriptions (`debug.rs`); `debug_sample` is identical in
        // every release and takes the default arm.
        PlayCb::DebugBlockValue | PlayCb::DebugChunkValue => {
            transcode(t, kind.name(), Dir::S2c, |x, o| {
                crate::debug::value_packet(x, &body, crate::debug::Prefix::Long, true, o)
            })?
        }
        PlayCb::DebugEntityValue => transcode(t, kind.name(), Dir::S2c, |x, o| {
            crate::debug::value_packet(x, &body, crate::debug::Prefix::VarInt, true, o)
        })?,
        PlayCb::DebugEvent => transcode(t, kind.name(), Dir::S2c, |x, o| {
            crate::debug::value_packet(x, &body, crate::debug::Prefix::None, false, o)
        })?,
        _ if client == Proto::V777 => super::play777::clientbound(t, kind, body, out)?,
        _ => Some(body),
    };
    if let Some(body) = body {
        t.emit_play_cb(kind, body, out);
    }
    Ok(())
}

pub fn serverbound(
    t: &mut ViaTranslator,
    kind: PlaySb,
    body: Bytes,
    out: &mut TranslatorOutput,
) -> WireResult<()> {
    let body: Option<Bytes> = match kind {
        PlaySb::Interact if t.client == Proto::V774 => {
            interact_774(t, &body, out)?;
            return Ok(());
        }
        PlaySb::SpectateEntity => {
            // 26.1's spectate_entity (VarInt id) is 26.2's spectator_action
            // (OptVarInt: 0 none, id + 1).
            let mut r = Reader::new(&body);
            let id = r.varint()?;
            let mut o = Vec::with_capacity(5);
            o.put_varint(id.saturating_add(1));
            t.emit_play_sb(PlaySb::SpectatorAction, o.into(), out);
            return Ok(());
        }
        PlaySb::ContainerClick => {
            // Records that answered a claim, refreshed once translated.
            let mut answered = Vec::new();
            let translated = transcode(t, kind.name(), Dir::C2s, |x, o| {
                // container id, state id, i16 slot, i8 button, VarInt click
                // type, Map<i16 slot, HashedStack>, HashedStack carried.
                let mut r = Reader::new(&body);
                o.put_varint(r.varint()?);
                o.put_varint(r.varint()?);
                o.put_bytes(r.bytes(3)?);
                o.put_varint(r.varint()?);
                let n = r.len()?;
                o.put_len(n);
                for _ in 0..n {
                    o.put_bytes(r.bytes(2)?);
                    item::hashed_stack(x, &mut r, o, &mut answered)?;
                }
                item::hashed_stack(x, &mut r, o, &mut answered)
            })?;
            for key in &answered {
                t.conn.sent_stacks.touch(key);
            }
            translated
        }
        PlaySb::SetCreativeModeSlot => transcode(t, kind.name(), Dir::C2s, |x, o| {
            let mut r = Reader::new(&body);
            o.put_bytes(r.bytes(2)?);
            item::untrusted_stack(x, &mut r, o)
        })?,
        _ if t.client == Proto::V777 => super::play777::serverbound(t, kind, body, out)?,
        _ => Some(body),
    };
    if let Some(body) = body {
        t.emit_play_sb(kind, body, out);
    }
    Ok(())
}

// ---- login / respawn -------------------------------------------------------

/// Reads `CommonPlayerSpawnInfo` (776) and writes it for the client.
///
/// ```text
/// VarInt dimension type, Identifier dimension, i64 seed,
/// game mode (776: i8; 777: VarInt), previous game mode (776: i8, -1 none;
/// 777: OptVarInt), Bool debug, Bool flat, Optional<GlobalPos> death,
/// VarInt portal cooldown, VarInt sea level
/// ```
fn spawn_info(t: &mut ViaTranslator, r: &mut Reader<'_>, out: &mut Vec<u8>) -> WireResult<()> {
    let dim_type =
        u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative dimension type"))?;
    let client_dim = t
        .conn
        .dyn_s2c("minecraft:dimension_type", dim_type)
        .ok_or(WireError::Malformed("dimension type unknown to the client"))?;
    t.conn.min_y = dimension_min_y(t, dim_type);
    out.put_varint(wire_id(client_dim));
    out.put_str_bytes(r.str_bytes()?);
    out.put_bytes(r.bytes(8)?);
    let mode = r.i8()?;
    let previous = r.i8()?;
    t.conn.game_mode = u8::try_from(mode).unwrap_or(0);
    if t.client == Proto::V777 {
        out.put_varint(i32::from(mode));
        out.put_varint(if previous < 0 {
            0
        } else {
            i32::from(previous) + 1
        });
    } else {
        out.put_i8(mode);
        out.put_i8(previous);
    }
    out.put_bytes(r.bytes(2)?);
    if r.bool()? {
        out.put_bool(true);
        out.put_str_bytes(r.str_bytes()?);
        out.put_bytes(r.bytes(8)?);
    } else {
        out.put_bool(false);
    }
    out.put_varint(r.varint()?);
    out.put_varint(r.varint()?);
    Ok(())
}

/// The lowest block y of a vanilla dimension type; -64 for anything else
/// (the converted Elyzune world is an overworld).
fn dimension_min_y(t: &ViaTranslator, dim_type: u32) -> i16 {
    let name = t
        .conn
        .dynamic("minecraft:dimension_type")
        .and_then(|r| r.names.get(dim_type as usize))
        .map(String::as_str);
    match name {
        Some("minecraft:the_nether" | "minecraft:the_end") => 0,
        _ => -64,
    }
}

/// `login`: i32 entity, Bool hardcore, List<Identifier> dimensions, VarInt
/// max players, VarInt view, VarInt simulation, Bool reduced debug, Bool
/// death screen, Bool limited crafting, spawn info, (776+) Bool online mode,
/// Bool secure chat.
fn login(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let mut out = Vec::with_capacity(body.len() + 4);
    let entity = r.i32()?;
    t.conn.player_entity = entity;
    t.conn.entities.clear();
    // The player's own entity never gets an add_entity; its entity data is
    // remapped with the player layout.
    if let Some(player) = id_of(Reg::EntityType, 776, "minecraft:player") {
        t.conn.track_entity(entity, player);
    }
    out.put_i32(entity);
    out.put_u8(r.u8()?);
    let n = r.len()?;
    out.put_len(n);
    for _ in 0..n {
        out.put_str_bytes(r.str_bytes()?);
    }
    for _ in 0..3 {
        out.put_varint(r.varint()?);
    }
    out.put_bytes(r.bytes(3)?);
    spawn_info(t, &mut r, &mut out)?;
    let online_mode = r.u8()?;
    if t.client == Proto::V777 {
        out.put_u8(online_mode);
    }
    out.put_u8(r.u8()?);
    Ok(out.into())
}

/// `respawn`: spawn info, i8 data kept.
fn respawn(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let mut out = Vec::with_capacity(body.len() + 2);
    spawn_info(t, &mut r, &mut out)?;
    out.put_bytes(r.rest());
    // A respawn (dimension change included) recreates every entity but the
    // player's own.
    let own = t.conn.entities.get(&t.conn.player_entity).copied();
    t.conn.entities.clear();
    if let Some(player) = own {
        t.conn.track_entity(t.conn.player_entity, player);
    }
    Ok(out.into())
}

/// `game_event`: u8 event, f32 value. Event 3 changes the game mode, which
/// decides how a 1.21.11 attack is translated.
fn track_game_event(t: &mut ViaTranslator, body: &[u8]) {
    let mut r = Reader::new(body);
    if let (Ok(3), Ok(value)) = (r.u8(), r.f32()) {
        t.conn.game_mode = (value + 0.5).floor().clamp(0.0, 3.0) as u8;
    }
}

// ---- world -----------------------------------------------------------------

/// `level_event`: i32 event, i64 position, i32 data, Bool global. Events
/// 2001 (block destroyed) and 3008 (brushing finished) carry a block state.
fn level_event(t: &ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let event = r.i32()?;
    let pos = r.i64()?;
    let data = r.i32()?;
    let global = r.u8()?;
    let data = if matches!(event, 2001 | 3008) {
        let state = u32::try_from(data).unwrap_or(0);
        wire_id(t.tables.block_states.s2c(state).unwrap_or(0))
    } else {
        data
    };
    let mut out = Vec::with_capacity(body.len());
    out.put_i32(event);
    out.put_i64(pos);
    out.put_i32(data);
    out.put_u8(global);
    Ok(out.into())
}

/// `level_particles` (776): Bool override limiter, Bool always show, f64
/// x/y/z, f32 offset x/y/z, f32 speed, i32 count, particle. 26.3 moves the
/// particle first, splits the speed per axis, sends the count as a VarInt
/// and appends a randomisation mode (0 = default).
fn level_particles(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Result<(), Xerr> {
    let mut r = Reader::new(body);
    let flags = r.bytes(2)?;
    let position = r.bytes(24)?;
    let offset = r.bytes(12)?;
    let speed = r.bytes(4)?;
    let count = r.i32()?;
    let mut particle = Vec::new();
    crate::particle::particle(x, &mut r, &mut particle)?;
    if x.t.client == Proto::V777 {
        out.put_bytes(&particle);
        out.put_bytes(flags);
        out.put_bytes(position);
        out.put_bytes(offset);
        for _ in 0..3 {
            out.put_bytes(speed);
        }
        out.put_varint(count);
        out.put_varint(0);
    } else {
        out.put_bytes(flags);
        out.put_bytes(position);
        out.put_bytes(offset);
        out.put_bytes(speed);
        out.put_i32(count);
        out.put_bytes(&particle);
    }
    Ok(())
}

/// `explode`: Vec3 center, f32 radius, i32 block count, Optional<Vec3>
/// knockback, particle, sound holder, List<{particle, f32 scaling, f32 speed,
/// VarInt weight}>; 26.3 appends Bool play sound.
fn explode(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Result<(), Xerr> {
    let mut r = Reader::new(body);
    out.put_bytes(r.bytes(32)?);
    if r.bool()? {
        out.put_bool(true);
        out.put_bytes(r.bytes(24)?);
    } else {
        out.put_bool(false);
    }
    crate::particle::particle(x, &mut r, out)?;
    x.value(&SOUND, &mut r, out)?;
    x.value(&BLOCK_PARTICLES, &mut r, out)?;
    if x.t.client == Proto::V777 {
        out.put_bool(true);
    }
    Ok(())
}

/// `award_stats`: Map<{VarInt stat type, VarInt value}, VarInt count>. The
/// value is an id in the registry the stat type names.
fn award_stats(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Result<(), Xerr> {
    let mut r = Reader::new(body);
    let n = r.len()?;
    let mut entries = Vec::new();
    let mut kept = 0usize;
    for _ in 0..n {
        let stat_type = u32::try_from(r.varint()?)
            .map_err(|_| Xerr::Wire(WireError::Malformed("negative stat type")))?;
        let value = u32::try_from(r.varint()?)
            .map_err(|_| Xerr::Wire(WireError::Malformed("negative stat value")))?;
        let count = r.varint()?;
        let reg = match name_of(Reg::StatType, 776, stat_type) {
            Some("minecraft:mined") => Some(Reg::Block),
            Some("minecraft:killed" | "minecraft:killed_by") => Some(Reg::EntityType),
            Some("minecraft:custom") => Some(Reg::CustomStat),
            Some(_) => Some(Reg::Item),
            None => None,
        };
        let mapped_type = x.map_static(Reg::StatType, stat_type);
        let mapped_value = reg.and_then(|reg| x.map_static(reg, value));
        if let (Some(ty), Some(value)) = (mapped_type, mapped_value) {
            entries.put_varint(wire_id(ty));
            entries.put_varint(wire_id(value));
            entries.put_varint(count);
            kept += 1;
        }
    }
    out.put_len(kept);
    out.put_bytes(&entries);
    Ok(())
}

/// `merchant_offers`: container id, List<offer>, VarInt level, VarInt xp,
/// Bool progress, Bool restock. An offer: ItemCost a, ItemStack result,
/// Optional<ItemCost> b, Bool out of stock, i32 uses, i32 max uses, i32 xp,
/// i32 special price, f32 multiplier, i32 demand. ItemCost: VarInt item,
/// VarInt count, List<TypedDataComponent>.
fn merchant_offers(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Result<(), Xerr> {
    let mut r = Reader::new(body);
    out.put_varint(r.varint()?);
    let n = r.len()?;
    let mut offers = Vec::new();
    let mut kept = 0usize;
    for _ in 0..n {
        let mut offer = Vec::new();
        let result = (|| -> Result<(), Xerr> {
            x.value(&ITEM_COST, &mut r, &mut offer)?;
            item::stack(x, &mut r, &mut offer, false)?;
            x.value(&OPTIONAL_ITEM_COST, &mut r, &mut offer)?;
            offer.put_bytes(r.bytes(1 + 4 * 4 + 4 + 4)?);
            Ok(())
        })();
        match result {
            Ok(()) => {
                offers.put_bytes(&offer);
                kept += 1;
            }
            // A trade whose items cannot be shown is left out; the reader
            // is past it only if the error came at the very end, so any
            // other position is a stream error.
            Err(e) => return Err(e),
        }
    }
    out.put_len(kept);
    out.put_bytes(&offers);
    out.put_bytes(r.rest());
    Ok(())
}

// ---- chat and teams --------------------------------------------------------

/// `player_chat`: VarInt global index, UUID sender, VarInt index,
/// Optional<256 bytes> signature, body { String message, i64 time, i64 salt,
/// List<last seen> }, Optional<NBT> unsigned, filter mask, chat type.
/// The chat type holder is remapped; 26.3 encodes a partial filter mask as
/// a byte array instead of a long array.
fn player_chat(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Option<Bytes>> {
    transcode(t, "minecraft:player_chat", Dir::S2c, |x, o| {
        let mut r = Reader::new(body);
        o.put_varint(r.varint()?);
        o.put_bytes(r.bytes(16)?);
        o.put_varint(r.varint()?);
        if r.bool()? {
            o.put_bool(true);
            o.put_bytes(r.bytes(256)?);
        } else {
            o.put_bool(false);
        }
        o.put_str_bytes(r.str_bytes()?);
        o.put_bytes(r.bytes(16)?);
        let seen = r.len()?;
        o.put_len(seen);
        for _ in 0..seen {
            let id = r.varint()?;
            o.put_varint(id);
            if id == 0 {
                o.put_bytes(r.bytes(256)?);
            }
        }
        x.value(&W::Opt(&W::Text), &mut r, o)?;
        let filter = r.varint()?;
        o.put_varint(filter);
        if filter == 2 {
            let longs = r.len()?;
            let raw = r.bytes(longs.checked_mul(8).ok_or(WireError::BadLength)?)?;
            if x.t.client == Proto::V777 {
                chunk::bitset_bytes(raw, o);
            } else {
                o.put_len(longs);
                o.put_bytes(raw);
            }
        }
        x.value(&CHAT_TYPE, &mut r, o)?;
        x.value(&W::Text, &mut r, o)?;
        x.value(&W::Opt(&W::Text), &mut r, o)
    })
}

/// `set_player_team`: String name, i8 method (0 create, 1 remove, 2 update,
/// 3 add players, 4 remove players), parameters for 0 and 2, players for 0,
/// 3 and 4.
///
/// Parameters, 26.2+: NBT name, NBT prefix, NBT suffix, VarInt visibility,
/// VarInt collision, Optional<VarInt color>, i8 options.
/// Up to 26.1: NBT name, i8 options, VarInt visibility, VarInt collision,
/// VarInt color (`ChatFormatting`: 0-15 colours, 21 reset), NBT prefix,
/// NBT suffix.
fn set_player_team(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Option<Bytes>> {
    transcode(t, "minecraft:set_player_team", Dir::S2c, |x, o| {
        let mut r = Reader::new(body);
        o.put_str_bytes(r.str_bytes()?);
        let method = r.i8()?;
        o.put_i8(method);
        if method == 0 || method == 2 {
            let mut name = Vec::new();
            let mut prefix = Vec::new();
            let mut suffix = Vec::new();
            x.value(&W::Text, &mut r, &mut name)?;
            x.value(&W::Text, &mut r, &mut prefix)?;
            x.value(&W::Text, &mut r, &mut suffix)?;
            if x.t.client == Proto::V777 {
                o.put_bytes(&name);
                o.put_bytes(&prefix);
                o.put_bytes(&suffix);
            } else {
                let visibility = r.varint()?;
                let collision = r.varint()?;
                let color = if r.bool()? { r.varint()? } else { 21 };
                let options = r.u8()?;
                o.put_bytes(&name);
                o.put_u8(options);
                o.put_varint(visibility);
                o.put_varint(collision);
                o.put_varint(color);
                o.put_bytes(&prefix);
                o.put_bytes(&suffix);
            }
        }
        o.put_bytes(r.rest());
        Ok(())
    })
}

/// `set_time` (26.1+): i64 game time, Map<VarInt clock, { VarLong total
/// ticks, f32 partial tick, f32 rate }>. 1.21.11: i64 game time, i64 day
/// time, Bool advancing. The overworld clock is the day time; when an update
/// does not carry it, the last known day time is advanced by the elapsed
/// game time, as ViaBackwards does.
fn set_time(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Option<Bytes>> {
    let mut r = Reader::new(body);
    let game_time = r.i64()?;
    let n = r.len()?;
    let mut clocks = Vec::new();
    let mut overworld: Option<(i64, bool)> = None;
    let mut kept = 0usize;
    for _ in 0..n {
        let clock =
            u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative clock"))?;
        let ticks = r.varlong()?;
        let partial = r.bytes(4)?;
        let rate = r.f32()?;
        let name = t
            .conn
            .dynamic("minecraft:world_clock")
            .and_then(|reg| reg.names.get(clock as usize))
            .map(String::as_str);
        if name == Some("minecraft:overworld") {
            overworld = Some((ticks, rate != 0.0));
        }
        if let Some(mapped) = t.conn.dyn_s2c("minecraft:world_clock", clock) {
            clocks.put_varint(wire_id(mapped));
            clocks.put_varlong(ticks);
            clocks.put_bytes(partial);
            clocks.put_f32(rate);
            kept += 1;
        }
    }
    if t.client != Proto::V774 {
        let mut out = Vec::with_capacity(body.len());
        out.put_i64(game_time);
        out.put_len(kept);
        out.put_bytes(&clocks);
        return Ok(Some(out.into()));
    }
    let clock = &mut t.conn.day_time;
    match overworld {
        Some((ticks, advancing)) => {
            clock.day_time = ticks;
            clock.advancing = advancing;
        }
        None if clock.advancing => clock.day_time += game_time - clock.game_time,
        None => {}
    }
    clock.game_time = game_time;
    let mut out = Vec::with_capacity(17);
    out.put_i64(game_time);
    out.put_i64(clock.day_time);
    out.put_bool(clock.advancing);
    Ok(Some(out.into()))
}

// ---- commands ----------------------------------------------------------------

/// `commands`: the command graph. Argument parser ids changed in 26.3 (five
/// parsers inserted), so every node is walked; a node whose parser the client
/// lacks keeps its place in the graph as a greedy string, which every client
/// can parse and complete.
fn commands(t: &mut ViaTranslator, body: &Bytes) -> WireResult<Option<Bytes>> {
    if !matches!(
        t.tables.reg(Reg::CommandArgumentType),
        crate::tables::RegMap::Remap { .. }
    ) {
        return Ok(Some(body.clone()));
    }
    let mut out = Vec::with_capacity(body.len() + 16);
    crate::commands::rewrite(t, body, &mut out)?;
    Ok(Some(out.into()))
}

// ---- interact (1.21.11) ------------------------------------------------------

/// 1.21.11 `interact`: VarInt entity, VarInt action (0 interact, 1 attack,
/// 2 interact at), [f32 x/y/z if 2], [VarInt hand if 0 or 2], Bool sneaking.
///
/// 26.1 split it: `attack` (VarInt entity), `interact` (VarInt entity,
/// VarInt hand, LpVec3 location, Bool sneaking) and, for spectators,
/// `spectator_action` (26.2: OptVarInt entity). A 1.21.11 client sends
/// "interact at" and then "interact" for the same click; only the first has
/// the location 26.1 requires, so the second is dropped (ViaBackwards does
/// the same).
fn interact_774(t: &mut ViaTranslator, body: &[u8], out: &mut TranslatorOutput) -> WireResult<()> {
    let mut r = Reader::new(body);
    let entity = r.varint()?;
    let action = r.varint()?;
    match action {
        0 => {}
        1 => {
            let mut o = Vec::with_capacity(5);
            if t.conn.game_mode == 3 {
                o.put_varint(entity.saturating_add(1));
                t.emit_play_sb(PlaySb::SpectatorAction, o.into(), out);
            } else {
                o.put_varint(entity);
                t.emit_play_sb(PlaySb::Attack, o.into(), out);
            }
        }
        2 => {
            let x = f64::from(r.f32()?);
            let y = f64::from(r.f32()?);
            let z = f64::from(r.f32()?);
            let hand = r.varint()?;
            let sneaking = r.bool()?;
            let mut o = Vec::with_capacity(16);
            o.put_varint(entity);
            o.put_varint(hand);
            entity::write_lp_vec3(&mut o, x, y, z);
            o.put_bool(sneaking);
            t.emit_play_sb(PlaySb::Interact, o.into(), out);
        }
        _ => return Err(WireError::Malformed("unknown interact action")),
    }
    Ok(())
}

// ---- 26.3 brewing stand ------------------------------------------------------

/// `open_screen`: 26.3's brewing stand has two more data slots (total brew
/// time and total fuel), which a 26.2 server never sends; they are added
/// right after the screen opens with vanilla's constants.
fn open_screen_777(t: &mut ViaTranslator, body: &Bytes, out: &mut TranslatorOutput) {
    t.emit_play_cb(PlayCb::OpenScreen, body.clone(), out);
    let mut r = Reader::new(body);
    let (Ok(container), Ok(menu)) = (r.varint(), r.varint()) else {
        return;
    };
    let brewing = id_of(Reg::Menu, 776, "minecraft:brewing_stand");
    if u32::try_from(menu).ok() != brewing {
        return;
    }
    for (slot, value) in [(2i16, 400i16), (3, 20)] {
        let mut o = Vec::with_capacity(8);
        o.put_varint(container);
        o.put_i16(slot);
        o.put_i16(value);
        t.emit_play_cb(PlayCb::ContainerSetData, o.into(), out);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Inventory PLAT-V14: 26.2's `minecraft:friction_modifier` does not exist
    /// before 26.2. The 1.21.11 client gets the other attribute of the packet,
    /// and the left-out one is declared once for the connection instead of
    /// vanishing silently.
    #[test]
    fn an_attribute_the_client_lacks_is_left_out_and_declared() {
        let mut t = ViaTranslator::new(Proto::V774, 1);
        let health = id_of(Reg::Attribute, 776, "minecraft:max_health").unwrap();
        let friction = id_of(Reg::Attribute, 776, "minecraft:friction_modifier").unwrap();
        assert_eq!(
            id_of(Reg::Attribute, 774, "minecraft:friction_modifier"),
            None
        );
        let mut body = Vec::new();
        body.put_varint(7);
        body.put_len(2);
        for (attribute, base) in [(health, 20.0_f64), (friction, 1.0)] {
            body.put_varint(i32::try_from(attribute).unwrap());
            body.put_bytes(&base.to_be_bytes());
            body.put_len(0);
        }
        let mut out = TranslatorOutput::new();
        clientbound(&mut t, PlayCb::UpdateAttributes, body.into(), &mut out).unwrap();
        let sent: Vec<_> = out.take_to_client().collect();
        assert_eq!(sent.len(), 1);
        let mut r = Reader::new(&sent[0].data);
        assert_eq!(r.varint().unwrap(), 7);
        assert_eq!(r.len().unwrap(), 1, "only max_health reaches 1.21.11");
        let client_health = id_of(Reg::Attribute, 774, "minecraft:max_health").unwrap();
        assert_eq!(r.varint().unwrap(), i32::try_from(client_health).unwrap());
        assert!(t.reported(
            "minecraft:update_attributes",
            "a list element with no equivalent on the other side was removed"
        ));
    }
}
