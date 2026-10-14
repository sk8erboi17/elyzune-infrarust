//! Every vanilla registry entry, converted from 26.2 to each client release
//! as if the backend had sent it inline, compared with that release's own
//! data generator output.
//!
//! The data comes from the via-data pipeline (`tools/via_data.py`, Mojang's
//! jars run with `--reports`/`--server`); it is not committed, so the test
//! looks for it in `ELYZUNE_VIA_DATA`, then `../../target/via-data` and the
//! main Infrarust checkout next to a worktree, and reports a skip when none
//! exists.
//!
//! A difference left after conversion must be in [`ACCEPTED`]: a value
//! vanilla itself changed between the releases (the client then simply gets
//! the backend's value), or a record field the client's codec ignores. Any
//! other difference fails, printed path by path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use bytes::Bytes;
use elyzune_via::generated::Proto;
use elyzune_via::generated::dynamic::SYNCED_776;
use elyzune_via::generated::packets::{ConfigCb, LoginCb, LoginSb};
use elyzune_via::layout::{Dir, Xc};
use elyzune_via::nbt::Tag;
use elyzune_via::session::ViaTranslator;
use elyzune_via::wire::Put;
use infrarust_api::filter::{ProtocolTranslator, TranslatorOutput};
use infrarust_api::types::RawPacket;
use serde_json::Value;

const REGISTRIES: &[&str] = &[
    "dimension_type",
    "worldgen/biome",
    "chat_type",
    "trim_pattern",
    "trim_material",
    "wolf_variant",
    "wolf_sound_variant",
    "pig_variant",
    "pig_sound_variant",
    "frog_variant",
    "cat_variant",
    "cat_sound_variant",
    "cow_sound_variant",
    "cow_variant",
    "chicken_sound_variant",
    "chicken_variant",
    "zombie_nautilus_variant",
    "painting_variant",
    "damage_type",
    "banner_pattern",
    "enchantment",
    "jukebox_song",
    "instrument",
    "test_environment",
    "test_instance",
    "dialog",
    "world_clock",
    "timeline",
];

/// Differences that are not translation defects, as `(client, registry,
/// path pattern, why)`. A pattern matches a diff path when every `*`
/// segment-wildcard and literal part lines up (see [`matches`]).
const ACCEPTED: &[(i32, &str, &str, &str)] = &[
    // Record fields 1.21.11 does not know and its codec skips.
    (
        774,
        "dimension_type",
        "-/default_clock",
        "1.21.11 has no world clocks; field ignored",
    ),
    (
        774,
        "dimension_type",
        "-/has_ender_dragon_fight",
        "field added in 26.1; ignored by 1.21.11",
    ),
    (
        774,
        "timeline",
        "-/clock",
        "field added in 26.1; ignored by 1.21.11",
    ),
    (
        774,
        "timeline",
        "-/time_markers",
        "field added in 26.1; ignored by 1.21.11",
    ),
    // Vanilla retuned the night fog in 26.2: the client gets 26.2's colour.
    (
        774,
        "timeline",
        "~/tracks/minecraft:visual/fog_color/keyframes/*/value",
        "vanilla value change",
    ),
    (
        775,
        "timeline",
        "~/tracks/minecraft:visual/fog_color/keyframes/*/value",
        "vanilla value change",
    ),
    (
        777,
        "timeline",
        "~/tracks/minecraft:visual/fog_color/keyframes/*/value",
        "vanilla value change",
    ),
    // 26.3 writes ARGB colours as hex; its codec still reads 26.2's int.
    (
        777,
        "timeline",
        "~/tracks/minecraft:visual/cloud_color/keyframes/*/value",
        "same colour, int form (read by 26.3's codec)",
    ),
    // 26.3 attributes with their defaults, absent in 26.2's data.
    (
        777,
        "dimension_type",
        "+/attributes/minecraft:gameplay/straw_bed_rule",
        "26.3-only attribute, default",
    ),
    // 26.3 made the nether/crimson particles append to the dimension's
    // (a vanilla behaviour change); 26.2's plain list is still a valid value.
    (
        777,
        "worldgen/biome",
        "~/attributes/minecraft:visual/ambient_particles",
        "vanilla behaviour change, value still valid",
    ),
    // Vanilla changed the end sky colour in 26.1.
    (
        774,
        "dimension_type",
        "~/attributes/minecraft:visual/sky_light_color",
        "vanilla value change",
    ),
    // Vanilla added a term to lunge's requirement in 26.1.
    (
        774,
        "enchantment",
        "~/effects/minecraft:post_piercing_attack/0/requirements/terms",
        "vanilla data change",
    ),
    // Vanilla 26.3 simplified fire protection's `all_of` of one condition
    // into that condition: the 26.2 form, converted, means the same.
    (
        777,
        "enchantment",
        "-/effects/minecraft:damage_protection/0/requirements/terms",
        "vanilla data change",
    ),
    (
        777,
        "enchantment",
        "~/effects/minecraft:damage_protection/0/requirements/type",
        "vanilla data change",
    ),
    (
        777,
        "enchantment",
        "+/effects/minecraft:damage_protection/0/requirements/predicate",
        "vanilla data change",
    ),
];

