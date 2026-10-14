//! Golden translations: hand-built 26.2 (776) packets and the exact bytes
//! each client protocol must receive, and client packets with the exact
//! bytes the 776 backend must receive.
//!
//! Expected registry ids are written out as numbers, taken from the vanilla
//! data generator's `registries.json` / `blocks.json` of each release (cited
//! next to each constant), not computed with the translator's own tables, so
//! a wrong table fails here. Packet ids are looked up by name in the
//! generated id tables, which the mcprotocol.c scenario cross-checks against
//! mcprotocol.c's independent catalogs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use elyzune_via::generated::Proto;
use elyzune_via::generated::dynamic::SYNCED_776;
use elyzune_via::generated::packets::{ConfigCb, ConfigSb, LoginCb, LoginSb, PlayCb, PlaySb};
use elyzune_via::session::{ViaTranslator, WireState};
use elyzune_via::wire::Put;
use infrarust_api::filter::{ProtocolTranslator, TranslatorOutput};
use infrarust_api::types::RawPacket;

const CLIENTS: [i32; 3] = [774, 775, 777];

fn proto(n: i32) -> Proto {
    Proto::from_number(n).unwrap()
}

/// A translator driven through handshake, login and configuration into play,
/// with every 26.2 synchronised registry sent as vanilla names.
fn play(client: i32) -> ViaTranslator {
    let p = proto(client);
    let mut t = ViaTranslator::new(p, 1);
    let mut out = TranslatorOutput::new();
    let mut hs = Vec::new();
    hs.put_varint(client);
    hs.put_str("localhost");
    hs.put_bytes(&25565u16.to_be_bytes());
    hs.put_varint(2);
    t.serverbound(RawPacket::new(0, hs.into()), &mut out);
    let sent: Vec<RawPacket> = out.take_to_server().collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        &sent[0].data[..2],
        &[0x88, 0x06],
        "handshake rewritten to 776"
    );

    let mut finished = Vec::new();
    finished.put_bytes(&[7; 16]);
    finished.put_str("Bot");
    finished.put_varint(0);
    finished.put_bytes(&[9; 16]);
    t.clientbound(
        RawPacket::new(
            LoginCb::LoginFinished.id(Proto::V776).unwrap(),
            finished.into(),
        ),
        &mut out,
    );
    t.serverbound(
        RawPacket::new(LoginSb::LoginAcknowledged.id(p).unwrap(), Bytes::new()),
        &mut out,
    );
    for (name, entries) in SYNCED_776 {
        let mut body = Vec::new();
        body.put_str(name);
        body.put_len(entries.len());
        for e in *entries {
            body.put_str(e);
            body.put_bool(false);
        }
        t.clientbound(
            RawPacket::new(ConfigCb::RegistryData.id(Proto::V776).unwrap(), body.into()),
            &mut out,
        );
    }
    t.clientbound(
        RawPacket::new(
            ConfigCb::FinishConfiguration.id(Proto::V776).unwrap(),
            Bytes::new(),
        ),
        &mut out,
    );
    t.serverbound(
        RawPacket::new(ConfigSb::FinishConfiguration.id(p).unwrap(), Bytes::new()),
        &mut out,
    );
    assert_eq!(t.states(), (WireState::Play, WireState::Play));
    t
}

/// Translates a clientbound 776 packet; returns the client packets as
/// `(name, body)`.
fn cb(t: &mut ViaTranslator, client: i32, kind: PlayCb, body: Vec<u8>) -> Vec<(PlayCb, Vec<u8>)> {
    let mut out = TranslatorOutput::new();
    t.clientbound(
        RawPacket::new(kind.id(Proto::V776).unwrap(), body.into()),
        &mut out,
    );
    assert!(out.close_requested().is_none());
    out.take_to_client()
        .map(|p| {
            (
                PlayCb::from_id(proto(client), p.packet_id).unwrap(),
                p.data.to_vec(),
            )
        })
        .collect()
}

/// Translates a serverbound client packet; returns the 776 packets.
fn sb(t: &mut ViaTranslator, client: i32, kind: PlaySb, body: Vec<u8>) -> Vec<(PlaySb, Vec<u8>)> {
    let mut out = TranslatorOutput::new();
    t.serverbound(
        RawPacket::new(kind.id(proto(client)).unwrap(), body.into()),
        &mut out,
    );
    out.take_to_server()
        .map(|p| {
            (
                PlaySb::from_id(Proto::V776, p.packet_id).unwrap(),
                p.data.to_vec(),
            )
        })
        .collect()
}

fn one(packets: Vec<(PlayCb, Vec<u8>)>, kind: PlayCb) -> Vec<u8> {
    assert_eq!(packets.len(), 1, "{packets:?}");
    assert_eq!(packets[0].0, kind);
    packets.into_iter().next().unwrap().1
}

fn nbt_string(s: &str) -> Vec<u8> {
    let mut v = vec![8u8];
    v.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
    v.extend_from_slice(s.as_bytes());
    v
}

// ---- login -------------------------------------------------------------------

