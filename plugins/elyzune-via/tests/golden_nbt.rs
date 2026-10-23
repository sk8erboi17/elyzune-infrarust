//! Golden translations of data in codec form: text components with hovers,
//! block entity update tags, item components whose payload is NBT, and the
//! 26.3 components that changed identity.
//!
//! Inputs are hand-built 26.2 (776) packets; expected outputs are written
//! out tag by tag. Registry ids are the numbers of each release's
//! `registries.json` (data generator reports), cited where they appear;
//! names follow the same reports.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use elyzune_via::generated::Proto;
use elyzune_via::generated::dynamic::SYNCED_776;
use elyzune_via::generated::packets::{ConfigCb, ConfigSb, LoginCb, LoginSb, PlayCb, PlaySb};
use elyzune_via::nbt::Tag;
use elyzune_via::session::{ViaTranslator, WireState};
use elyzune_via::wire::Put;
use infrarust_api::filter::{ProtocolTranslator, TranslatorOutput};
use infrarust_api::types::RawPacket;

const CLIENTS: [i32; 3] = [774, 775, 777];

fn proto(n: i32) -> Proto {
    Proto::from_number(n).unwrap()
}

/// A translator in play, with every 26.2 synchronised registry sent by name.
fn play(client: i32) -> ViaTranslator {
    let p = proto(client);
    let mut t = config(client);
    let mut out = TranslatorOutput::new();
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

/// A translator in configuration, registries sent.
fn config(client: i32) -> ViaTranslator {
    let p = proto(client);
    let mut t = ViaTranslator::new(p, 1);
    let mut out = TranslatorOutput::new();
    let mut hs = Vec::new();
    hs.put_varint(client);
    hs.put_str("localhost");
    hs.put_bytes(&25565u16.to_be_bytes());
    hs.put_varint(2);
    t.serverbound(RawPacket::new(0, hs.into()), &mut out);
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
    t
}

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

// ---- NBT builders ------------------------------------------------------------

fn s(v: &str) -> Tag {
    Tag::string(v)
}

fn c(entries: Vec<(&str, Tag)>) -> Tag {
    Tag::Compound(
        entries
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect(),
    )
}

fn l(items: Vec<Tag>) -> Tag {
    let ty = items.first().map_or(0, Tag::type_id);
    Tag::List(ty, items)
}

fn nbt(t: &Tag) -> Vec<u8> {
    let mut v = Vec::new();
    t.write(&mut v);
    v
}

/// The `custom_data` value a translated slot stack is sent with when its
/// server-side hashes are backed up (`elyzune_via::hashed`, "Telling apart
/// stacks the client sees as one"), created by the translator: the
/// server's item, the server hashes the client cannot reproduce by server
/// component id, the server's removed ids. Returns its network NBT and the
/// hash a client claims for it (`HashOps` over NBT, pinned against the jar
/// by `hashed::tests::nbt_matches_the_jar_vector`).
fn backup(server_item: i32, hashes: &[(i32, i32)], removed: &[i32]) -> (Vec<u8>, i32) {
    let mut original = vec![(b"id".to_vec(), Tag::Int(server_item))];
    for (ty, hash) in hashes {
        original.push((ty.to_string().into_bytes(), Tag::Int(*hash)));
    }
    original.push((b"removed".to_vec(), Tag::IntArray(removed.to_vec())));
    let tag = Tag::Compound(vec![
        (b"elyzune-via|custom_data".to_vec(), Tag::Byte(1)),
        (
            b"elyzune-via|original_hashes".to_vec(),
            Tag::Compound(original),
        ),
    ]);
    let key = elyzune_via::hashed::as_int(elyzune_via::hashed::nbt(&tag).unwrap());
    (nbt(&tag), key)
}

/// `{text, hover_event: show_entity <entity>}`.
fn entity_hover(text: &str, entity: &str) -> Tag {
    c(vec![
        ("text", s(text)),
        (
            "hover_event",
            c(vec![
                ("action", s("show_entity")),
                ("id", s(entity)),
                ("uuid", Tag::IntArray(vec![1, 2, 3, 4])),
            ]),
        ),
    ])
}

/// The client's name of the 26.2-only `sulfur_cube` entity type
/// (elyzune-via's REGISTRY_FALLBACK shows it as a slime before 26.2).
fn sulfur_cube(client: i32) -> &'static str {
    if client == 777 {
        "minecraft:sulfur_cube"
    } else {
        "minecraft:slime"
    }
}

/// The client's name of 26.2's `cinnabar_bricks` (cut red sandstone before 26.2).
fn cinnabar_bricks(client: i32) -> &'static str {
    if client == 777 {
        "minecraft:cinnabar_bricks"
    } else {
        "minecraft:cut_red_sandstone"
    }
}

// ---- text hovers -------------------------------------------------------------

/// A `show_item` hover with everything that differs between releases:
/// a 26.2-only item, a nested entity hover in its name, a 26.2-only
/// component, the 26.2 swing animation, a map colour, pot decorations and a
/// removed component.
fn item_hover_776() -> Tag {
    c(vec![
        ("text", s("look")),
        (
            "hover_event",
            c(vec![
                ("action", s("show_item")),
                ("id", s("minecraft:cinnabar_bricks")),
                ("count", Tag::Int(2)),
                (
                    "components",
                    c(vec![
                        (
                            "minecraft:custom_name",
                            entity_hover("Brick", "minecraft:sulfur_cube"),
                        ),
                        ("minecraft:sulfur_cube_content", c(vec![])),
                        (
                            "minecraft:swing_animation",
                            c(vec![("type", s("whack")), ("duration", Tag::Int(6))]),
                        ),
                        ("minecraft:map_color", Tag::Int(123)),
                        (
                            "minecraft:pot_decorations",
                            l(vec![s("minecraft:angler_pottery_sherd")]),
                        ),
                        ("!minecraft:lore", c(vec![])),
                    ]),
                ),
            ]),
        ),
    ])
}

fn item_hover_for(client: i32) -> Tag {
    let swing = c(vec![("type", s("whack")), ("duration", Tag::Int(6))]);
    let name = entity_hover("Brick", sulfur_cube(client));
    let components = match client {
        // 1.21.11 and 26.1 have no sulfur cube content; the rest is shared.
        774 | 775 => c(vec![
            ("minecraft:custom_name", name),
            ("minecraft:swing_animation", swing),
            ("minecraft:map_color", Tag::Int(123)),
            (
                "minecraft:pot_decorations",
                l(vec![s("minecraft:angler_pottery_sherd")]),
            ),
            ("!minecraft:lore", c(vec![])),
        ]),
        // 26.3: swing split in two, no map colour, decorations by side.
        _ => c(vec![
            ("minecraft:custom_name", name),
            ("minecraft:sulfur_cube_content", c(vec![])),
            ("minecraft:attack_animation", swing.clone()),
            ("minecraft:interact_animation", swing),
            (
                "minecraft:pot_decorations",
                c(vec![
                    ("back", c(vec![("id", s("minecraft:angler_pottery_sherd"))])),
                    ("left", c(vec![("id", s("minecraft:brick"))])),
                    ("right", c(vec![("id", s("minecraft:brick"))])),
                    ("front", c(vec![("id", s("minecraft:brick"))])),
                ]),
            ),
            ("!minecraft:lore", c(vec![])),
        ]),
    };
    c(vec![
        ("text", s("look")),
        (
            "hover_event",
            c(vec![
                ("action", s("show_item")),
                ("id", s(cinnabar_bricks(client))),
                ("count", Tag::Int(2)),
                ("components", components),
            ]),
        ),
    ])
}