/// 26.3 biome attributes that are gameplay-only (mob spawning moved into
/// attributes): the network codec filters them out, so they are removed from
/// the expected data before comparing.
const SERVER_ONLY_ATTRIBUTES: &[&str] = &[
    "minecraft:gameplay/natural_mob_spawns",
    "minecraft:gameplay/creature_world_gen_spawn_probability",
];

/// Keys of the data generator output that the network codec never sends,
/// stripped before comparing.
const NOT_SENT: &[(&str, &str)] = &[
    ("worldgen/biome", "spawners"),
    ("worldgen/biome", "spawn_costs"),
    ("worldgen/biome", "carvers"),
    ("worldgen/biome", "features"),
    ("worldgen/biome", "creature_spawn_probability"),
];

fn via_data() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ELYZUNE_VIA_DATA") {
        return Some(PathBuf::from(path));
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    [
        here.join("../../target/via-data"),
        here.join("../../../../infrarust/target/via-data"),
    ]
    .into_iter()
    .find(|p| p.join("gen/26.2/generated").is_dir())
}

fn release(p: i32) -> &'static str {
    match p {
        774 => "1.21.11",
        775 => "26.1.2",
        776 => "26.2",
        _ => "26.3",
    }
}

fn to_tag(v: &Value) -> Tag {
    match v {
        Value::Null => Tag::Byte(0),
        Value::Bool(b) => Tag::Byte(i8::from(*b)),
        Value::Number(n) => match n.as_i64().and_then(|i| i32::try_from(i).ok()) {
            Some(i) => Tag::Int(i),
            None => Tag::Double(n.as_f64().unwrap()),
        },
        Value::String(s) => Tag::string(s),
        Value::Array(items) => {
            let items: Vec<Tag> = items.iter().map(to_tag).collect();
            let ty = items.first().map_or(0, Tag::type_id);
            Tag::List(ty, items)
        }
        Value::Object(map) => Tag::Compound(
            map.iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), to_tag(v)))
                .collect(),
        ),
    }
}

/// Back to JSON with every number as f64 (and booleans as 0/1), so values
/// compare whatever numeric tag carried them.
fn to_json(t: &Tag) -> Value {
    let num = |f: f64| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number);
    match t {
        Tag::Byte(v) => num(f64::from(*v)),
        Tag::Short(v) => num(f64::from(*v)),
        Tag::Int(v) => num(f64::from(*v)),
        Tag::Long(v) => num(*v as f64),
        Tag::Float(v) => num(f64::from(*v)),
        Tag::Double(v) => num(*v),
        Tag::String(_) => Value::String(t.as_str().unwrap().to_owned()),
        Tag::List(_, items) => Value::Array(items.iter().map(to_json).collect()),
        Tag::Compound(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), to_json(v)))
                .collect(),
        ),
        Tag::ByteArray(v) => Value::Array(v.iter().map(|b| num(f64::from(*b))).collect()),
        Tag::IntArray(v) => Value::Array(v.iter().map(|b| num(f64::from(*b))).collect()),
        Tag::LongArray(v) => Value::Array(v.iter().map(|b| num(*b as f64)).collect()),
    }
}