#[test]
fn login_finished_drops_the_session_id_for_1_21_11_and_26_1() {
    for client in CLIENTS {
        let p = proto(client);
        let mut t = ViaTranslator::new(p, 1);
        let mut out = TranslatorOutput::new();
        let mut hs = Vec::new();
        hs.put_varint(client);
        hs.put_str("h");
        hs.put_bytes(&[0, 1]);
        hs.put_varint(2);
        t.serverbound(RawPacket::new(0, hs.into()), &mut out);
        let mut profile = Vec::new();
        profile.put_bytes(&[1; 16]);
        profile.put_str("Bot");
        profile.put_varint(1);
        profile.put_str("textures");
        profile.put_str("e30=");
        profile.put_bool(false);
        let mut body = profile.clone();
        body.put_bytes(&[2; 16]);
        t.clientbound(RawPacket::new(2, body.clone().into()), &mut out);
        let got: Vec<RawPacket> = out.take_to_client().collect();
        let expected = if client == 777 { body } else { profile };
        assert_eq!(got[0].data.as_ref(), expected.as_slice(), "{client}");
        assert_eq!(t.states().0, WireState::Config);
    }
}

// ---- join game and respawn ---------------------------------------------------

/// A join-game body. `varint_modes` is 26.3's encoding of the game modes,
/// `online` the 26.2+ online-mode flag (absent before 26.2).
fn join_body(varint_modes: bool, online: Option<bool>) -> Vec<u8> {
    let mut body = Vec::new();
    body.put_i32(7);
    body.put_bool(false);
    body.put_varint(1);
    body.put_str("minecraft:overworld");
    body.put_varint(20);
    body.put_varint(10);
    body.put_varint(8);
    body.put_bytes(&[0, 1, 0]);
    // spawn info: dimension type 0 (overworld, first in sorted order)
    body.put_varint(0);
    body.put_str("minecraft:overworld");
    body.put_i64(42);
    if varint_modes {
        body.put_varint(1); // creative
        body.put_varint(0); // OptVarInt: no previous mode
    } else {
        body.put_i8(1);
        body.put_i8(-1);
    }
    body.put_bytes(&[0, 1]); // debug, flat
    body.put_bool(false); // no death location
    body.put_varint(0);
    body.put_varint(63);
    if let Some(online) = online {
        body.put_bool(online);
    }
    body.put_bool(false); // secure chat
    body
}

#[test]
fn join_game_per_client() {
    let server = join_body(false, Some(true));
    for (client, expected) in [
        (774, join_body(false, None)),
        (775, join_body(false, None)),
        (777, join_body(true, Some(true))),
    ] {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::Login, server.clone()),
            PlayCb::Login,
        );
        assert_eq!(got, expected, "{client}");
    }
}

// ---- entities ----------------------------------------------------------------

fn add_entity(id: i32, ty: i32, data: i32) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_varint(id);
    b.put_bytes(&[3; 16]);
    b.put_varint(ty);
    b.put_f64(1.0);
    b.put_f64(64.0);
    b.put_f64(-2.0);
    b.put_u8(0); // zero motion (LpVec3)
    b.put_bytes(&[0, 64, 64]);
    b.put_varint(data);
    b
}

#[test]
fn entity_types_and_falling_block_states() {
    // registries.json entity_type: player 155/155/156/159, falling_block 51/51/51/52.
    // blocks.json red_sandstone: 13045/13247/13247/15094; cinnabar 25926 in
    // 26.2 falls back to red_sandstone in 1.21.11 and 26.1, is 29283 in 26.3.
    let player = [(774, 155), (775, 155), (777, 159)];
    let falling = [(774, 51, 13045), (775, 51, 13247), (777, 52, 29283)];
    for ((client, player_id), (_, falling_id, state)) in player.into_iter().zip(falling) {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::AddEntity, add_entity(9, 156, 0)),
            PlayCb::AddEntity,
        );
        assert_eq!(got, add_entity(9, player_id, 0), "{client}");
        let got = one(
            cb(&mut t, client, PlayCb::AddEntity, add_entity(10, 51, 25926)),
            PlayCb::AddEntity,
        );
        assert_eq!(got, add_entity(10, falling_id, state), "{client}");
    }
}

#[test]
fn slime_entity_data_indices() {
    // 26.2 made slimes ageable: index 16 baby (BOOLEAN), 17 age locked,
    // 18 size (INT). 1.21.11 and 26.1 have the size at 16 and no baby flag.
    // Serializer ids BYTE 0, INT 1, BOOLEAN 8 are the same in every release.
    let mut body = Vec::new();
    body.put_varint(11);
    body.put_bytes(&[0, 0, 0x20]); // index 0 BYTE flags
    body.put_bytes(&[16, 8, 1]); // index 16 BOOLEAN baby
    body.put_u8(18);
    body.put_varint(1);
    body.put_varint(3); // index 18 INT size
    body.put_u8(0xFF);
    for client in CLIENTS {
        let mut t = play(client);
        cb(&mut t, client, PlayCb::AddEntity, add_entity(11, 117, 0));
        let got = one(
            cb(&mut t, client, PlayCb::SetEntityData, body.clone()),
            PlayCb::SetEntityData,
        );
        let expected = if client == 777 {
            body.clone()
        } else {
            let mut e = Vec::new();
            e.put_varint(11);
            e.put_bytes(&[0, 0, 0x20]);
            e.put_bytes(&[16, 1, 3]);
            e.put_u8(0xFF);
            e
        };
        assert_eq!(got, expected, "{client}");
    }
}