#[test]
fn show_item_hovers_are_rewritten_per_release() {
    for client in CLIENTS {
        let mut t = play(client);
        let mut body = nbt(&item_hover_776());
        body.put_bool(false);
        let got = one(
            cb(&mut t, client, PlayCb::SystemChat, body),
            PlayCb::SystemChat,
        );
        let mut expected = nbt(&item_hover_for(client));
        expected.put_bool(false);
        assert_eq!(got, expected, "{client}");
    }
}

#[test]
fn plain_text_is_copied_byte_for_byte() {
    // Even text that spells an action name without being a hover.
    let text = c(vec![
        ("text", s("try show_item now")),
        ("extra", l(vec![s("x")])),
        ("color", s("#88C0E8")),
    ]);
    for client in CLIENTS {
        let mut t = play(client);
        let mut body = nbt(&text);
        body.put_bool(true);
        let got = one(
            cb(&mut t, client, PlayCb::SystemChat, body.clone()),
            PlayCb::SystemChat,
        );
        assert_eq!(got, body, "{client}");
    }
}

#[test]
fn a_hover_on_an_item_with_no_equivalent_is_removed_not_sent_broken() {
    // A hover whose action is unknown to every release stays; one naming an
    // entity no release has would fail the client's decode, so it goes.
    let text = c(vec![
        ("text", s("x")),
        (
            "hover_event",
            c(vec![
                ("action", s("show_entity")),
                ("id", s("minecraft:no_such_entity")),
                ("uuid", Tag::IntArray(vec![0, 0, 0, 1])),
            ]),
        ),
    ]);
    for client in CLIENTS {
        let mut t = play(client);
        let mut body = nbt(&text);
        body.put_bool(false);
        let got = one(
            cb(&mut t, client, PlayCb::SystemChat, body),
            PlayCb::SystemChat,
        );
        let mut expected = nbt(&c(vec![("text", s("x"))]));
        expected.put_bool(false);
        assert_eq!(got, expected, "{client}");
    }
}

/// Every packet family that carries text, each with an entity hover naming
/// the 26.2 sulfur cube: the handler must find the text in its layout.
#[test]
fn every_text_packet_rewrites_its_text() {
    let hover = |client: i32| nbt(&entity_hover("t", sulfur_cube(client)));
    let h776 = nbt(&entity_hover("t", "minecraft:sulfur_cube"));
    for client in CLIENTS {
        let mut t = play(client);
        let h = hover(client);

        // Titles, action bar: the text alone.
        for kind in [
            PlayCb::SetTitleText,
            PlayCb::SetSubtitleText,
            PlayCb::SetActionBarText,
        ] {
            let got = one(cb(&mut t, client, kind, h776.clone()), kind);
            assert_eq!(got, h, "{client} {kind:?}");
        }

        // tab_list: header and footer.
        let mut body = h776.clone();
        body.extend_from_slice(&h776);
        let mut expected = h.clone();
        expected.extend_from_slice(&h);
        assert_eq!(
            one(cb(&mut t, client, PlayCb::TabList, body), PlayCb::TabList),
            expected
        );

        // boss_event add: UUID, op 0, title, progress, colour, overlay, flags.
        let mut body = vec![5u8; 16];
        body.put_varint(0);
        body.extend_from_slice(&h776);
        body.put_f32(0.5);
        body.put_varint(1);
        body.put_varint(0);
        body.put_u8(0);
        let mut expected = vec![5u8; 16];
        expected.put_varint(0);
        expected.extend_from_slice(&h);
        expected.put_f32(0.5);
        expected.put_varint(1);
        expected.put_varint(0);
        expected.put_u8(0);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::BossEvent, body),
                PlayCb::BossEvent
            ),
            expected
        );

        // set_objective add with a styled number format whose style hovers.
        let style_776 = nbt(&c(vec![(
            "hover_event",
            c(vec![
                ("action", s("show_entity")),
                ("id", s("minecraft:sulfur_cube")),
                ("uuid", Tag::IntArray(vec![1, 2, 3, 4])),
            ]),
        )]));
        let style = nbt(&c(vec![(
            "hover_event",
            c(vec![
                ("action", s("show_entity")),
                ("id", s(sulfur_cube(client))),
                ("uuid", Tag::IntArray(vec![1, 2, 3, 4])),
            ]),
        )]));
        let objective = |text: &[u8], style: &[u8]| {
            let mut b = Vec::new();
            b.put_str("kills");
            b.put_i8(0);
            b.put_bytes(text);
            b.put_varint(0);
            b.put_bool(true);
            b.put_varint(1);
            b.put_bytes(style);
            b
        };
        assert_eq!(
            one(
                cb(
                    &mut t,
                    client,
                    PlayCb::SetObjective,
                    objective(&h776, &style_776)
                ),
                PlayCb::SetObjective
            ),
            objective(&h, &style)
        );

        // set_score: owner, objective, score, display, fixed number format.
        let score = |text: &[u8]| {
            let mut b = Vec::new();
            b.put_str("Bot");
            b.put_str("kills");
            b.put_varint(3);
            b.put_bool(true);
            b.put_bytes(text);
            b.put_bool(true);
            b.put_varint(2);
            b.put_bytes(text);
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::SetScore, score(&h776)),
                PlayCb::SetScore
            ),
            score(&h)
        );

        // player_info_update: ADD_PLAYER | UPDATE_DISPLAY_NAME | UPDATE_HAT.
        let info = |text: &[u8]| {
            let mut b = Vec::new();
            b.put_u8(0b1010_0001);
            b.put_varint(1);
            b.put_bytes(&[3; 16]);
            b.put_str("Bot");
            b.put_varint(1);
            b.put_str("textures");
            b.put_str("e30=");
            b.put_bool(false);
            b.put_bool(true);
            b.put_bytes(text);
            b.put_bool(true);
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::PlayerInfoUpdate, info(&h776)),
                PlayCb::PlayerInfoUpdate
            ),
            info(&h)
        );

        // open_screen: container, menu (generic_9x3 = 2 everywhere), title.
        let screen = |text: &[u8]| {
            let mut b = Vec::new();
            b.put_varint(3);
            b.put_varint(2);
            b.put_bytes(text);
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::OpenScreen, screen(&h776)),
                PlayCb::OpenScreen
            ),
            screen(&h)
        );

        // resource_pack_push with a prompt.
        let pack = |text: &[u8]| {
            let mut b = vec![1u8; 16];
            b.put_str("http://127.0.0.1/p.zip");
            b.put_str("0123456789012345678901234567890123456789");
            b.put_bool(true);
            b.put_bool(true);
            b.put_bytes(text);
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::ResourcePackPush, pack(&h776)),
                PlayCb::ResourcePackPush
            ),
            pack(&h)
        );

        // map_item_data: one decoration (player, 0) named.
        let map = |text: &[u8]| {
            let mut b = Vec::new();
            b.put_varint(7);
            b.put_i8(0);
            b.put_bool(false);
            b.put_bool(true);
            b.put_varint(1);
            b.put_varint(0);
            b.put_i8(1);
            b.put_i8(2);
            b.put_i8(3);
            b.put_bool(true);
            b.put_bytes(text);
            b.put_u8(0); // no colour patch
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::MapItemData, map(&h776)),
                PlayCb::MapItemData
            ),
            map(&h)
        );

        // server_links: one custom label, one known type.
        let links = |text: &[u8]| {
            let mut b = Vec::new();
            b.put_varint(2);
            b.put_bool(false);
            b.put_bytes(text);
            b.put_str("https://elyzune.example");
            b.put_bool(true);
            b.put_varint(4);
            b.put_str("https://elyzune.example/news");
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::ServerLinks, links(&h776)),
                PlayCb::ServerLinks
            ),
            links(&h)
        );

        // command_suggestions with a tooltip.
        let suggest = |text: &[u8]| {
            let mut b = Vec::new();
            b.put_varint(1);
            b.put_varint(0);
            b.put_varint(4);
            b.put_varint(1);
            b.put_str("spawn");
            b.put_bool(true);
            b.put_bytes(text);
            b
        };
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::CommandSuggestions, suggest(&h776)),
                PlayCb::CommandSuggestions
            ),
            suggest(&h)
        );

        // player_combat_kill and disconnect.
        let mut body = Vec::new();
        body.put_varint(9);
        body.extend_from_slice(&h776);
        let mut expected = Vec::new();
        expected.put_varint(9);
        expected.extend_from_slice(&h);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::PlayerCombatKill, body),
                PlayCb::PlayerCombatKill
            ),
            expected
        );
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::Disconnect, h776.clone()),
                PlayCb::Disconnect
            ),
            h
        );
    }
}

