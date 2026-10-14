//! Registry entries sent inline in `registry_data`.
//!
//! A backend sends an entry's contents only when the client cannot take it
//! from its own copy of the vanilla data: Hyperion and vanilla 26.2 send
//! every vanilla entry by name (`config.rs`), so inline contents are
//! custom entries (a datapack dimension type, chat type, enchantment...).
//! The client decodes them with its own release's network codec, which is
//! strict about environment attribute names, loot condition types and the
//! shape of a few values: one entry it cannot decode fails the whole
//! configuration. Each entry is therefore rewritten to the client's shape
//! here, and an entry that still uses something the client cannot read is
//! left out of the client's registry (logged), exactly like a vanilla entry
//! the client does not ship.
//!
//! # What differs (26.2 -> client)
//!
//! Found by converting every vanilla entry of 26.2 and comparing with the
//! client release's own data generator output (`tests/registry_entries.rs`
//! does that for all of them):
//!
//! ```text
//! all          text (descriptions, titles, chat styles): text.rs
//! 1.21.11      environment attributes added in 26.1 removed (dimension
//!              types, timeline tracks); wolf sounds were flat, without
//!              baby sounds; cat/frog/chicken/cow/pig asset ids were named
//!              <variant>_<entity> (cat: <variant>) and had no baby asset
//! 1.21.11, 26.1  entity predicates were a record: `type`, `flags`,
//!              `type_specific: {type}`... instead of 26.2's map of
//!              namespaced sub-predicates
//! 26.3         loot conditions dispatch on `type` (was `condition`);
//!              `block_state_property` became `match_block`; damage source
//!              tag tests name the tag with `#`; block state providers and
//!              block states were renamed (`simple_state_provider` with
//!              `{Name, Properties}` -> `{id, properties}`); bed rules
//!              `explodes` -> `destroy_on_use`; trim materials have a
//!              palette id instead of an asset group
//! ```

use crate::generated::Proto;
use crate::generated::registries::static_names;
use crate::layout::Xc;
use crate::nbt::Tag;

/// Rewrites one inline entry of `registry` for the client. False when the
/// entry uses something the client's release cannot represent; the caller
/// then removes it from the client's registry.
pub fn entry(x: &mut Xc<'_>, registry: &str, tag: &mut Tag) -> bool {
    let client = x.t.client;
    let kept = match registry {
        "minecraft:dimension_type" => {
            if let Some(attributes) = tag.get_mut("attributes") {
                attribute_map(x, attributes);
            }
            true
        }
        "minecraft:worldgen/biome" => {
            if let Some(attributes) = tag.get_mut("attributes") {
                attribute_map(x, attributes);
            }
            true
        }
        "minecraft:timeline" => {
            if let Some(tracks) = tag.get_mut("tracks") {
                attribute_map(x, tracks);
            }
            true
        }
        "minecraft:trim_material" => {
            crate::codec::trim_material_shape(x, tag);
            true
        }
        "minecraft:wolf_variant" if client == Proto::V774 => {
            tag.remove("baby_assets");
            true
        }
        "minecraft:wolf_sound_variant" if client == Proto::V774 => {
            flatten_wolf_sounds(tag);
            true
        }
        "minecraft:cat_variant" if client == Proto::V774 => {
            rename_asset(tag, |entity, last| {
                last.strip_prefix(&format!("{entity}_")).map(str::to_owned)
            });
            true
        }
        "minecraft:frog_variant"
        | "minecraft:chicken_variant"
        | "minecraft:cow_variant"
        | "minecraft:pig_variant"
            if client == Proto::V774 =>
        {
            rename_asset(tag, |entity, last| {
                last.strip_prefix(&format!("{entity}_"))
                    .map(|variant| format!("{variant}_{entity}"))
            });
            true
        }
        "minecraft:jukebox_song" | "minecraft:instrument" => {
            if let Some(sound) = tag.get_mut("sound_event") {
                crate::codec::sound_event(x, sound);
            }
            true
        }
        "minecraft:enchantment" => enchantment(x, tag),
        _ => true,
    };
    if kept {
        crate::text::walk(x, tag);
    }
    kept
}