// ---- blocks ------------------------------------------------------------------

#[test]
fn block_updates_and_bed_block_entities() {
    let pos = 0x0000_0040_0010_0005i64;
    let update = |state: i32| {
        let mut b = Vec::new();
        b.put_i64(pos);
        b.put_varint(state);
        b
    };
    // blocks.json white_bed facing=north occupied=false part=head:
    // 1733 (1.21.11), 1933 (26.1, 26.2), 2032 (26.3). block_entity_type bed: 25.
    for (client, bed_state) in [(774, 1733), (775, 1933), (777, 2032)] {
        let mut t = play(client);
        let got = cb(&mut t, client, PlayCb::BlockUpdate, update(1933));
        assert_eq!(got[0], (PlayCb::BlockUpdate, update(bed_state)), "{client}");
        if client == 777 {
            assert_eq!(got.len(), 1);
        } else {
            let mut be = Vec::new();
            be.put_i64(pos);
            be.put_varint(25);
            be.put_bytes(&[10, 0]);
            assert_eq!(got[1], (PlayCb::BlockEntityData, be), "{client}");
        }
    }
}

/// A one-section chunk at (2, -3): stone everywhere except a bed head at
/// (x 1, y 2, z 3) given by an indirect 4-bit palette; plains biome (id 0).
fn chunk_776() -> (Vec<u8>, Vec<u8>, usize) {
    let mut data = Vec::new();
    data.put_i16(4095);
    data.put_i16(0); // fluid count (26.1+)
    data.put_u8(4);
    data.put_varint(2);
    data.put_varint(1); // stone
    data.put_varint(1933); // bed
    let bed_index = 2 * 256 + 3 * 16 + 1;
    let mut longs = vec![0u64; 256];
    longs[bed_index / 16] |= 1u64 << ((bed_index % 16) * 4);
    for l in &longs {
        data.put_i64(*l as i64);
    }
    data.put_u8(0);
    data.put_varint(0); // biome
    let mut body = Vec::new();
    body.put_i32(2);
    body.put_i32(-3);
    body.put_varint(0); // no heightmaps
    body.put_len(data.len());
    body.put_bytes(&data);
    body.put_varint(0); // no block entities
    let mut light = Vec::new();
    for mask in [0b101u64, 0, 0, 0] {
        if mask == 0 {
            light.put_varint(0);
        } else {
            light.put_varint(1);
            light.put_i64(mask as i64);
        }
    }
    light.put_varint(0);
    light.put_varint(0);
    (body, light, bed_index)
}

#[test]
fn chunk_sections_light_and_beds() {
    let (body, light, bed_index) = chunk_776();
    let mut packet = body.clone();
    packet.extend_from_slice(&light);
    for (client, stone, bed) in [(774, 1, 1733), (775, 1, 1933), (777, 1, 2032)] {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::LevelChunkWithLight, packet.clone()),
            PlayCb::LevelChunkWithLight,
        );

        let mut data = Vec::new();
        data.put_i16(4095);
        if client != 774 {
            data.put_i16(0);
        }
        data.put_u8(4);
        data.put_varint(2);
        data.put_varint(stone);
        data.put_varint(bed);
        let mut longs = vec![0u64; 256];
        longs[bed_index / 16] |= 1u64 << ((bed_index % 16) * 4);
        for l in &longs {
            data.put_i64(*l as i64);
        }
        data.put_u8(0);
        data.put_varint(0);
        let mut expected = Vec::new();
        expected.put_i32(2);
        expected.put_i32(-3);
        expected.put_varint(0);
        expected.put_len(data.len());
        expected.put_bytes(&data);
        if client == 777 {
            expected.put_varint(0);
            // 26.3 masks are BitSet byte arrays: 0b101 -> [0x05].
            expected.put_varint(1);
            expected.put_u8(5);
            expected.put_bytes(&[0, 0, 0]);
        } else {
            // The bed block entity: packed xz (x 1, z 3), y = -64 + 2.
            expected.put_varint(1);
            expected.put_u8((1 << 4) | 3);
            expected.put_i16(-62);
            expected.put_varint(25);
            expected.put_bytes(&[10, 0]);
            expected.extend_from_slice(&light);
        }
        if client == 777 {
            expected.put_varint(0);
            expected.put_varint(0);
        }
        assert_eq!(got, expected, "{client}");
    }
}