#[test]
fn team_names_are_rewritten_on_every_layout() {
    let team = |text: &[u8], client: i32| {
        let mut b = Vec::new();
        b.put_str("red");
        b.put_i8(0);
        if client >= 776 {
            b.put_bytes(text);
            b.put_bytes(text);
            b.put_bytes(text);
            b.put_varint(0);
            b.put_varint(0);
            b.put_bool(true);
            b.put_varint(12);
            b.put_u8(1);
        } else {
            b.put_bytes(text);
            b.put_u8(1);
            b.put_varint(0);
            b.put_varint(0);
            b.put_varint(12);
            b.put_bytes(text);
            b.put_bytes(text);
        }
        b.put_varint(1);
        b.put_str("Bot");
        b
    };
    let h776 = nbt(&entity_hover("t", "minecraft:sulfur_cube"));
    for client in CLIENTS {
        let mut t = play(client);
        let h = nbt(&entity_hover("t", sulfur_cube(client)));
        let got = one(
            cb(&mut t, client, PlayCb::SetPlayerTeam, team(&h776, 776)),
            PlayCb::SetPlayerTeam,
        );
        assert_eq!(got, team(&h, client), "{client}");
    }
}

#[test]
fn entity_custom_names_are_rewritten() {
    // set_entity_data of the player itself (tracked from login): index 2
    // DATA_CUSTOM_NAME, serializer 6 OPTIONAL_COMPONENT in every release.
    let data = |text: &[u8]| {
        let mut b = Vec::new();
        b.put_varint(0);
        b.put_u8(2);
        b.put_varint(6);
        b.put_bool(true);
        b.put_bytes(text);
        b.put_u8(0xFF);
        b
    };
    let h776 = nbt(&entity_hover("t", "minecraft:sulfur_cube"));
    for client in CLIENTS {
        let mut t = play(client);
        let h = nbt(&entity_hover("t", sulfur_cube(client)));
        let got = one(
            cb(&mut t, client, PlayCb::SetEntityData, data(&h776)),
            PlayCb::SetEntityData,
        );
        assert_eq!(got, data(&h), "{client}");
    }
}

#[test]
fn configuration_text_and_dialogs_are_rewritten() {
    let h776 = nbt(&entity_hover("t", "minecraft:sulfur_cube"));
    let dialog = |client: i32| {
        c(vec![
            ("type", s("minecraft:notice")),
            (
                "title",
                entity_hover(
                    "t",
                    if client == 776 {
                        "minecraft:sulfur_cube"
                    } else {
                        sulfur_cube(client)
                    },
                ),
            ),
            (
                "body",
                l(vec![c(vec![
                    ("type", s("minecraft:item")),
                    (
                        "item",
                        c(vec![(
                            "id",
                            s(if client == 776 {
                                "minecraft:cinnabar_bricks"
                            } else {
                                cinnabar_bricks(client)
                            }),
                        )]),
                    ),
                ])]),
            ),
        ])
    };
    for client in CLIENTS {
        let mut t = config(client);
        let mut out = TranslatorOutput::new();
        t.clientbound(
            RawPacket::new(
                ConfigCb::Disconnect.id(Proto::V776).unwrap(),
                h776.clone().into(),
            ),
            &mut out,
        );
        t.clientbound(
            RawPacket::new(
                ConfigCb::ShowDialog.id(Proto::V776).unwrap(),
                nbt(&dialog(776)).into(),
            ),
            &mut out,
        );
        let got: Vec<RawPacket> = out.take_to_client().collect();
        assert_eq!(got.len(), 2);
        assert_eq!(
            ConfigCb::from_id(proto(client), got[0].packet_id),
            Some(ConfigCb::Disconnect)
        );
        assert_eq!(
            got[0].data.as_ref(),
            nbt(&entity_hover("t", sulfur_cube(client))).as_slice()
        );
        assert_eq!(
            ConfigCb::from_id(proto(client), got[1].packet_id),
            Some(ConfigCb::ShowDialog)
        );
        assert_eq!(
            got[1].data.as_ref(),
            nbt(&dialog(client)).as_slice(),
            "{client}"
        );
    }
}

// ---- block entities ------------------------------------------------------------

/// A chunk at (0, 0) whose one section is air, carrying `entities`
/// `(packed xz, y, type, tag)`, with empty light.
fn chunk_with(client: i32, entities: &[(u8, i16, i32, Tag)]) -> Vec<u8> {
    let mut section = Vec::new();
    section.put_i16(0);
    if client != 774 {
        section.put_i16(0);
    }
    section.put_u8(0);
    section.put_varint(0);
    section.put_u8(0);
    section.put_varint(0);
    let mut b = Vec::new();
    b.put_i32(0);
    b.put_i32(0);
    b.put_varint(0);
    b.put_len(section.len());
    b.put_bytes(&section);
    b.put_len(entities.len());
    for (xz, y, ty, tag) in entities {
        b.put_u8(*xz);
        b.put_i16(*y);
        b.put_varint(*ty);
        b.put_bytes(&nbt(tag));
    }
    for _ in 0..6 {
        b.put_varint(0);
    }
    b
}