fn normalized(v: &Value) -> Value {
    to_json(&to_tag(v))
}

fn diff(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
                let p = format!("{path}/{k}");
                match (x.get(k), y.get(k)) {
                    (Some(_), None) => out.push(format!("-{p}")),
                    (None, Some(_)) => out.push(format!("+{p}")),
                    (Some(x), Some(y)) => diff(x, y, &p, out),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (x, y)) in x.iter().zip(y).enumerate() {
                diff(x, y, &format!("{path}/{i}"), out);
            }
        }
        _ if a != b => out.push(format!("~{path}")),
        _ => {}
    }
}

/// Whether `path` (`~/a/0/b`) matches `pattern` (`~/a/*/b`): same segment
/// count, `*` matching any one segment, or a prefix match when the pattern's
/// last segment is the path's ancestor.
fn matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let q: Vec<&str> = path.split('/').collect();
    q.len() >= p.len() && p.iter().zip(&q).all(|(a, b)| *a == "*" || a == b)
}

/// A translator in configuration with every 26.2 registry sent by name.
fn configured(client: i32) -> ViaTranslator {
    let p = Proto::from_number(client).unwrap();
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

#[test]
fn every_vanilla_entry_converts_to_the_clients_own_data() {
    let Some(data) = via_data() else {
        eprintln!("skipped: no via-data (set ELYZUNE_VIA_DATA to the via_data.py output)");
        return;
    };
    let mut failures = Vec::new();
    let mut compared = 0usize;
    for client in [774, 775, 777] {
        let t = configured(client);
        for registry in REGISTRIES {
            let server_dir = data.join(format!("gen/26.2/generated/data/minecraft/{registry}"));
            let client_dir = data.join(format!(
                "gen/{}/generated/data/minecraft/{registry}",
                release(client)
            ));
            if !server_dir.is_dir() || !client_dir.is_dir() {
                continue;
            }
            let mut files: Vec<PathBuf> = walk(&server_dir);
            files.sort();
            for file in files {
                let rel = file.strip_prefix(&server_dir).unwrap();
                let Ok(client_text) = std::fs::read_to_string(client_dir.join(rel)) else {
                    continue;
                };
                let mut server: Value =
                    serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
                let mut expected: Value = serde_json::from_str(&client_text).unwrap();
                for (r, key) in NOT_SENT {
                    if r == registry {
                        server.as_object_mut().unwrap().remove(*key);
                        expected.as_object_mut().unwrap().remove(*key);
                    }
                }
                if let Some(attributes) = expected
                    .get_mut("attributes")
                    .and_then(Value::as_object_mut)
                {
                    for key in SERVER_ONLY_ATTRIBUTES {
                        attributes.remove(*key);
                    }
                    if attributes.is_empty() {
                        expected.as_object_mut().unwrap().remove("attributes");
                    }
                }
                let mut tag = to_tag(&server);
                let mut x = Xc::new(&t, Dir::S2c);
                let kept = elyzune_via::registry_data::entry(
                    &mut x,
                    &format!("minecraft:{registry}"),
                    &mut tag,
                );
                compared += 1;
                if !kept {
                    failures.push(format!(
                        "{client} {registry}/{}: entry rejected",
                        rel.display()
                    ));
                    continue;
                }
                let mut diffs = Vec::new();
                diff(&to_json(&tag), &normalized(&expected), "", &mut diffs);
                for d in diffs {
                    let accepted = ACCEPTED.iter().any(|(c, r, pattern, _)| {
                        *c == client && r == registry && matches(pattern, &d)
                    });
                    if !accepted {
                        failures.push(format!("{client} {registry}/{}: {d}", rel.display()));
                    }
                }
            }
        }
    }
    assert!(compared > 1000, "only {compared} entries compared");
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    out
}