#[test]
fn direct_palettes_are_repacked_for_26_3() {
    // A direct section (15 bits) holding red_sandstone (13247) and cinnabar
    // (25926): 26.3 needs 16 bits (35723 states) and ids 15094 / 29283.
    let entries: Vec<u32> = (0..4096)
        .map(|i| if i % 2 == 0 { 13247 } else { 25926 })
        .collect();
    let pack = |values: &[u32], bits: u32| -> Vec<u8> {
        let per = 64 / bits as usize;
        let mut out = Vec::new();
        for chunk in values.chunks(per) {
            let mut w = 0u64;
            for (j, v) in chunk.iter().enumerate() {
                w |= u64::from(*v) << (j * bits as usize);
            }
            out.put_i64(w as i64);
        }
        out
    };
    let mut data = Vec::new();
    data.put_i16(4096);
    data.put_i16(0);
    data.put_u8(15);
    data.put_bytes(&pack(&entries, 15));
    data.put_u8(0);
    data.put_varint(0);
    let mut body = Vec::new();
    body.put_i32(0);
    body.put_i32(0);
    body.put_varint(0);
    body.put_len(data.len());
    body.put_bytes(&data);
    body.put_varint(0);
    for _ in 0..6 {
        body.put_varint(0);
    }
    let mut t = play(777);
    let got = one(
        cb(&mut t, 777, PlayCb::LevelChunkWithLight, body),
        PlayCb::LevelChunkWithLight,
    );
    let mapped: Vec<u32> = entries
        .iter()
        .map(|&s| if s == 13247 { 15094 } else { 29283 })
        .collect();
    let mut expected_data = Vec::new();
    expected_data.put_i16(4096);
    expected_data.put_i16(0);
    expected_data.put_u8(16);
    expected_data.put_bytes(&pack(&mapped, 16));
    expected_data.put_u8(0);
    expected_data.put_varint(0);
    let mut expected = Vec::new();
    expected.put_i32(0);
    expected.put_i32(0);
    expected.put_varint(0);
    expected.put_len(expected_data.len());
    expected.put_bytes(&expected_data);
    expected.put_varint(0);
    for _ in 0..6 {
        expected.put_varint(0);
    }
    assert_eq!(got, expected);
}

// ---- items -------------------------------------------------------------------

/// A head's resolved owner carries Mojang's signed texture value. The
/// profile bytes must survive remapping of the item and component ids.
fn head_slot(item: i32, component: i32, resolved: bool, signed: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_varint(1);
    b.put_varint(3);
    b.put_i16(10);
    b.put_varint(1);
    b.put_varint(item);
    b.put_varint(1);
    b.put_varint(0);
    b.put_varint(component);
    b.put_bool(resolved);
    if resolved {
        b.put_bytes(&[7; 16]);
        b.put_str("SkinnedPlayer");
    } else {
        b.put_bool(false); // No name: Paper's offline head resolves by id.
        b.put_bool(true);
        b.put_bytes(&[7; 16]);
    }
    b.put_varint(i32::from(resolved));
    if resolved {
        b.put_str("textures");
        b.put_str(
            "eyJ0ZXh0dXJlcyI6eyJTS0lOIjp7InVybCI6Imh0dHBzOi8vdGV4dHVyZXMuaW52YWxpZC9za2luIn19fQ==",
        );
        b.put_bool(signed);
        if signed {
            b.put_str("c2lnbmF0dXJl");
        }
    }
    b.put_bytes(&[0; 4]); // No body/cape/elytra/model overrides.
    b
}

#[test]
fn player_head_profiles_keep_the_signed_skin_and_offline_owner() {
    // Mojang's registries.json, 1.21.11/26.1.2/26.2/26.3:
    // player_head 1235/1236/1265/1386; profile 68/70/70/72.
    for (client, item, component) in [(774, 1235, 68), (775, 1236, 70), (777, 1386, 72)] {
        for (resolved, signed) in [(true, true), (true, false), (false, false)] {
            let mut t = play(client);
            let got = one(
                cb(
                    &mut t,
                    client,
                    PlayCb::ContainerSetSlot,
                    head_slot(1265, 70, resolved, signed),
                ),
                PlayCb::ContainerSetSlot,
            );
            assert_eq!(
                got,
                head_slot(item, component, resolved, signed),
                "{client}"
            );
        }
    }
}

// ---- dialogs -----------------------------------------------------------------

#[test]
fn inline_dialog_and_clear_reach_every_client() {
    use elyzune_via::nbt::Tag;

    let dialog = Tag::Compound(vec![
        (b"type".to_vec(), Tag::string("minecraft:multi_action")),
        (b"title".to_vec(), Tag::string("Invite a player")),
        (
            b"actions".to_vec(),
            Tag::List(
                10,
                vec![Tag::Compound(vec![
                    (b"label".to_vec(), Tag::string("Search")),
                    (
                        b"action".to_vec(),
                        Tag::Compound(vec![
                            (b"type".to_vec(), Tag::string("minecraft:dynamic/custom")),
                            (b"id".to_vec(), Tag::string("paper:dialog_click_callback")),
                            (
                                b"additions".to_vec(),
                                Tag::Compound(vec![(
                                    b"id".to_vec(),
                                    Tag::string("93c1729c-b2e5-48dd-a1c8-d01c22a9f487"),
                                )]),
                            ),
                        ]),
                    ),
                ])],
            ),
        ),
    ]);
    let mut body = vec![0]; // Inline Holder<Dialog>.
    dialog.write(&mut body);
    for client in CLIENTS {
        let mut t = play(client);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::ShowDialog, body.clone()),
                PlayCb::ShowDialog
            ),
            body,
            "{client}",
        );
        assert!(
            one(
                cb(&mut t, client, PlayCb::ClearDialog, Vec::new()),
                PlayCb::ClearDialog
            )
            .is_empty()
        );
    }
}