fn sign_tag(entity: &str) -> Tag {
    let side = |first: Tag| {
        c(vec![
            (
                "messages",
                l(vec![
                    first,
                    c(vec![("text", s(""))]),
                    c(vec![("text", s(""))]),
                    c(vec![("text", s(""))]),
                ]),
            ),
            ("color", s("black")),
            ("has_glowing_text", Tag::Byte(0)),
        ])
    };
    c(vec![
        ("front_text", side(entity_hover("Elyzune", entity))),
        ("back_text", side(c(vec![("text", s(""))]))),
        ("is_waxed", Tag::Byte(1)),
    ])
}

fn pot_776() -> Tag {
    c(vec![
        (
            "sherds",
            l(vec![
                s("minecraft:angler_pottery_sherd"),
                s("minecraft:brick"),
                s("minecraft:brick"),
                s("minecraft:brick"),
            ]),
        ),
        (
            "item",
            c(vec![
                ("id", s("minecraft:cinnabar_bricks")),
                ("count", Tag::Int(1)),
            ]),
        ),
    ])
}

fn pot_for(client: i32) -> Tag {
    let sherds = if client == 777 {
        c(vec![
            ("back", c(vec![("id", s("minecraft:angler_pottery_sherd"))])),
            ("left", c(vec![("id", s("minecraft:brick"))])),
            ("right", c(vec![("id", s("minecraft:brick"))])),
            ("front", c(vec![("id", s("minecraft:brick"))])),
        ])
    } else {
        l(vec![
            s("minecraft:angler_pottery_sherd"),
            s("minecraft:brick"),
            s("minecraft:brick"),
            s("minecraft:brick"),
        ])
    };
    c(vec![
        ("sherds", sherds),
        (
            "item",
            c(vec![
                ("id", s(cinnabar_bricks(client))),
                ("count", Tag::Int(1)),
            ]),
        ),
    ])
}

fn campfire_tag(item: &str) -> Tag {
    c(vec![(
        "Items",
        l(vec![c(vec![
            ("Slot", Tag::Byte(0)),
            ("id", s(item)),
            ("count", Tag::Int(1)),
        ])]),
    )])
}

/// `block_entity_type` ids: sign 7, banner 20 in every release;
/// decorated_pot 42 (1.21.11, 26.1) / 41 (26.2, 26.3); campfire 33 / 32.
#[test]
fn block_entity_tags_in_chunks() {
    let pot = |client: i32| if client >= 776 { 41 } else { 42 };
    let campfire = |client: i32| if client >= 776 { 32 } else { 33 };
    // golden_dandelion is new in 26.1: 1.21.11 shows a dandelion.
    let dandelion = |client: i32| {
        if client == 774 {
            "minecraft:dandelion"
        } else {
            "minecraft:golden_dandelion"
        }
    };
    let banner = |pattern: &str| {
        c(vec![
            (
                "patterns",
                l(vec![
                    c(vec![("pattern", s(pattern)), ("color", s("red"))]),
                    c(vec![
                        ("pattern", s("minecraft:stripe_top")),
                        ("color", s("blue")),
                    ]),
                ]),
            ),
            (
                "components",
                c(vec![(
                    "minecraft:item_name",
                    entity_hover("B", "minecraft:sulfur_cube"),
                )]),
            ),
        ])
    };
    let banner_for = |client: i32| {
        c(vec![
            (
                "patterns",
                l(vec![c(vec![
                    ("pattern", s("minecraft:stripe_top")),
                    ("color", s("blue")),
                ])]),
            ),
            (
                "components",
                c(vec![(
                    "minecraft:item_name",
                    entity_hover("B", sulfur_cube(client)),
                )]),
            ),
        ])
    };
    let server = chunk_with(
        776,
        &[
            (0x11, 64, 7, sign_tag("minecraft:sulfur_cube")),
            (0x22, 64, 41, pot_776()),
            (0x33, 64, 32, campfire_tag("minecraft:golden_dandelion")),
            // A pattern no release has goes; the layer after it stays.
            (0x44, 64, 20, banner("minecraft:no_such_pattern")),
        ],
    );
    for client in CLIENTS {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::LevelChunkWithLight, server.clone()),
            PlayCb::LevelChunkWithLight,
        );
        let expected = chunk_with(
            client,
            &[
                (0x11, 64, 7, sign_tag(sulfur_cube(client))),
                (0x22, 64, pot(client), pot_for(client)),
                (0x33, 64, campfire(client), campfire_tag(dandelion(client))),
                (0x44, 64, 20, banner_for(client)),
            ],
        );
        // Empty light: four empty masks (long arrays before 26.3, byte
        // arrays in 26.3: the same zero lengths) and two empty lists.
        assert_eq!(got, expected, "{client}");
    }
}

#[test]
fn block_entity_data_packets() {
    let packet = |ty: i32, tag: &Tag| {
        let mut b = Vec::new();
        b.put_i64(0x0000_0040_0010_0005);
        b.put_varint(ty);
        b.put_bytes(&nbt(tag));
        b
    };
    for client in CLIENTS {
        let mut t = play(client);
        let got = one(
            cb(
                &mut t,
                client,
                PlayCb::BlockEntityData,
                packet(41, &pot_776()),
            ),
            PlayCb::BlockEntityData,
        );
        let ty = if client >= 776 { 41 } else { 42 };
        assert_eq!(got, packet(ty, &pot_for(client)), "{client}");
        // A sign whose text has no hover is copied without being parsed.
        let plain = c(vec![(
            "front_text",
            c(vec![("messages", l(vec![s("a"), s("b"), s("c"), s("d")]))]),
        )]);
        let got = one(
            cb(&mut t, client, PlayCb::BlockEntityData, packet(7, &plain)),
            PlayCb::BlockEntityData,
        );
        assert_eq!(got, packet(7, &plain), "{client}");
    }
}

// ---- items in network form -----------------------------------------------------

/// `container_set_slot` of one stack: container 0, state 1, slot 36.
fn slot(item: i32, components: &[(i32, Vec<u8>)], removed: &[i32]) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_varint(0);
    b.put_varint(1);
    b.put_i16(36);
    b.put_varint(1);
    b.put_varint(item);
    b.put_varint(i32::try_from(components.len()).unwrap());
    b.put_varint(i32::try_from(removed.len()).unwrap());
    for (ty, value) in components {
        b.put_varint(*ty);
        b.put_bytes(value);
    }
    for ty in removed {
        b.put_varint(*ty);
    }
    b
}

/// `SwingAnimation` network form: VarInt type (1 whack), VarInt duration.
fn swing() -> Vec<u8> {
    let mut v = Vec::new();
    v.put_varint(1);
    v.put_varint(6);
    v
}

/// `block_entity_data` component: VarInt type, NBT.
fn typed(ty: i32, tag: &Tag) -> Vec<u8> {
    let mut v = Vec::new();
    v.put_varint(ty);
    v.put_bytes(&nbt(tag));
    v
}