/// Removes, from a map keyed by environment attribute (a dimension type's or
/// biome's `attributes`, a timeline's `tracks`), the attributes the client's
/// release does not have, and reshapes the values 26.3 changed.
fn attribute_map(x: &mut Xc<'_>, tag: &mut Tag) {
    let client = x.t.client;
    let known = static_names(
        "minecraft:environment_attribute",
        client.number().unsigned_abs(),
    );
    let Tag::Compound(entries) = tag else {
        return;
    };
    let before = entries.len();
    entries.retain(|(key, _)| {
        let Ok(key) = std::str::from_utf8(key) else {
            return true;
        };
        let key = crate::codec::qualified(key);
        known.is_none_or(|names| names.binary_search(&&*key).is_ok())
    });
    if entries.len() != before {
        x.notes
            .push("an environment attribute the client lacks was removed from a registry entry");
    }
    if client == Proto::V777 {
        for (key, value) in entries.iter_mut() {
            if key.as_slice() == b"minecraft:gameplay/bed_rule" {
                bed_rule_777(value);
            }
        }
    }
}

/// 26.3 renamed a bed rule's `explodes` to `destroy_on_use`. The value is
/// either the rule itself or a modifier `{argument: rule, modifier}`.
fn bed_rule_777(tag: &mut Tag) {
    let rule = if tag.get("argument").is_some() {
        tag.get_mut("argument")
    } else {
        Some(tag)
    };
    if let Some(rule) = rule
        && let Some(explodes) = rule.remove("explodes")
    {
        rule.insert("destroy_on_use", explodes);
    }
}

/// 1.21.11 wolf sound variants are flat; 26.1 nests the adult sounds and
/// adds baby sounds and a step sound.
fn flatten_wolf_sounds(tag: &mut Tag) {
    tag.remove("baby_sounds");
    if let Some(Tag::Compound(adult)) = tag.remove("adult_sounds") {
        for (key, value) in adult {
            if key.as_slice() != b"step_sound" {
                tag.insert(&String::from_utf8_lossy(&key), value);
            }
        }
    }
}

/// Renames the last path segment of `asset_id` (`ns:entity/<entity>/<last>`)
/// with `rename(entity, last)` and removes `baby_asset_id`, which 1.21.11
/// variants do not have. Asset ids of other shapes (a resource pack's own)
/// are left as they are.
fn rename_asset(tag: &mut Tag, rename: impl Fn(&str, &str) -> Option<String>) {
    tag.remove("baby_asset_id");
    let Some(asset) = tag.get("asset_id").and_then(Tag::as_str).map(str::to_owned) else {
        return;
    };
    let Some((namespace, path)) = asset.split_once(':') else {
        return;
    };
    let mut parts: Vec<&str> = path.split('/').collect();
    let (Some(&"entity"), Some(&entity), Some(&last)) = (parts.first(), parts.get(1), parts.last())
    else {
        return;
    };
    if parts.len() != 3 {
        return;
    }
    if let Some(renamed) = rename(entity, last) {
        let renamed: &str = &renamed;
        if let Some(slot) = parts.last_mut() {
            *slot = renamed;
        }
        tag.insert(
            "asset_id",
            Tag::string(&format!("{namespace}:{}", parts.join("/"))),
        );
    }
}

// ---- enchantments ------------------------------------------------------------