#[test]
fn dialog_custom_click_keeps_callback_and_text_response() {
    use elyzune_via::nbt::Tag;

    let response = Tag::Compound(vec![
        (
            b"id".to_vec(),
            Tag::string("93c1729c-b2e5-48dd-a1c8-d01c22a9f487"),
        ),
        (b"name".to_vec(), Tag::string("SkinnedPlayer")),
    ]);
    for client in CLIENTS {
        let mut t = play(client);
        for payload in [None, Some(&response)] {
            let mut body = Vec::new();
            body.put_str("paper:dialog_click_callback");
            match payload {
                Some(tag) => tag.write(&mut body),
                None => body.put_u8(0),
            }
            assert_eq!(
                sb(&mut t, client, PlaySb::CustomClickAction, body.clone()),
                vec![(PlaySb::CustomClickAction, body)],
                "{client}",
            );
        }
    }
}

/// `container_set_slot` with a diamond sword carrying a custom name, a dyed
/// colour, an enchantment and the 26.1 `dye` component.
fn sword_slot(item: i32, name: i32, dyed: i32, ench: i32, dye: Option<i32>) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_varint(0);
    b.put_varint(1);
    b.put_i16(36);
    b.put_varint(1);
    b.put_varint(item);
    b.put_varint(if dye.is_some() { 4 } else { 3 });
    b.put_varint(0);
    b.put_varint(name);
    b.put_bytes(&nbt_string("Blade"));
    b.put_varint(dyed);
    b.put_i32(0x00FF_0000);
    b.put_varint(ench);
    b.put_varint(1);
    b.put_varint(5);
    b.put_varint(3);
    if let Some(dye) = dye {
        b.put_varint(dye);
        b.put_varint(14);
    }
    b
}

#[test]
fn item_and_component_ids() {
    // registries.json: diamond_sword 936/937/964/1050; custom_name 6 and
    // enchantments 13 everywhere; dyed_color 42/44/44/47; dye -/43/43/46.
    let body = sword_slot(964, 6, 44, 13, Some(43));
    for (client, expected) in [
        (774, sword_slot(936, 6, 42, 13, None)),
        (775, sword_slot(937, 6, 44, 13, Some(43))),
        (777, sword_slot(1050, 6, 47, 13, Some(46))),
    ] {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::ContainerSetSlot, body.clone()),
            PlayCb::ContainerSetSlot,
        );
        assert_eq!(got, expected, "{client}");
    }
}

#[test]
fn jukebox_playable_is_wrapped_for_1_21_11() {
    // jukebox_playable 64 (26.2) / 62 (1.21.11): 26.1 sends Holder<JukeboxSong>,
    // 1.21.11 an EitherHolder (Bool true, then the holder).
    let slot = |component: i32, either: bool| {
        let mut b = Vec::new();
        b.put_varint(0);
        b.put_varint(1);
        b.put_i16(0);
        b.put_varint(1);
        b.put_varint(if component == 64 { 1057 } else { 1028 }); // paper
        b.put_varint(1);
        b.put_varint(0);
        b.put_varint(component);
        if either {
            b.put_bool(true);
        }
        b.put_varint(3); // jukebox song id 2 (+1)
        b
    };
    let mut t = play(774);
    let got = one(
        cb(&mut t, 774, PlayCb::ContainerSetSlot, slot(64, false)),
        PlayCb::ContainerSetSlot,
    );
    assert_eq!(got, slot(62, true));
}

// ---- 26.3 movement and animation -------------------------------------------

#[test]
fn movement_and_animation_for_26_3() {
    let mut t = play(777);
    let mut pos = Vec::new();
    pos.put_varint(5);
    pos.put_i16(100);
    pos.put_i16(-20);
    pos.put_i16(0);
    pos.put_bool(true);
    let mut expected = Vec::new();
    expected.put_varint(5);
    expected.put_varint(1); // step count 0, on ground
    expected.put_i16(100);
    expected.put_i16(-20);
    expected.put_i16(0);
    assert_eq!(
        one(
            cb(&mut t, 777, PlayCb::MoveEntityPos, pos),
            PlayCb::MoveEntityPos
        ),
        expected
    );

    let mut rot = Vec::new();
    rot.put_varint(5);
    rot.put_bytes(&[10, 20]);
    rot.put_bool(false);
    let mut expected = Vec::new();
    expected.put_varint(5);
    expected.put_bool(false);
    expected.put_bytes(&[10, 20]);
    assert_eq!(
        one(
            cb(&mut t, 777, PlayCb::MoveEntityRot, rot),
            PlayCb::MoveEntityRot
        ),
        expected
    );

    let animate = |action: u8| {
        let mut b = Vec::new();
        b.put_varint(5);
        b.put_u8(action);
        b
    };
    let mut swing = Vec::new();
    swing.put_varint(5);
    swing.put_varint(1); // off hand
    swing.put_varint(1); // whack
    swing.put_varint(6);
    assert_eq!(
        one(
            cb(&mut t, 777, PlayCb::Animate, animate(3)),
            PlayCb::SwingAnimation
        ),
        swing
    );
    assert_eq!(
        one(
            cb(&mut t, 777, PlayCb::Animate, animate(4)),
            PlayCb::Animate
        ),
        animate(1)
    );
}

// ---- world time ----------------------------------------------------------------