/// data_component_type ids (registries.json): swing_animation 40 (774-776),
/// attack_animation 40 / interact_animation 41 (777); block_entity_data 58
/// (774), 60 (775, 776), 62 (777). Items: diamond_sword 936/937/964/1050,
/// campfire block entity 33 (774, 775) / 32.
#[test]
fn item_components_carrying_nbt_and_26_3_animations() {
    let sword = |client: i32| match client {
        774 => 936,
        775 => 937,
        776 => 964,
        _ => 1050,
    };
    let bed_component = |client: i32| match client {
        774 => 58,
        775 | 776 => 60,
        _ => 62,
    };
    let campfire = |client: i32| if client >= 776 { 32 } else { 33 };
    let dandelion = |client: i32| {
        if client == 774 {
            "minecraft:dandelion"
        } else {
            "minecraft:golden_dandelion"
        }
    };
    let server = slot(
        sword(776),
        &[
            (40, swing()),
            (60, typed(32, &campfire_tag("minecraft:golden_dandelion"))),
        ],
        &[40],
    );
    for client in CLIENTS {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::ContainerSetSlot, server.clone()),
            PlayCb::ContainerSetSlot,
        );
        let data = (
            bed_component(client),
            typed(campfire(client), &campfire_tag(dandelion(client))),
        );
        // 26.3's split swing needs no backup (its two animations are
        // claimed as the one swing), nor does a block entity whose type
        // was only renumbered (26.1). 1.21.11's campfire holds the
        // dandelion shown for 26.1's golden dandelion, so its data hashes
        // differently and the server's hash is backed up: 801797658 for
        // {id: campfire, Items: [{Slot: 0b, id: golden_dandelion, count:
        // 1}]} (an independent Python transcription of HashOps).
        let expected = match client {
            777 => slot(
                sword(client),
                &[(40, swing()), (41, swing()), data],
                &[40, 41],
            ),
            774 => slot(
                sword(client),
                &[
                    (40, swing()),
                    data,
                    (0, backup(964, &[(60, 801_797_658)], &[40]).0),
                ],
                &[40],
            ),
            _ => slot(sword(client), &[(40, swing()), data], &[40]),
        };
        assert_eq!(got, expected, "{client}");
    }
}

/// 26.3 creative slot (delimited patch): an oak sign carrying 26.3's sign
/// text, waxed flag and attack animation reaches 26.2 as block_entity_data
/// (type sign = 7) and swing_animation.
///
/// Ids: oak_sign 1102 (777) / 1016 (776); sign_text_front 118, waxed 120,
/// attack_animation 40 (777); swing_animation 40, block_entity_data 60 (776).
#[test]
fn creative_sign_items_from_26_3_fold_into_block_entity_data() {
    let mut t = play(777);
    let mut sign_text = Vec::new();
    for line in ["Hi", "", "", ""] {
        sign_text.put_bytes(&nbt(&s(line)));
    }
    sign_text.put_bool(false);
    sign_text.put_varint(14); // red
    sign_text.put_bool(true);
    let mut body = Vec::new();
    body.put_i16(36);
    body.put_varint(1);
    body.put_varint(1102);
    body.put_varint(3);
    body.put_varint(0);
    for (ty, value) in [(118, sign_text), (120, Vec::new()), (40, swing())] {
        body.put_varint(ty);
        body.put_len(value.len());
        body.put_bytes(&value);
    }
    let got = sb(&mut t, 777, PlaySb::SetCreativeModeSlot, body);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, PlaySb::SetCreativeModeSlot);
    let front = c(vec![
        (
            "messages",
            l(vec![
                c(vec![("text", s("Hi"))]),
                c(vec![("text", s(""))]),
                c(vec![("text", s(""))]),
                c(vec![("text", s(""))]),
            ]),
        ),
        ("color", s("red")),
        ("has_glowing_text", Tag::Byte(1)),
    ]);
    let data = typed(
        7,
        &c(vec![("front_text", front), ("is_waxed", Tag::Byte(1))]),
    );
    let mut expected = Vec::new();
    expected.put_i16(36);
    expected.put_varint(1);
    expected.put_varint(1016);
    expected.put_varint(2);
    expected.put_varint(0);
    let swing = swing();
    for (ty, value) in [(40, &swing), (60, &data)] {
        expected.put_varint(ty);
        expected.put_len(value.len());
        expected.put_bytes(value);
    }
    assert_eq!(got[0].1, expected);
}

/// 26.2 container slot: an oak sign whose block_entity_data holds its front
/// text and wax reaches 26.3 as sign_text_front and waxed, without the data
/// (the reverse of the creative fold above: what a 26.3 server's
/// `collectImplicitComponents` and `removeComponentsFromTag` send), so a
/// 26.3 client sees the stack as its own release writes it. Data with more
/// than the text keeps the rest.
///
/// Ids as above; hanging sign block entity 8 (776).
#[test]
fn sign_items_for_26_3_carry_their_text_as_components() {
    let line = |text: &str| c(vec![("text", s(text))]);
    let front = c(vec![
        (
            "messages",
            l(vec![line("Hi"), line(""), line(""), line("")]),
        ),
        ("color", s("red")),
        ("has_glowing_text", Tag::Byte(1)),
    ]);
    let server = slot(
        1016,
        &[(
            60,
            typed(
                7,
                &c(vec![
                    ("front_text", front.clone()),
                    ("is_waxed", Tag::Byte(1)),
                ]),
            ),
        )],
        &[],
    );
    let mut t = play(777);
    let got = one(
        cb(&mut t, 777, PlayCb::ContainerSetSlot, server),
        PlayCb::ContainerSetSlot,
    );
    let mut text = Vec::new();
    for value in ["Hi", "", "", ""] {
        text.put_bytes(&nbt(&line(value)));
    }
    text.put_bool(false);
    text.put_varint(14); // red
    text.put_bool(true);
    // The server's data (no client counterpart left) is backed up with its
    // hash, -476928617 (see `claims_about_26_3_sign_stacks_reach_the_server_
    // with_its_data_hash`).
    assert_eq!(
        got,
        slot(
            1102,
            &[
                (118, text.clone()),
                (120, Vec::new()),
                (0, backup(1016, &[(60, -476_928_617)], &[]).0)
            ],
            &[]
        )
    );

    // An unwaxed sign whose data also holds a field the components do not
    // take keeps that field in its block_entity_data.
    let server = slot(
        1016,
        &[(
            60,
            typed(
                7,
                &c(vec![
                    ("front_text", front),
                    ("is_waxed", Tag::Byte(0)),
                    ("allow_op_features", Tag::Byte(1)),
                ]),
            ),
        )],
        &[],
    );
    let got = one(
        cb(&mut t, 777, PlayCb::ContainerSetSlot, server),
        PlayCb::ContainerSetSlot,
    );
    // The server's whole data hashes to 1520026718 ({id: sign, front_text,
    // is_waxed: 0b, allow_op_features: 1b}, an independent Python
    // transcription of HashOps); the client's rest hashes otherwise.
    let rest = typed(7, &c(vec![("allow_op_features", Tag::Byte(1))]));
    assert_eq!(
        got,
        slot(
            1102,
            &[
                (118, text),
                (62, rest),
                (0, backup(1016, &[(60, 1_520_026_718)], &[]).0)
            ],
            &[]
        )
    );
}