/// An enchantment's `effects` hold loot conditions (`requirements`), entity
/// predicates inside them, block state providers and attribute modifiers.
/// False when it uses a construct that has no form in the client's release.
fn enchantment(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    let client = x.t.client;
    let mut ok = true;
    let Some(Tag::Compound(effects)) = tag.get_mut("effects") else {
        return true;
    };
    for (_, list) in effects.iter_mut() {
        // An effect component is a list of conditional effects, or (for a
        // few unconditional components) a single value.
        let items: Vec<&mut Tag> = match list {
            Tag::List(_, items) => items.iter_mut().collect(),
            other => vec![other],
        };
        for effect in items {
            if let Some(requirements) = effect.get_mut("requirements") {
                ok &= condition(x, requirements);
            }
            if let Some(inner) = effect.get_mut("effect") {
                effect_body(x, inner);
            }
            // `minecraft:attributes` entries are the effect itself.
            attribute_effect(x, effect);
            if client == Proto::V777 {
                block_states_777(effect);
            }
        }
    }
    if !ok {
        x.notes
            .push("a custom enchantment uses a loot condition the client release lacks; left out");
    }
    ok
}

/// The `effect` of a conditional enchantment effect: entity effects may nest
/// further effects (`all_of`), and replace-block effects carry block states.
fn effect_body(x: &mut Xc<'_>, tag: &mut Tag) {
    if let Some(Tag::List(_, effects)) = tag.get_mut("effects") {
        for effect in effects {
            effect_body(x, effect);
        }
    }
    attribute_effect(x, tag);
    if x.t.client == Proto::V777 {
        block_states_777(tag);
    }
}