#[test]
fn set_time_becomes_day_time_for_1_21_11() {
    // world_clock entries in order: overworld (0), the_end (1).
    let mut body = Vec::new();
    body.put_i64(1000);
    body.put_varint(1);
    body.put_varint(0);
    body.put_varlong(6000);
    body.put_f32(0.0);
    body.put_f32(1.0);
    let mut t = play(774);
    let mut expected = Vec::new();
    expected.put_i64(1000);
    expected.put_i64(6000);
    expected.put_bool(true);
    assert_eq!(
        one(cb(&mut t, 774, PlayCb::SetTime, body), PlayCb::SetTime),
        expected
    );
    // An update without the overworld clock advances the remembered day time.
    let mut body = Vec::new();
    body.put_i64(1100);
    body.put_varint(0);
    let mut expected = Vec::new();
    expected.put_i64(1100);
    expected.put_i64(6100);
    expected.put_bool(true);
    assert_eq!(
        one(cb(&mut t, 774, PlayCb::SetTime, body), PlayCb::SetTime),
        expected
    );
}

// ---- teams, sounds, particles ------------------------------------------------

#[test]
fn team_parameters_before_26_2() {
    let mut body = Vec::new();
    body.put_str("red");
    body.put_i8(0);
    body.put_bytes(&nbt_string("Red"));
    body.put_bytes(&nbt_string("["));
    body.put_bytes(&nbt_string("]"));
    body.put_varint(1);
    body.put_varint(2);
    body.put_bool(true);
    body.put_varint(12);
    body.put_u8(1);
    body.put_varint(1);
    body.put_str("Bot");
    let mut old = Vec::new();
    old.put_str("red");
    old.put_i8(0);
    old.put_bytes(&nbt_string("Red"));
    old.put_u8(1);
    old.put_varint(1);
    old.put_varint(2);
    old.put_varint(12);
    old.put_bytes(&nbt_string("["));
    old.put_bytes(&nbt_string("]"));
    old.put_varint(1);
    old.put_str("Bot");
    for (client, expected) in [(774, old.clone()), (775, old), (777, body.clone())] {
        let mut t = play(client);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::SetPlayerTeam, body.clone()),
                PlayCb::SetPlayerTeam
            ),
            expected,
            "{client}"
        );
    }
}

#[test]
fn sounds_missing_from_1_21_11_play_by_name() {
    // entity.baby_cat.ambient: 279 (26.2), 277 (26.1), 279 (26.3), absent in 1.21.11.
    let sound = |holder: &[u8]| {
        let mut b = holder.to_vec();
        b.put_varint(5); // source
        b.put_bytes(&[0; 12]);
        b.put_f32(1.0);
        b.put_f32(1.0);
        b.put_i64(77);
        b
    };
    let mut server = Vec::new();
    server.put_varint(280);
    let body = sound(&server);
    let mut inline = Vec::new();
    inline.put_varint(0);
    inline.put_str("minecraft:entity.baby_cat.ambient");
    inline.put_bool(false);
    let mut h775 = Vec::new();
    h775.put_varint(278);
    for (client, expected) in [
        (774, sound(&inline)),
        (775, sound(&h775)),
        (777, sound(&server)),
    ] {
        let mut t = play(client);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::Sound, body.clone()),
                PlayCb::Sound
            ),
            expected,
            "{client}"
        );
    }
}

#[test]
fn particles_are_renumbered_and_reordered_for_26_3() {
    // heart: 45 (1.21.11, 26.1), 52 (26.2), 55 (26.3).
    let mut body = Vec::new();
    body.put_bytes(&[1, 0]);
    body.put_f64(1.0);
    body.put_f64(2.0);
    body.put_f64(3.0);
    body.put_f32(0.1);
    body.put_f32(0.2);
    body.put_f32(0.3);
    body.put_f32(0.5);
    body.put_i32(4);
    body.put_varint(52);
    for client in [774, 775] {
        let mut t = play(client);
        let mut expected = body[..body.len() - 1].to_vec();
        expected.put_varint(45);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::LevelParticles, body.clone()),
                PlayCb::LevelParticles
            ),
            expected
        );
    }
    let mut t = play(777);
    let mut expected = Vec::new();
    expected.put_varint(55);
    expected.put_bytes(&[1, 0]);
    expected.put_f64(1.0);
    expected.put_f64(2.0);
    expected.put_f64(3.0);
    expected.put_f32(0.1);
    expected.put_f32(0.2);
    expected.put_f32(0.3);
    for _ in 0..3 {
        expected.put_f32(0.5);
    }
    expected.put_varint(4);
    expected.put_varint(0);
    assert_eq!(
        one(
            cb(&mut t, 777, PlayCb::LevelParticles, body),
            PlayCb::LevelParticles
        ),
        expected
    );
}

// ---- commands ------------------------------------------------------------------