// ---- registry entries sent inline --------------------------------------------

/// A `registry_data` body: registry name, then `(name, inline data)`.
fn registry(name: &str, entries: &[(&str, Option<Tag>)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_str(name);
    b.put_len(entries.len());
    for (entry, data) in entries {
        b.put_str(entry);
        b.put_bool(data.is_some());
        if let Some(data) = data {
            b.put_bytes(&nbt(data));
        }
    }
    b
}

/// Translates one configuration `registry_data`; returns the client's body
/// (or none when the client does not have the registry).
fn registry_to(client: i32, body: Vec<u8>) -> (ViaTranslator, Option<Vec<u8>>) {
    let mut t = config(client);
    let mut out = TranslatorOutput::new();
    t.clientbound(
        RawPacket::new(ConfigCb::RegistryData.id(Proto::V776).unwrap(), body.into()),
        &mut out,
    );
    let got = out.take_to_client().next().map(|p| p.data.to_vec());
    (t, got)
}

#[test]
fn inline_dimension_types_lose_attributes_the_client_lacks() {
    // `visual/ambient_light_color` is new in 26.1; the bed rule's
    // `explodes` is `destroy_on_use` in 26.3.
    let dimension = |light: bool, bed_key: &str| {
        let mut attributes = vec![(
            "minecraft:gameplay/bed_rule",
            c(vec![
                ("can_sleep", s("never")),
                ("can_set_spawn", s("never")),
                (bed_key, Tag::Byte(1)),
            ]),
        )];
        if light {
            attributes.insert(0, ("minecraft:visual/ambient_light_color", s("#0a0a0a")));
        }
        c(vec![
            ("height", Tag::Int(384)),
            ("min_y", Tag::Int(-64)),
            ("attributes", c(attributes)),
        ])
    };
    let server = registry(
        "minecraft:dimension_type",
        &[
            ("minecraft:overworld", None),
            ("elyzune:void", Some(dimension(true, "explodes"))),
        ],
    );
    for (client, expected) in [
        (774, dimension(false, "explodes")),
        (775, dimension(true, "explodes")),
        (777, dimension(true, "destroy_on_use")),
    ] {
        let (_, got) = registry_to(client, server.clone());
        let expected = registry(
            "minecraft:dimension_type",
            &[
                ("minecraft:overworld", None),
                ("elyzune:void", Some(expected)),
            ],
        );
        assert_eq!(got.unwrap(), expected, "{client}");
    }
}

#[test]
fn inline_enchantments_take_the_clients_predicate_and_condition_shapes() {
    let requirement = |entity_type_key: &str, condition_key: &str| {
        c(vec![
            (condition_key, s("minecraft:entity_properties")),
            ("entity", s("this")),
            (
                "predicate",
                c(vec![(entity_type_key, s("#minecraft:undead"))]),
            ),
        ])
    };
    let enchantment = |requirements: Tag| {
        c(vec![
            (
                "description",
                entity_hover("Smite+", "minecraft:sulfur_cube"),
            ),
            (
                "effects",
                c(vec![(
                    "minecraft:damage",
                    l(vec![c(vec![("requirements", requirements)])]),
                )]),
            ),
        ])
    };
    let server = registry(
        "minecraft:enchantment",
        &[(
            "elyzune:smite_plus",
            Some(enchantment(requirement(
                "minecraft:entity_type",
                "condition",
            ))),
        )],
    );
    for client in CLIENTS {
        let (_, got) = registry_to(client, server.clone());
        let requirements = match client {
            // 1.21.11 and 26.1: entity predicates are a record (`type`).
            774 | 775 => requirement("type", "condition"),
            // 26.3: loot conditions dispatch on `type`.
            _ => c(vec![
                ("entity", s("this")),
                (
                    "predicate",
                    c(vec![("minecraft:entity_type", s("#minecraft:undead"))]),
                ),
                ("type", s("minecraft:entity_properties")),
            ]),
        };
        let mut expected_entry = enchantment(requirements);
        expected_entry.insert("description", entity_hover("Smite+", sulfur_cube(client)));
        let expected = registry(
            "minecraft:enchantment",
            &[("elyzune:smite_plus", Some(expected_entry))],
        );
        assert_eq!(got.unwrap(), expected, "{client}");
    }
}

/// An enchantment whose damage effect requires `condition`.
fn requiring(condition: Tag) -> Tag {
    c(vec![(
        "effects",
        c(vec![(
            "minecraft:damage",
            l(vec![c(vec![("requirements", condition)])]),
        )]),
    )])
}

#[test]
fn an_inline_entry_the_client_cannot_read_is_left_out_and_ids_shift() {
    // 26.3 has no `reference` condition (it resolves predicates through a
    // registry the backend never sends): the entry is removed for 26.3
    // only, and the next entry takes its id there.
    let custom = requiring(c(vec![
        ("condition", s("minecraft:reference")),
        ("name", s("elyzune:night")),
    ]));
    let server = registry(
        "minecraft:enchantment",
        &[("elyzune:odd", Some(custom)), ("minecraft:sharpness", None)],
    );
    let (t, got) = registry_to(777, server.clone());
    assert_eq!(
        got.unwrap(),
        registry("minecraft:enchantment", &[("minecraft:sharpness", None)])
    );
    let reg = t
        .connection_state()
        .dynamic("minecraft:enchantment")
        .unwrap();
    assert_eq!(reg.s2c(0), None);
    assert_eq!(reg.s2c(1), Some(0));
    // Every other client keeps it.
    for client in [774, 775] {
        let (t, _) = registry_to(client, server.clone());
        let reg = t
            .connection_state()
            .dynamic("minecraft:enchantment")
            .unwrap();
        assert_eq!((reg.s2c(0), reg.s2c(1)), (Some(0), Some(1)), "{client}");
    }
}

/// 26.2 `value_check` tests `value.getInt` (Java's `Math.round` of the
/// float) against an int range: for 26.3 it is `int_value_check` with the
/// same constants, a point or a line. A computed provider has no faithful
/// 26.3 form and the entry is left out, as before.
#[test]
fn constant_value_checks_reach_26_3_as_int_value_checks() {
    let check = |value: Tag, range: Tag| {
        requiring(c(vec![
            ("condition", s("minecraft:value_check")),
            ("value", value),
            ("range", range),
        ]))
    };
    let server = registry(
        "minecraft:enchantment",
        &[
            ("elyzune:point", Some(check(Tag::Float(2.5), Tag::Int(3)))),
            (
                "elyzune:line",
                Some(check(
                    c(vec![
                        ("type", s("minecraft:constant")),
                        ("value", Tag::Float(-2.5)),
                    ]),
                    c(vec![("min", Tag::Float(-2.4)), ("max", Tag::Int(7))]),
                )),
            ),
            (
                "elyzune:scored",
                Some(check(
                    c(vec![
                        ("type", s("minecraft:score")),
                        ("target", c(vec![("type", s("minecraft:context"))])),
                        ("score", s("kills")),
                    ]),
                    Tag::Int(1),
                )),
            ),
        ],
    );
    let (_, got) = registry_to(777, server.clone());
    let expected = |value: i32, test: Tag| {
        requiring(c(vec![
            ("value", Tag::Int(value)),
            ("test", test),
            ("type", s("minecraft:int_value_check")),
        ]))
    };
    assert_eq!(
        got.unwrap(),
        registry(
            "minecraft:enchantment",
            &[
                // Math.round(2.5f) = 3, Math.round(-2.5f) = -2,
                // Math.round(-2.4f) = -2.
                ("elyzune:point", Some(expected(3, Tag::Int(3)))),
                (
                    "elyzune:line",
                    Some(expected(
                        -2,
                        c(vec![("min", Tag::Int(-2)), ("max", Tag::Int(7))]),
                    )),
                ),
            ],
        )
    );
    // The other releases keep 26.2's condition as it is.
    for client in [774, 775] {
        let (_, got) = registry_to(client, server.clone());
        assert_eq!(got.unwrap(), server, "{client}");
    }
}

#[test]
fn inline_variants_and_trims_take_the_clients_shapes() {
    // 1.21.11: `<variant>_<entity>` asset ids, no baby assets, flat sounds.
    let (_, got) = registry_to(
        774,
        registry(
            "minecraft:frog_variant",
            &[(
                "elyzune:moss",
                Some(c(vec![
                    ("asset_id", s("minecraft:entity/frog/frog_moss")),
                    ("baby_asset_id", s("minecraft:entity/frog/frog_moss_baby")),
                ])),
            )],
        ),
    );
    assert_eq!(
        got.unwrap(),
        registry(
            "minecraft:frog_variant",
            &[(
                "elyzune:moss",
                Some(c(vec![("asset_id", s("minecraft:entity/frog/moss_frog"))]))
            )],
        )
    );
    let (_, got) = registry_to(
        774,
        registry(
            "minecraft:wolf_sound_variant",
            &[(
                "elyzune:quiet",
                Some(c(vec![
                    (
                        "adult_sounds",
                        c(vec![
                            ("ambient_sound", s("minecraft:entity.wolf.ambient")),
                            ("step_sound", s("minecraft:entity.wolf.step")),
                        ]),
                    ),
                    (
                        "baby_sounds",
                        c(vec![(
                            "ambient_sound",
                            s("minecraft:entity.baby_wolf.ambient"),
                        )]),
                    ),
                ])),
            )],
        ),
    );
    assert_eq!(
        got.unwrap(),
        registry(
            "minecraft:wolf_sound_variant",
            &[(
                "elyzune:quiet",
                Some(c(vec![(
                    "ambient_sound",
                    s("minecraft:entity.wolf.ambient")
                )]))
            )],
        )
    );
    // 26.3: a palette id instead of the asset group.
    let (_, got) = registry_to(
        777,
        registry(
            "minecraft:trim_material",
            &[(
                "elyzune:mithril",
                Some(c(vec![
                    ("asset_name", s("mithril")),
                    ("description", c(vec![("translate", s("trim.mithril"))])),
                ])),
            )],
        ),
    );
    assert_eq!(
        got.unwrap(),
        registry(
            "minecraft:trim_material",
            &[(
                "elyzune:mithril",
                Some(c(vec![
                    ("description", c(vec![("translate", s("trim.mithril"))])),
                    ("palette_id", s("minecraft:trim/mithril")),
                ])),
            )],
        )
    );
}

// ---- debug subscriptions -------------------------------------------------------

/// `game_event` ids (registries.json): 26.2 inserted `bounce` at 9, so
/// 26.2's 10 (`minecraft:container_close`) is 9 in 1.21.11 and 26.1; 26.3
/// keeps 26.2's numbers. Subscription 15 is `game_events` everywhere.
#[test]
fn debug_values_renumber_their_registry_ids() {
    let event = |game_event: i32| {
        let mut b = Vec::new();
        b.put_varint(15);
        b.put_varint(game_event);
        b.put_f64(1.0);
        b.put_f64(2.0);
        b.put_f64(3.0);
        b
    };
    for (client, mapped) in [(774, 9), (775, 9), (777, 10)] {
        let mut t = play(client);
        let got = one(
            cb(&mut t, client, PlayCb::DebugEvent, event(10)),
            PlayCb::DebugEvent,
        );
        assert_eq!(got, event(mapped), "{client}");
        // `bounce` has no 26.1 equivalent: the event is not sent.
        let got = cb(&mut t, client, PlayCb::DebugEvent, event(9));
        if client == 777 {
            assert_eq!(one(got, PlayCb::DebugEvent), event(9));
        } else {
            assert!(got.is_empty(), "{client}");
        }
        // An update that clears a value (absent) and the tick-time sample
        // pass unchanged.
        let mut clear = Vec::new();
        clear.put_i64(42);
        clear.put_varint(8);
        clear.put_bool(false);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::DebugBlockValue, clear.clone()),
                PlayCb::DebugBlockValue
            ),
            clear
        );
        let mut sample = Vec::new();
        sample.put_varint(2);
        sample.put_i64(5);
        sample.put_i64(6);
        sample.put_varint(0);
        assert_eq!(
            one(
                cb(&mut t, client, PlayCb::DebugSample, sample.clone()),
                PlayCb::DebugSample
            ),
            sample
        );
    }
}