/// An attribute effect (`{attribute, id, amount, operation}`) naming an
/// attribute the client lacks keeps working with the attribute removed:
/// the attribute name is replaced by one every release has, so the entry
/// stays decodable. Its effect is only computed by the server.
fn attribute_effect(x: &mut Xc<'_>, tag: &mut Tag) {
    let Some(attribute) = tag
        .get("attribute")
        .and_then(Tag::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    if crate::codec::map_name(x, crate::generated::registries::Reg::Attribute, &attribute).is_none()
    {
        tag.insert("attribute", Tag::string("minecraft:luck"));
        x.notes
            .push("an enchantment attribute the client lacks is shown as luck (server-side only)");
    }
}

/// A loot condition (`requirements` and its nested terms), rewritten for the
/// client. False when the client release has no such condition type.
fn condition(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    let client = x.t.client;
    let kind = tag
        .get("condition")
        .and_then(Tag::as_str)
        .map(|k| crate::codec::qualified(k).into_owned());
    let Some(kind) = kind else {
        return true;
    };
    let mut ok = true;
    match kind.as_str() {
        "minecraft:all_of" | "minecraft:any_of" => {
            if let Some(Tag::List(_, terms)) = tag.get_mut("terms") {
                for term in terms {
                    ok &= condition(x, term);
                }
            }
        }
        "minecraft:inverted" => {
            if let Some(term) = tag.get_mut("term") {
                ok &= condition(x, term);
            }
        }
        "minecraft:entity_properties" => {
            if let Some(predicate) = tag.get_mut("predicate") {
                entity_predicate(x, predicate);
            }
        }
        "minecraft:damage_source_properties" => {
            if let Some(predicate) = tag.get_mut("predicate") {
                for key in ["source_entity", "direct_entity"] {
                    if let Some(entity) = predicate.get_mut(key) {
                        entity_predicate(x, entity);
                    }
                }
                if client == Proto::V777
                    && let Some(Tag::List(_, tests)) = predicate.get_mut("tags")
                {
                    for test in tests {
                        if let Some(id) = test.get("id").and_then(Tag::as_str).map(str::to_owned)
                            && !id.starts_with('#')
                        {
                            test.insert("id", Tag::string(&format!("#{id}")));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    if client == Proto::V777 {
        ok &= condition_777(tag, &kind);
    }
    ok
}

/// 26.3 dispatches loot conditions on `type`, renamed `block_state_property`
/// to `match_block` (`{block, properties}` -> `{blocks, state}`), and
/// replaced `value_check` and `reference` with conditions of another shape;
/// those two have no direct form and make the entry unrepresentable.
fn condition_777(tag: &mut Tag, kind: &str) -> bool {
    tag.remove("condition");
    match kind {
        "minecraft:block_state_property" => {
            tag.insert("type", Tag::string("minecraft:match_block"));
            if let Some(block) = tag.remove("block") {
                tag.insert("blocks", block);
            }
            if let Some(state) = tag.remove("properties") {
                tag.insert("state", state);
            }
            true
        }
        "minecraft:value_check" | "minecraft:reference" => {
            tag.insert("type", Tag::string(kind));
            false
        }
        _ => {
            tag.insert("type", Tag::string(kind));
            true
        }
    }
}

/// An entity predicate. 26.2 writes it as a map of namespaced sub-predicates
/// (`{"minecraft:entity_type": .., "minecraft:flags": ..}`); 1.21.11 and
/// 26.1 as a record with plain field names, the location ones inline and the
/// type-specific one under `type_specific: {type, ..}`. 26.3 keeps 26.2's
/// form, so only nested predicates are visited there.
fn entity_predicate(x: &mut Xc<'_>, tag: &mut Tag) {
    let down = matches!(x.t.client, Proto::V774 | Proto::V775);
    let Tag::Compound(entries) = tag else {
        return;
    };
    let old = std::mem::take(entries);
    let mut out = Vec::with_capacity(old.len());
    for (key, mut value) in old {
        let name = String::from_utf8_lossy(&key).into_owned();
        let short = name.strip_prefix("minecraft:").unwrap_or(&name);
        if matches!(short, "vehicle" | "passenger" | "targeted_entity") {
            entity_predicate(x, &mut value);
        }
        if !down {
            out.push((key, value));
            continue;
        }
        match short {
            "entity_type" => out.push((b"type".to_vec(), value)),
            "entity_tags" => x
                .notes
                .push("an entity tag predicate has no 26.1 form; removed from an enchantment"),
            _ => {
                if let Some(kind) = short.strip_prefix("type_specific/") {
                    let kind = if kind == "cube_mob" { "slime" } else { kind };
                    let mut specific = match value {
                        Tag::Compound(fields) => fields,
                        _ => Vec::new(),
                    };
                    specific.insert(
                        0,
                        (b"type".to_vec(), Tag::string(&format!("minecraft:{kind}"))),
                    );
                    out.push((b"type_specific".to_vec(), Tag::Compound(specific)));
                } else {
                    out.push((short.as_bytes().to_vec(), value));
                }
            }
        }
    }
    *entries = out;
}

/// 26.3 block states in enchantment effects: `{Name, Properties}` became
/// `{id, properties}`, and block state providers lost their `_provider`
/// suffixes; a simple provider is the block state itself.
fn block_states_777(tag: &mut Tag) {
    if let Some(provider) = tag.get_mut("block_state") {
        state_provider_777(provider);
    }
}

fn state_provider_777(tag: &mut Tag) {
    let kind = tag.get("type").and_then(Tag::as_str).map(str::to_owned);
    match kind.as_deref() {
        Some("minecraft:simple_state_provider" | "simple_state_provider") => {
            if let Some(mut state) = tag.remove("state") {
                block_state_777(&mut state);
                *tag = state;
            }
        }
        Some(kind) => {
            let short = kind.strip_prefix("minecraft:").unwrap_or(kind);
            let renamed = match short {
                "randomized_int_state_provider" => "randomized_int".to_owned(),
                "rotated_block_provider" => "rotated".to_owned(),
                "rule_based_state_provider" => "rule_based".to_owned(),
                "weighted_state_provider" => "weighted".to_owned(),
                other => other.replace("_provider", ""),
            };
            tag.insert("type", Tag::string(&format!("minecraft:{renamed}")));
            for key in ["state", "default_state"] {
                if let Some(state) = tag.get_mut(key) {
                    block_state_777(state);
                }
            }
            if let Some(Tag::List(_, entries)) = tag.get_mut("entries") {
                for entry in entries {
                    if let Some(data) = entry.get_mut("data") {
                        block_state_777(data);
                    }
                }
            }
        }
        None => {}
    }
}

fn block_state_777(tag: &mut Tag) {
    if let Some(name) = tag.remove("Name") {
        tag.insert("id", name);
    }
    if let Some(properties) = tag.remove("Properties") {
        tag.insert("properties", properties);
    }
}