#[test]
fn argument_parsers_shift_for_26_3() {
    // command_argument_type: dialog 55 -> 58, uuid 56 -> 61, string 5 everywhere.
    let graph = |dialog: i32, uuid: i32| {
        let mut b = Vec::new();
        b.put_varint(3);
        // root with two children
        b.put_u8(0);
        b.put_varint(2);
        b.put_varint(1);
        b.put_varint(2);
        // argument "d" (dialog), executable
        b.put_u8(0x02 | 0x04);
        b.put_varint(0);
        b.put_str("d");
        b.put_varint(dialog);
        // argument "u" (uuid) with suggestions
        b.put_u8(0x02 | 0x10);
        b.put_varint(0);
        b.put_str("u");
        b.put_varint(uuid);
        b.put_str("minecraft:ask_server");
        b.put_varint(0);
        b
    };
    let mut t = play(777);
    assert_eq!(
        one(
            cb(&mut t, 777, PlayCb::Commands, graph(55, 56)),
            PlayCb::Commands
        ),
        graph(58, 61)
    );
    for client in [774, 775] {
        let mut t = play(client);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::Commands, graph(55, 56)),
                PlayCb::Commands
            ),
            graph(55, 56)
        );
    }
}

// ---- serverbound -----------------------------------------------------------------

#[test]
fn interact_from_1_21_11() {
    let mut t = play(774);
    // interact at (0.5, 1.0, 0.25), main hand, not sneaking
    let mut at = Vec::new();
    at.put_varint(9);
    at.put_varint(2);
    at.put_f32(0.5);
    at.put_f32(1.0);
    at.put_f32(0.25);
    at.put_varint(0);
    at.put_bool(false);
    let got = sb(&mut t, 774, PlaySb::Interact, at);
    // LpVec3, scale 1: x = round((0.5 * 0.5 + 0.5) * 32766) = 24575,
    // y = 32766, z = round(0.625 * 32766) = 20479.
    let buffer: i64 = 1 | (24575 << 3) | (32766 << 18) | (20479i64 << 33);
    let mut expected = Vec::new();
    expected.put_varint(9);
    expected.put_varint(0);
    expected.put_u8(buffer as u8);
    expected.put_u8((buffer >> 8) as u8);
    expected.put_i32((buffer >> 16) as i32);
    expected.put_bool(false);
    assert_eq!(got, vec![(PlaySb::Interact, expected)]);

    // the plain "interact" a 1.21.11 client sends after "interact at"
    let mut plain = Vec::new();
    plain.put_varint(9);
    plain.put_varint(0);
    plain.put_varint(0);
    plain.put_bool(false);
    assert!(sb(&mut t, 774, PlaySb::Interact, plain).is_empty());

    let mut attack = Vec::new();
    attack.put_varint(9);
    attack.put_varint(1);
    attack.put_bool(true);
    let mut expected = Vec::new();
    expected.put_varint(9);
    assert_eq!(
        sb(&mut t, 774, PlaySb::Interact, attack),
        vec![(PlaySb::Attack, expected)]
    );
}

#[test]
fn spectate_entity_from_26_1() {
    let mut t = play(775);
    let mut body = Vec::new();
    body.put_varint(9);
    let mut expected = Vec::new();
    expected.put_varint(10);
    assert_eq!(
        sb(&mut t, 775, PlaySb::SpectateEntity, body),
        vec![(PlaySb::SpectatorAction, expected)]
    );
}

#[test]
fn serverbound_from_26_3() {
    let mut t = play(777);
    let mut confirm = Vec::new();
    confirm.put_varint(3);
    confirm.put_f64(1.5);
    confirm.put_f64(70.0);
    confirm.put_f64(-4.5);
    confirm.put_f32(90.0);
    confirm.put_f32(10.0);
    let got = sb(&mut t, 777, PlaySb::AcceptTeleportation, confirm);
    let mut accept = Vec::new();
    accept.put_varint(3);
    let mut moved = Vec::new();
    moved.put_f64(1.5);
    moved.put_f64(70.0);
    moved.put_f64(-4.5);
    moved.put_f32(90.0);
    moved.put_f32(10.0);
    moved.put_u8(0);
    assert_eq!(
        got,
        vec![
            (PlaySb::AcceptTeleportation, accept),
            (PlaySb::MovePlayerPosRot, moved)
        ]
    );

    assert_eq!(
        sb(&mut t, 777, PlaySb::Punch, Vec::new()),
        vec![(PlaySb::Swing, vec![0])]
    );

    let action = |status: i32| {
        let mut b = Vec::new();
        b.put_varint(status);
        b.put_i64(123);
        b.put_u8(1);
        b.put_varint(7);
        b
    };
    assert_eq!(
        sb(&mut t, 777, PlaySb::PlayerAction, action(3)),
        vec![(PlaySb::PlayerAction, action(2))]
    );
    assert!(sb(&mut t, 777, PlaySb::PlayerAction, action(1)).is_empty());

    let mut sign = Vec::new();
    sign.put_i64(55);
    for line in ["a", "b", "", "d"] {
        sign.put_str(line);
    }
    sign.put_varint(1);
    let mut expected = Vec::new();
    expected.put_i64(55);
    expected.put_bool(true);
    for line in ["a", "b", "", "d"] {
        expected.put_str(line);
    }
    assert_eq!(
        sb(&mut t, 777, PlaySb::SignUpdate, sign),
        vec![(PlaySb::SignUpdate, expected)]
    );
}