// ---- hashed stacks in container clicks -------------------------------------------

/// `container_click` claiming slot 36 holds `item` with `components`
/// (type, hash), cursor empty: container 0, state 1, slot 36, button 0,
/// mode 0 (pickup).
fn click(item: i32, components: &[(i32, i32)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_varint(0);
    b.put_varint(1);
    b.put_i16(36);
    b.put_i8(0);
    b.put_varint(0);
    b.put_varint(1);
    b.put_i16(36);
    b.put_bool(true);
    b.put_varint(item);
    b.put_varint(1);
    b.put_varint(i32::try_from(components.len()).unwrap());
    for (ty, hash) in components {
        b.put_varint(*ty);
        b.put_i32(*hash);
    }
    b.put_varint(0);
    b.put_bool(false);
    b
}

/// Sends `slot` (a 776 container_set_slot body) to the client, then the
/// client's `claim`; returns what the backend receives.
fn claim_after(client: i32, slot_776: Vec<u8>, claim: Vec<u8>) -> Vec<u8> {
    let mut t = play(client);
    cb(&mut t, client, PlayCb::ContainerSetSlot, slot_776);
    let got = sb(&mut t, client, PlaySb::ContainerClick, claim);
    assert_eq!(got.len(), 1, "{client}");
    got.into_iter().next().unwrap().1
}

/// Hashes are the 26.2 jar's `HashOps` (CRC32C), computed by an
/// independent implementation (a Python transcription of
/// `net.minecraft.util.HashOps`) for these values:
/// `{id: "minecraft:stone"}` = 725422587; the four pot sides
/// `[angler_pottery_sherd, brick, brick, brick]` = 570053514.
#[test]
fn claims_about_translated_stacks_reach_the_server_as_its_own_stacks() {
    let name = nbt(&s("Brick"));
    let custom_name_hash = 1_894_353_999; // any: same codec form, kept

    // 1.21.11 holds cinnabar bricks (26.2: 48) as cut red sandstone (571)
    // with a backup naming the cinnabar: the claim, which carries the
    // backup's hash under custom_data (0), must name the server's item
    // again. Without that hash it is a claim about real cut red sandstone.
    let (_, cinnabar) = backup(48, &[], &[]);
    let got = claim_after(
        774,
        slot(48, &[(6, name.clone())], &[]),
        click(571, &[(6, custom_name_hash), (0, cinnabar)]),
    );
    assert_eq!(got, click(48, &[(6, custom_name_hash)]));
    let got = claim_after(
        774,
        slot(48, &[(6, name.clone())], &[]),
        click(571, &[(6, custom_name_hash)]),
    );
    assert_eq!(got, click(599, &[(6, custom_name_hash)]));

    // 26.1 has no sulfur_cube_content (26.2: 78): the server's component
    // and its hash come back. paper: 1029 (26.1) / 1057 (26.2).
    let mut sulfur = Vec::new();
    sulfur.put_varint(1); // stone
    sulfur.put_varint(1);
    sulfur.put_varint(0);
    sulfur.put_varint(0);
    let (_, key) = backup(1057, &[(78, 725_422_587)], &[]);
    let got = claim_after(
        775,
        slot(1057, &[(78, sulfur)], &[]),
        click(1029, &[(0, key)]),
    );
    assert_eq!(got, click(1057, &[(78, 725_422_587)]));

    // 26.3 pot decorations (26.3: 76) hash another codec form; the server's
    // (26.2: 74) is computed. decorated_pot: 392 (26.3) / 347 (26.2);
    // angler_pottery_sherd 1477, brick 1054 (26.2).
    let mut pot = Vec::new();
    pot.put_varint(4);
    for item in [1477, 1054, 1054, 1054] {
        pot.put_varint(item);
    }
    let (_, key) = backup(347, &[(74, 570_053_514)], &[]);
    let got = claim_after(
        777,
        slot(347, &[(74, pot)], &[]),
        click(392, &[(76, 12345), (0, key)]),
    );
    assert_eq!(got, click(347, &[(74, 570_053_514)]));

    // 26.3 attack + interact animations (40, 41) are one 26.2 swing
    // animation (40) with the same hash. diamond_sword 1050 / 964.
    let got = claim_after(
        777,
        slot(964, &[(40, swing())], &[]),
        click(1050, &[(40, 777), (41, 777)]),
    );
    assert_eq!(got, click(964, &[(40, 777)]));
}

/// A 26.3 sign stack carries its text as components where the server's
/// carries block_entity_data (`sign_items_for_26_3_carry_their_text_as_
/// components`): a claim about it comes back as the server's stack with the
/// data's hash computed. `{id: "minecraft:sign", front_text: {messages:
/// [{text: "Hi"}, {text: ""} x3], color: "red", has_glowing_text: 1b},
/// is_waxed: 1b}` hashes to -476928617 by NbtOps types (an independent
/// Python transcription of `HashOps`, checked against the jar's vector in
/// `hashed::tests::nbt_matches_the_jar_vector`).
#[test]
fn claims_about_26_3_sign_stacks_reach_the_server_with_its_data_hash() {
    let line = |text: &str| c(vec![("text", s(text))]);
    let data = c(vec![
        (
            "front_text",
            c(vec![
                (
                    "messages",
                    l(vec![line("Hi"), line(""), line(""), line("")]),
                ),
                ("color", s("red")),
                ("has_glowing_text", Tag::Byte(1)),
            ]),
        ),
        ("is_waxed", Tag::Byte(1)),
    ]);
    let (_, key) = backup(1016, &[(60, -476_928_617)], &[]);
    let got = claim_after(
        777,
        slot(1016, &[(60, typed(7, &data))], &[]),
        click(1102, &[(118, 4242), (120, -982_207_288), (0, key)]),
    );
    assert_eq!(got, click(1016, &[(60, -476_928_617)]));
}

#[test]
fn claims_without_a_record_are_renumbered_as_before() {
    // A stack the translation did not change has no record: ids are
    // renumbered, hashes kept (paper 1145 -> 1057 for 26.3, custom_name 6).
    let mut t = play(777);
    let got = sb(&mut t, 777, PlaySb::ContainerClick, click(1145, &[(6, 99)]));
    assert_eq!(got[0].1, click(1057, &[(6, 99)]));
    // 26.3 animations without a record still reach 26.2 as its swing.
    let got = sb(
        &mut t,
        777,
        PlaySb::ContainerClick,
        click(1050, &[(40, 5), (41, 5)]),
    );
    assert_eq!(got[0].1, click(964, &[(40, 5)]));
}

// ---- items only 26.3 has, serverbound --------------------------------------------

/// A 26.3 creative slot of an item 26.2 lacks reaches the backend as the
/// closest 26.2 item and is reported, never as air. Ids (registries.json):
/// poplar_planks 72 -> birch_planks 65; red_concrete_slab 735 ->
/// red_concrete 656; woodland_mansion_map 1240 -> filled_map 1133.
#[test]
fn items_only_26_3_has_reach_26_2_as_their_closest_item() {
    for (item, expected) in [(72, 65), (735, 656), (1240, 1133)] {
        let mut t = play(777);
        let mut body = Vec::new();
        body.put_i16(36);
        body.put_varint(3);
        body.put_varint(item);
        body.put_varint(0);
        body.put_varint(0);
        let got = sb(&mut t, 777, PlaySb::SetCreativeModeSlot, body);
        let mut want = Vec::new();
        want.put_i16(36);
        want.put_varint(3);
        want.put_varint(expected);
        want.put_varint(0);
        want.put_varint(0);
        assert_eq!(got, vec![(PlaySb::SetCreativeModeSlot, want)], "{item}");
        assert!(t.reported(
            "minecraft:set_creative_mode_slot",
            "an item only the client's release has reached 26.2 as its closest item"
        ));
    }
}

// ---- JSON text ---------------------------------------------------------------

/// The login disconnect reason is JSON text; its hovers are rewritten too
/// and its booleans stay booleans.
#[test]
fn login_disconnect_json_text_is_rewritten() {
    let reason = |entity: &str| {
        format!(
            r#"{{"bold":true,"hover_event":{{"action":"show_entity","id":"{entity}","uuid":[0,0,0,1]}},"text":"Banned"}}"#
        )
    };
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
        let mut body = Vec::new();
        body.put_str(&reason("minecraft:sulfur_cube"));
        t.clientbound(
            RawPacket::new(
                LoginCb::LoginDisconnect.id(Proto::V776).unwrap(),
                body.into(),
            ),
            &mut out,
        );
        let got: Vec<RawPacket> = out.take_to_client().collect();
        let mut expected = Vec::new();
        expected.put_str(&reason(sulfur_cube(client)));
        assert_eq!(got[0].data.as_ref(), expected.as_slice(), "{client}");
    }
}