#[test]
fn hashed_stacks_from_26_3() {
    // container_click: diamond_sword 1050 -> 964, custom_name 6, dye 46 -> 43.
    let click = |item: i32, dye: i32| {
        let mut b = Vec::new();
        b.put_varint(0);
        b.put_varint(4);
        b.put_i16(36);
        b.put_i8(0);
        b.put_varint(0);
        b.put_varint(1);
        b.put_i16(36);
        b.put_bool(true);
        b.put_varint(item);
        b.put_varint(1);
        b.put_varint(2);
        b.put_varint(6);
        b.put_i32(0x1234);
        b.put_varint(dye);
        b.put_i32(0x5678);
        b.put_varint(0);
        b.put_bool(false);
        b
    };
    let mut t = play(777);
    assert_eq!(
        sb(&mut t, 777, PlaySb::ContainerClick, click(1050, 46)),
        vec![(PlaySb::ContainerClick, click(964, 43))]
    );
}

#[test]
fn creative_slot_from_1_21_11() {
    // set_creative_mode_slot uses delimited components; diamond_sword 936 ->
    // 964, dyed_color 42 -> 44.
    let slot = |item: i32, dyed: i32| {
        let mut b = Vec::new();
        b.put_i16(36);
        b.put_varint(1);
        b.put_varint(item);
        b.put_varint(1);
        b.put_varint(0);
        b.put_varint(dyed);
        b.put_varint(4);
        b.put_i32(0x00AB_CDEF);
        b
    };
    let mut t = play(774);
    assert_eq!(
        sb(&mut t, 774, PlaySb::SetCreativeModeSlot, slot(936, 42)),
        vec![(PlaySb::SetCreativeModeSlot, slot(964, 44))]
    );
}

/// Resource packs, transfers and cookies have the same layout in every
/// release: the bodies cross untouched, only the packet ids change.
#[test]
fn resource_packs_transfers_and_cookies_pass_through() {
    let pack_id = [3u8; 16];
    let mut push = Vec::new();
    push.put_bytes(&pack_id);
    push.put_str("http://127.0.0.1:41299/pack.zip");
    push.put_str("0123456789abcdef0123456789abcdef01234567");
    push.put_bool(true);
    push.put_bool(true);
    push.put_bytes(&nbt_string("Pack prompt"));
    let mut pop = Vec::new();
    pop.put_bool(true);
    pop.put_bytes(&pack_id);
    let mut transfer = Vec::new();
    transfer.put_str("127.0.0.1");
    transfer.put_varint(41201);
    let mut store = Vec::new();
    store.put_str("elyzune:session");
    store.put_len(3);
    store.put_bytes(&[1, 2, 3]);
    let mut request = Vec::new();
    request.put_str("elyzune:session");
    let mut answer = Vec::new();
    answer.put_bytes(&pack_id);
    answer.put_varint(3); // accepted
    let mut cookie = Vec::new();
    cookie.put_str("elyzune:session");
    cookie.put_bool(true);
    cookie.put_len(3);
    cookie.put_bytes(&[1, 2, 3]);

    for client in CLIENTS {
        let mut t = play(client);
        for (kind, body) in [
            (PlayCb::ResourcePackPush, &push),
            (PlayCb::ResourcePackPop, &pop),
            (PlayCb::Transfer, &transfer),
            (PlayCb::StoreCookie, &store),
            (PlayCb::CookieRequest, &request),
        ] {
            assert_eq!(
                one(cb(&mut t, client, kind, body.clone()), kind),
                *body,
                "{client} {kind:?}"
            );
        }
        for (kind, body) in [
            (PlaySb::ResourcePack, &answer),
            (PlaySb::CookieResponse, &cookie),
        ] {
            let sent = sb(&mut t, client, kind, body.clone());
            assert_eq!(sent, vec![(kind, body.clone())], "{client} {kind:?}");
        }
    }
}

/// `ELYZUNE_VIA_STATS` timing (`stats.rs`): a timed translator counts every
/// packet it handles, per direction, with the bytes it was given; an untimed
/// one keeps nothing.
#[test]
fn timed_translators_count_packets_per_direction() {
    let mut t = ViaTranslator::timed(Proto::V774, 3);
    let mut out = TranslatorOutput::new();
    let mut hs = Vec::new();
    hs.put_varint(774);
    hs.put_str("localhost");
    hs.put_bytes(&25565u16.to_be_bytes());
    hs.put_varint(2);
    let hs_len = hs.len() as u64;
    t.serverbound(RawPacket::new(0, hs.into()), &mut out);
    let mut finished = Vec::new();
    finished.put_bytes(&[7; 16]);
    finished.put_str("Bot");
    finished.put_varint(0);
    finished.put_bytes(&[9; 16]);
    let finished_len = finished.len() as u64;
    t.clientbound(
        RawPacket::new(
            LoginCb::LoginFinished.id(Proto::V776).unwrap(),
            finished.into(),
        ),
        &mut out,
    );
    let timing = t.timing().unwrap();
    assert_eq!(
        (timing.serverbound.packets, timing.serverbound.bytes),
        (1, hs_len)
    );
    assert_eq!(
        (timing.clientbound.packets, timing.clientbound.bytes),
        (1, finished_len)
    );
    assert!(timing.clientbound.max_ns >= timing.clientbound.mean_ns());
    let line = timing.line(3, 774);
    assert!(line.starts_with("elyzune-via stats: connection=3 client=774 cb_packets=1 "));

    // The environment is not set for the test run: `new` keeps no timing.
    assert!(ViaTranslator::new(Proto::V774, 4).timing().is_none());
}
