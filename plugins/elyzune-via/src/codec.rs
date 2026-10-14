//! Values in their *codec form*: the NBT a vanilla `Codec` writes, as opposed
//! to the `StreamCodec` bytes `item.rs` and `layout.rs` handle.
//!
//! # Where codec form reaches the wire
//!
//! - item stacks inside text hovers (`show_item`), inline dialogs and the
//!   NBT of block entities (a pot's item, a campfire's `Items`);
//! - the `block_entity_data` and `entity_data` item components, whose
//!   payload is a block entity's or entity's saved NBT;
//! - block entity update tags (`chunk.rs`, `block_entity_data` packets);
//! - registry entries sent inline (`registry_data.rs`).
//!
//! # The codec form of an item
//!
//! ```text
//! ItemStack / ItemStackTemplate   {id: "ns:item", count?: int, components?: {..}}
//!                                 (26.x templates also accept a bare id)
//! DataComponentPatch              {"ns:component": value, "!ns:component": {}}
//! ```
//!
//! Every name in it is a registry entry: the item, the component types, and
//! whatever the values name (enchantments, blocks of a tool rule, sounds,
//! attributes, banner patterns, trim materials, entity types...). A vanilla
//! decoder rejects an unknown name, and inside a hover that rejection fails
//! the whole packet, so every name is translated to the destination's, an
//! entry with no equivalent is removed with the smallest enclosing part that
//! can go (a list element, a component, the item), and each removal is
//! noted for the once-per-connection log.
//!
//! # Values whose codec form changed
//!
//! Between 1.21.11 and 26.2 the codec forms are compatible except where 26.1
//! turned tag keys into holder sets (`damage_resistant`, `blocks_attacks`'
//! `bypassed_by`, `provides_banner_patterns`): 1.21.11 reads only the `#tag`
//! string, so a 26.x direct list cannot go down. 26.3 changed:
//!
//! ```text
//! pot_decorations   26.2: ["ns:item" × ≤4]          26.3: {back, left, right, front: template}
//! trim material     26.2: {asset_name, override_armor_assets?, description}
//!                   26.3: {palette_id: "ns:trim/<asset_name>", description}
//! instrument        26.3 adds durability_damage (optional, 0)
//! swing_animation   26.3 splits it into attack_animation and interact_animation
//! map_color         26.3 has none
//! signs             26.3 items carry sign_text_front/back and waxed where
//!                   26.2 keeps the sign in block_entity_data
//! teleport_randomly 26.3 adds directional_particles (optional, false)
//! map decorations   26.3 adds five structure marker types
//! ```
//!
//! and a few 26.3-only components with no 26.2 counterpart, which are
//! removed going to the backend.

use std::borrow::Cow;

use crate::generated::Proto;
use crate::generated::registries::{Reg, name_of};
use crate::layout::{Dir, Xc};
use crate::nbt::{TAG_COMPOUND, TAG_STRING, Tag};
use crate::state::vanilla_entries;
use crate::tables::id_by_name;

/// `DyeColor` by network id (`DyeColor.STREAM_CODEC` is its id).
pub const DYE_COLORS: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

/// `name` with the `minecraft:` namespace an identifier defaults to.
#[must_use]
pub fn qualified(name: &str) -> Cow<'_, str> {
    if name.contains(':') {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("minecraft:{name}"))
    }
}

/// The destination's name for the source entry `name` of static registry
/// `reg`, through the id tables (fallbacks included); `None` when the entry
/// has no equivalent.
#[must_use]
pub fn map_name(x: &Xc<'_>, reg: Reg, name: &str) -> Option<&'static str> {
    let id = id_by_name(reg, x.src_proto(), name)?;
    let mapped = x.map_static(reg, id)?;
    name_of(reg, x.dst_proto().number().unsigned_abs(), mapped)
}

/// Whether the destination has the entry `name` of the synchronised registry
/// `registry`: the client keeps what the configuration translation left it
/// (`config.rs`), the backend has everything it sent.
#[must_use]
pub fn dynamic_has(x: &Xc<'_>, registry: &str, name: &str) -> bool {
    let name = qualified(name);
    let synced = x.t.conn.dynamic(registry);
    match (x.dir, synced) {
        (Dir::S2c, Some(reg)) => reg.client_id_of(&name).is_some(),
        (Dir::C2s, Some(reg)) => reg.names.iter().any(|n| *n == *name),
        (dir, None) => {
            let proto = if dir == Dir::S2c {
                x.t.client
            } else {
                Proto::V776
            };
            vanilla_entries(proto, registry).is_some_and(|e| e.binary_search(&&*name).is_ok())
        }
    }
}

fn owned_str(tag: Option<&Tag>) -> Option<String> {
    tag.and_then(Tag::as_str).map(str::to_owned)
}

// ---- items -------------------------------------------------------------------

/// An item stack or template in codec form, rewritten in place. False when
/// the item has no equivalent: the caller removes it.
pub fn item(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    match tag {
        Tag::String(_) => {
            let Some(name) = tag.as_str().and_then(|n| map_name(x, Reg::Item, n)) else {
                return false;
            };
            // 1.21.11 reads stacks only in map form.
            *tag = if x.dst_proto() == Proto::V774 {
                Tag::Compound(vec![
                    (b"id".to_vec(), Tag::string(name)),
                    (b"count".to_vec(), Tag::Int(1)),
                ])
            } else {
                Tag::string(name)
            };
            true
        }
        Tag::Compound(_) => item_fields(x, tag),
        _ => true,
    }
}

/// The `id` and `components` of a compound that carries an item's fields
/// (a stack, or a `show_item` hover). False when the item has no equivalent.
pub fn item_fields(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    let Some(source) = owned_str(tag.get("id")) else {
        return true;
    };
    let Some(name) = map_name(x, Reg::Item, &source) else {
        return false;
    };
    if x.dir == Dir::C2s
        && id_by_name(Reg::Item, x.t.client, &source).is_some_and(|id| {
            crate::generated::registries::approximated_c2s(
                Reg::Item,
                x.t.client.number().unsigned_abs(),
            )
            .binary_search(&id)
            .is_ok()
        })
    {
        x.notes
            .push("an item only the client's release has reached 26.2 as its closest item");
    }
    tag.insert("id", Tag::string(name));
    if let Some(components) = tag.get_mut("components") {
        components_of(x, components, &source);
    }
    true
}

/// A list of codec-form stacks (`Items` of a block entity, a bundle):
/// elements with no equivalent are removed.
fn item_list(x: &mut Xc<'_>, tag: &mut Tag) {
    if let Tag::List(_, items) = tag {
        let before = items.len();
        items.retain_mut(|item| self::item(x, item));
        if items.len() != before {
            x.notes
                .push("an item with no equivalent was removed from a list");
        }
    }
}

// ---- component patches -------------------------------------------------------

/// What happens to one component of a patch.
enum Fate {
    /// Kept under this destination name.
    Keep(&'static str),
    /// Copied under two destination names (26.2 swing_animation for 26.3).
    Split(&'static str, &'static str),
    /// Part of a 26.3 sign item, folded into 26.2's block_entity_data.
    Sign(&'static str),
    /// Removed: the destination has no equivalent.
    Drop(&'static str),
}

fn fate(x: &Xc<'_>, name: &str) -> Fate {
    match (x.dir, x.t.client, name) {
        (Dir::S2c, Proto::V777, "minecraft:swing_animation") => {
            Fate::Split("minecraft:attack_animation", "minecraft:interact_animation")
        }
        (Dir::C2s, Proto::V777, "minecraft:attack_animation" | "minecraft:interact_animation") => {
            Fate::Keep("minecraft:swing_animation")
        }
        (Dir::C2s, Proto::V777, "minecraft:sign_text_front") => Fate::Sign("front_text"),
        (Dir::C2s, Proto::V777, "minecraft:sign_text_back") => Fate::Sign("back_text"),
        (Dir::C2s, Proto::V777, "minecraft:waxed") => Fate::Sign("is_waxed"),
        _ => match map_name(x, Reg::DataComponentType, name) {
            Some(mapped) => Fate::Keep(mapped),
            None => Fate::Drop("an item component with no equivalent was removed"),
        },
    }
}

/// A codec-form `DataComponentPatch` (`{"ns:name": value, "!ns:name": {}}`)
/// of the item `item` (source name), rewritten in place.
pub fn components_of(x: &mut Xc<'_>, patch: &mut Tag, item: &str) {
    let Tag::Compound(entries) = patch else {
        return;
    };
    let old = std::mem::take(entries);
    let mut out: Vec<(Vec<u8>, Tag)> = Vec::with_capacity(old.len() + 1);
    let mut sign: Vec<(&'static str, Tag)> = Vec::new();
    for (key, mut value) in old {
        let (removal, raw) = match key.split_first() {
            Some((b'!', rest)) => (true, rest),
            _ => (false, key.as_slice()),
        };
        let Ok(raw) = std::str::from_utf8(raw) else {
            // Not an identifier a vanilla codec would accept either way.
            out.push((key, value));
            continue;
        };
        let source = qualified(raw).into_owned();
        let prefix: &[u8] = if removal { b"!" } else { b"" };
        let key_for = |name: &str| [prefix, name.as_bytes()].concat();
        match fate(x, &source) {
            Fate::Keep(name) => {
                if removal || value_of(x, &source, &mut value) {
                    let key = key_for(name);
                    // Two 26.3 animations fold into one 26.2 key: first wins.
                    if !out.iter().any(|(k, _)| *k == key) {
                        out.push((key, value));
                    }
                } else {
                    x.notes
                        .push("an item component naming something with no equivalent was removed");
                }
            }
            Fate::Split(a, b) => {
                if removal || value_of(x, &source, &mut value) {
                    out.push((key_for(a), value.clone()));
                    out.push((key_for(b), value));
                }
            }
            Fate::Sign(field) => {
                if removal {
                    x.notes
                        .push("a removed 26.3 sign component has no 26.2 counterpart");
                } else {
                    let value = if field == "is_waxed" {
                        Tag::Byte(1)
                    } else {
                        sign_text(x, &mut value);
                        value
                    };
                    sign.push((field, value));
                }
            }
            Fate::Drop(why) => x.notes.push(why),
        }
    }
    if !sign.is_empty() {
        fold_sign(&mut out, sign, item);
    }
    *entries = out;
}

/// Puts 26.3 sign components into the 26.2 `block_entity_data` of the item,
/// creating it with the sign's block entity type when it is absent.
fn fold_sign(out: &mut Vec<(Vec<u8>, Tag)>, parts: Vec<(&'static str, Tag)>, item: &str) {
    const KEY: &[u8] = b"minecraft:block_entity_data";
    let index = match out.iter().position(|(k, _)| k.as_slice() == KEY) {
        Some(index) => index,
        None => {
            let kind = if item.ends_with("_hanging_sign") {
                "minecraft:hanging_sign"
            } else {
                "minecraft:sign"
            };
            out.push((
                KEY.to_vec(),
                Tag::Compound(vec![(b"id".to_vec(), Tag::string(kind))]),
            ));
            out.len() - 1
        }
    };
    if let Some((_, data)) = out.get_mut(index) {
        for (field, value) in parts {
            data.insert(field, value);
        }
    }
}

/// Rewrites one component value (source component `name`). False when the
/// value names something the destination lacks and cannot be kept.
fn value_of(x: &mut Xc<'_>, name: &str, value: &mut Tag) -> bool {
    let to_774 = x.dir == Dir::S2c && x.t.client == Proto::V774;
    let from_777 = x.dir == Dir::C2s && x.t.client == Proto::V777;
    match name {
        "minecraft:custom_name" | "minecraft:item_name" => crate::text::tree(x, value),
        "minecraft:lore" => {
            if let Tag::List(_, lines) = value {
                for line in lines {
                    crate::text::tree(x, line);
                }
            }
        }
        "minecraft:written_book_content" => {
            if let Some(Tag::List(_, pages)) = value.get_mut("pages") {
                for page in pages {
                    filterable_text(x, page);
                }
            }
        }
        "minecraft:container" => {
            if let Tag::List(_, slots) = value {
                let before = slots.len();
                slots
                    .retain_mut(|slot| slot.get_mut("item").is_none_or(|item| self::item(x, item)));
                if slots.len() != before {
                    x.notes
                        .push("an item with no equivalent was removed from a container");
                }
            }
        }
        "minecraft:bundle_contents" | "minecraft:charged_projectiles" => item_list(x, value),
        "minecraft:use_remainder" => return item(x, value),
        "minecraft:pot_decorations" => pot_decorations(x, value),
        "minecraft:trim" => {
            let material = value
                .get_mut("material")
                .is_none_or(|m| trim_material(x, m));
            let pattern = value
                .get_mut("pattern")
                .is_none_or(|p| holder(x, "minecraft:trim_pattern", p));
            return material && pattern;
        }
        "minecraft:provides_trim_material" => return trim_material(x, value),
        "minecraft:instrument" => {
            if !holder(x, "minecraft:instrument", value) {
                return false;
            }
            if let Some(sound) = value.get_mut("sound_event") {
                sound_event(x, sound);
            }
            if from_777 {
                value.remove("durability_damage");
            }
        }
        "minecraft:jukebox_playable" => {
            if !holder(x, "minecraft:jukebox_song", value) {
                return false;
            }
            if let Some(sound) = value.get_mut("sound_event") {
                sound_event(x, sound);
            }
        }
        "minecraft:banner_patterns" => banner_layers(x, value),
        "minecraft:enchantments" | "minecraft:stored_enchantments" => {
            if let Tag::Compound(levels) = value {
                let before = levels.len();
                levels.retain(|(key, _)| {
                    std::str::from_utf8(key)
                        .is_ok_and(|key| dynamic_has(x, "minecraft:enchantment", key))
                });
                if levels.len() != before {
                    x.notes.push("an enchantment the peer lacks was removed");
                }
            }
        }
        "minecraft:block_entity_data" => return typed_block_entity(x, value),
        "minecraft:entity_data" => return entity(x, value),
        "minecraft:bucket_entity_data" => {
            if let Some(name) = value.get_mut("CustomName") {
                crate::text::tree(x, name);
            }
        }
        "minecraft:attribute_modifiers" => {
            if let Tag::List(_, modifiers) = value {
                let before = modifiers.len();
                modifiers.retain_mut(|m| {
                    let Some(kind) = owned_str(m.get("type")) else {
                        return true;
                    };
                    let Some(mapped) = map_name(x, Reg::Attribute, &kind) else {
                        return false;
                    };
                    m.insert("type", Tag::string(mapped));
                    if let Some(display) = m.get_mut("display")
                        && let Some(text) = display.get_mut("value")
                    {
                        crate::text::tree(x, text);
                    }
                    true
                });
                if modifiers.len() != before {
                    x.notes
                        .push("an attribute modifier the peer lacks was removed");
                }
            }
        }
        "minecraft:tool" => {
            if let Some(Tag::List(_, rules)) = value.get_mut("rules") {
                rules.retain_mut(|rule| {
                    rule.get_mut("blocks")
                        .is_none_or(|b| holder_set(x, Reg::Block, b))
                });
            }
        }
        "minecraft:can_place_on" | "minecraft:can_break" => block_predicates(x, value),
        "minecraft:repairable" => {
            return value
                .get_mut("items")
                .is_none_or(|i| holder_set(x, Reg::Item, i));
        }
        "minecraft:equippable" => {
            if let Some(sound) = value.get_mut("equip_sound") {
                sound_event(x, sound);
            }
            if let Some(entities) = value.get_mut("allowed_entities")
                && !holder_set(x, Reg::EntityType, entities)
            {
                value.remove("allowed_entities");
            }
        }
        "minecraft:consumable" => {
            if let Some(sound) = value.get_mut("sound") {
                sound_event(x, sound);
            }
            if let Some(effects) = value.get_mut("on_consume_effects") {
                consume_effects(x, effects);
            }
        }
        "minecraft:death_protection" => {
            if let Some(effects) = value.get_mut("death_effects") {
                consume_effects(x, effects);
            }
        }
        "minecraft:break_sound" => sound_event(x, value),
        "minecraft:blocks_attacks" => {
            for key in ["block_sound", "disable_sound"] {
                if let Some(sound) = value.get_mut(key) {
                    sound_event(x, sound);
                }
            }
            if to_774
                && value
                    .get("bypassed_by")
                    .is_some_and(|b| !is_tag_reference(b))
            {
                value.remove("bypassed_by");
                x.notes
                    .push("1.21.11 reads bypassed_by only as a tag; a direct list was removed");
            }
        }
        "minecraft:damage_resistant" if to_774 => {
            return value.get("types").is_some_and(is_tag_reference);
        }
        "minecraft:provides_banner_patterns" if to_774 => return is_tag_reference(value),
        "minecraft:damage_type" => return holder(x, "minecraft:damage_type", value),
        "minecraft:map_decorations" => map_decorations(x, value),
        _ => {
            if let Some(registry) = variant_registry(name) {
                return holder(x, registry, value);
            }
        }
    }
    true
}

/// A page of a written book: a component, or `{raw, filtered?}`.
fn filterable_text(x: &mut Xc<'_>, page: &mut Tag) {
    if page.get("raw").is_some() {
        if let Some(raw) = page.get_mut("raw") {
            crate::text::tree(x, raw);
        }
        if let Some(filtered) = page.get_mut("filtered") {
            crate::text::tree(x, filtered);
        }
    } else {
        crate::text::tree(x, page);
    }
}

/// The synchronised registry an entity variant component refers to.
fn variant_registry(component: &str) -> Option<&'static str> {
    Some(match component {
        "minecraft:wolf/variant" => "minecraft:wolf_variant",
        "minecraft:wolf/sound_variant" => "minecraft:wolf_sound_variant",
        "minecraft:pig/variant" => "minecraft:pig_variant",
        "minecraft:pig/sound_variant" => "minecraft:pig_sound_variant",
        "minecraft:cow/variant" => "minecraft:cow_variant",
        "minecraft:cow/sound_variant" => "minecraft:cow_sound_variant",
        "minecraft:chicken/variant" => "minecraft:chicken_variant",
        "minecraft:chicken/sound_variant" => "minecraft:chicken_sound_variant",
        "minecraft:cat/variant" => "minecraft:cat_variant",
        "minecraft:cat/sound_variant" => "minecraft:cat_sound_variant",
        "minecraft:frog/variant" => "minecraft:frog_variant",
        "minecraft:painting/variant" => "minecraft:painting_variant",
        "minecraft:zombie_nautilus/variant" => "minecraft:zombie_nautilus_variant",
        _ => return None,
    })
}

/// A holder of a synchronised registry in codec form: a name the
/// destination must have, or an inline value (whose text is rewritten).
fn holder(x: &mut Xc<'_>, registry: &str, tag: &mut Tag) -> bool {
    match tag {
        Tag::String(_) => tag.as_str().is_some_and(|n| dynamic_has(x, registry, n)),
        Tag::Compound(_) => {
            crate::text::walk(x, tag);
            true
        }
        _ => true,
    }
}

/// Whether a holder set in codec form is a tag reference (`"#ns:tag"`).
fn is_tag_reference(tag: &Tag) -> bool {
    tag.as_str().is_some_and(|s| s.starts_with('#'))
}

/// A holder set of a static registry in codec form: `"#tag"` (kept), one
/// name, or a list of names. Names with no equivalent leave the list; false
/// when the set was a single name with none.
fn holder_set(x: &mut Xc<'_>, reg: Reg, tag: &mut Tag) -> bool {
    match tag {
        Tag::String(_) => {
            if is_tag_reference(tag) {
                return true;
            }
            match tag.as_str().and_then(|n| map_name(x, reg, n)) {
                Some(mapped) => {
                    *tag = Tag::string(mapped);
                    true
                }
                None => false,
            }
        }
        Tag::List(_, names) => {
            let before = names.len();
            names.retain_mut(|n| match n.as_str().and_then(|s| map_name(x, reg, s)) {
                Some(mapped) => {
                    *n = Tag::string(mapped);
                    true
                }
                None => false,
            });
            if names.len() != before {
                x.notes
                    .push("a registry entry with no equivalent left a holder set");
            }
            true
        }
        _ => true,
    }
}

/// `AdventureModePredicate`: one block predicate or a list of them.
fn block_predicates(x: &mut Xc<'_>, tag: &mut Tag) {
    let one = |x: &mut Xc<'_>, predicate: &mut Tag| {
        if let Some(blocks) = predicate.get_mut("blocks")
            && !holder_set(x, Reg::Block, blocks)
        {
            predicate.remove("blocks");
        }
    };
    match tag {
        Tag::List(_, predicates) => {
            for predicate in predicates {
                one(x, predicate);
            }
        }
        Tag::Compound(_) => one(x, tag),
        _ => {}
    }
}

/// A `SoundEvent` holder: a name, or `{sound_id, range?}`. A name the
/// destination lacks becomes the inline form, which needs no registry entry
/// (a resource pack can still define the sound).
pub fn sound_event(x: &mut Xc<'_>, tag: &mut Tag) {
    let Some(name) = owned_str(Some(tag)) else {
        return;
    };
    match map_name(x, Reg::SoundEvent, &name) {
        Some(mapped) => *tag = Tag::string(mapped),
        None => {
            *tag = Tag::Compound(vec![(b"sound_id".to_vec(), Tag::string(&qualified(&name)))]);
        }
    }
}

/// `ConsumeEffect` list (codec form `{type, ...}`).
fn consume_effects(x: &mut Xc<'_>, tag: &mut Tag) {
    let Tag::List(_, effects) = tag else {
        return;
    };
    let from_777 = x.dir == Dir::C2s && x.t.client == Proto::V777;
    for effect in effects {
        match effect.get("type").and_then(Tag::as_str) {
            Some("minecraft:play_sound" | "play_sound") => {
                if let Some(sound) = effect.get_mut("sound") {
                    sound_event(x, sound);
                }
            }
            Some("minecraft:teleport_randomly" | "teleport_randomly") if from_777 => {
                effect.remove("directional_particles");
            }
            _ => {}
        }
    }
}

/// `pot_decorations` (also a decorated pot's `sherds`). 26.2 and earlier: a
/// list of up to four item names (back, left, right, front; missing ones are
/// bricks); 26.3: four optional templates by side.
pub fn pot_decorations(x: &mut Xc<'_>, tag: &mut Tag) {
    const SIDES: [&str; 4] = ["back", "left", "right", "front"];
    const BRICK: &str = "minecraft:brick";
    let to_777 = x.dir == Dir::S2c && x.t.client == Proto::V777;
    let from_777 = x.dir == Dir::C2s && x.t.client == Proto::V777;
    if to_777 {
        let names: Vec<String> = match tag {
            Tag::List(_, items) => items.iter().filter_map(|i| owned_str(Some(i))).collect(),
            _ => return,
        };
        let mut sides = Vec::with_capacity(4);
        for (i, side) in SIDES.iter().enumerate() {
            let name = names
                .get(i)
                .and_then(|n| map_name(x, Reg::Item, n))
                .unwrap_or(BRICK);
            sides.push((
                side.as_bytes().to_vec(),
                Tag::Compound(vec![(b"id".to_vec(), Tag::string(name))]),
            ));
        }
        *tag = Tag::Compound(sides);
    } else if from_777 {
        let mut names = Vec::with_capacity(4);
        for side in SIDES {
            let name = match tag.get(side) {
                Some(Tag::String(_)) => owned_str(tag.get(side)),
                Some(template) => owned_str(template.get("id")),
                None => None,
            };
            let mapped = name
                .and_then(|n| map_name(x, Reg::Item, &n))
                .unwrap_or(BRICK);
            names.push(Tag::string(mapped));
        }
        *tag = Tag::List(TAG_STRING, names);
    } else if let Tag::List(_, items) = tag {
        for item in items {
            let mapped = item
                .as_str()
                .and_then(|n| map_name(x, Reg::Item, n))
                .unwrap_or(BRICK);
            *item = Tag::string(mapped);
        }
    }
}

/// A trim material holder: a name the destination must have, or an inline
/// material, whose asset group 26.3 replaced with a palette id
/// (`minecraft:trim/<asset_name>`; per-armour overrides have no 26.3 form).
fn trim_material(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    if let Tag::String(_) = tag {
        return holder(x, "minecraft:trim_material", tag);
    }
    trim_material_shape(x, tag);
    true
}

/// The inline form of a trim material (also a `trim_material` registry
/// entry): asset group in 26.2, palette id in 26.3; text rewritten.
pub fn trim_material_shape(x: &mut Xc<'_>, tag: &mut Tag) {
    let to_777 = x.dir == Dir::S2c && x.t.client == Proto::V777;
    let from_777 = x.dir == Dir::C2s && x.t.client == Proto::V777;
    if to_777 && let Some(asset) = owned_str(tag.get("asset_name")) {
        tag.remove("asset_name");
        if tag.remove("override_armor_assets").is_some() {
            x.notes
                .push("26.3 trim materials have no per-armour overrides; removed");
        }
        tag.insert(
            "palette_id",
            Tag::string(&format!("minecraft:trim/{asset}")),
        );
    }
    if from_777 && let Some(palette) = owned_str(tag.get("palette_id")) {
        tag.remove("palette_id");
        let path = palette
            .rsplit_once(':')
            .map_or(palette.as_str(), |(_, p)| p);
        let asset = path.strip_prefix("trim/").unwrap_or(path);
        tag.insert("asset_name", Tag::string(asset));
    }
    if let Some(description) = tag.get_mut("description") {
        crate::text::tree(x, description);
    }
}

/// Banner pattern layers (`[{pattern, color}]`): a layer whose pattern the
/// destination lacks is removed.
fn banner_layers(x: &mut Xc<'_>, tag: &mut Tag) {
    if let Tag::List(_, layers) = tag {
        let before = layers.len();
        layers.retain_mut(|layer| {
            layer
                .get_mut("pattern")
                .is_none_or(|p| holder(x, "minecraft:banner_pattern", p))
        });
        if layers.len() != before {
            x.notes.push("a banner pattern the peer lacks was removed");
        }
    }
}

/// `map_decorations`: `{key: {type, x, z, rotation}}`. 26.3 added structure
/// markers 26.2 lacks; they are shown as the plains village marker going up
/// (as ViaVersion does).
pub fn map_decorations(x: &mut Xc<'_>, tag: &mut Tag) {
    let Tag::Compound(entries) = tag else {
        return;
    };
    for (_, decoration) in entries.iter_mut() {
        let Some(kind) = owned_str(decoration.get("type")) else {
            continue;
        };
        let mapped =
            map_name(x, Reg::MapDecorationType, &kind).unwrap_or("minecraft:village_plains");
        decoration.insert("type", Tag::string(mapped));
    }
}

/// `SignText` in codec form (`{messages: [4], filtered_messages?: [4],
/// color, has_glowing_text}`); identical in every release.
pub fn sign_text(x: &mut Xc<'_>, tag: &mut Tag) {
    for key in ["messages", "filtered_messages"] {
        if let Some(Tag::List(_, lines)) = tag.get_mut(key) {
            for line in lines {
                crate::text::tree(x, line);
            }
        }
    }
}

// ---- block entities and entities ---------------------------------------------

/// A `TypedEntityData` of a block entity (`{id: type, ...saved fields}`).
/// False when the type has no equivalent.
fn typed_block_entity(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    let Some(source) = owned_str(tag.get("id")) else {
        return true;
    };
    let Some(mapped) = map_name(x, Reg::BlockEntityType, &source) else {
        return false;
    };
    tag.insert("id", Tag::string(mapped));
    block_entity(x, &qualified(&source), tag);
    true
}

/// The NBT of a block entity of source type `kind` (an update tag, or saved
/// fields inside an item), rewritten in place. Only the fields that can name
/// release-dependent data are touched; see the module docs.
pub fn block_entity(x: &mut Xc<'_>, kind: &str, tag: &mut Tag) {
    match kind {
        "minecraft:sign" | "minecraft:hanging_sign" => {
            for side in ["front_text", "back_text"] {
                if let Some(text) = tag.get_mut(side) {
                    sign_text(x, text);
                }
            }
        }
        "minecraft:banner" => {
            if let Some(layers) = tag.get_mut("patterns") {
                banner_layers(x, layers);
            }
            if let Some(components) = tag.get_mut("components") {
                components_of(x, components, "minecraft:white_banner");
            }
        }
        "minecraft:decorated_pot" => {
            if let Some(sherds) = tag.get_mut("sherds") {
                pot_decorations(x, sherds);
            }
            if tag.get_mut("item").is_some_and(|i| !item(x, i)) {
                tag.remove("item");
                x.notes.push("a pot's item with no equivalent was removed");
            }
        }
        "minecraft:brushable_block" => {
            if tag.get_mut("item").is_some_and(|i| !item(x, i)) {
                tag.remove("item");
                x.notes
                    .push("a brushable block's item with no equivalent was removed");
            }
        }
        "minecraft:mob_spawner" => {
            if let Some(data) = tag.get_mut("SpawnData")
                && let Some(spawned) = data.get_mut("entity")
            {
                entity(x, spawned);
            }
            if let Some(Tag::List(_, potentials)) = tag.get_mut("SpawnPotentials") {
                for potential in potentials {
                    if let Some(data) = potential.get_mut("data")
                        && let Some(spawned) = data.get_mut("entity")
                    {
                        entity(x, spawned);
                    }
                }
            }
        }
        _ => {}
    }
    // Fields several types share: names (beacon, skull, banner, containers)
    // and item lists (campfire, shelf, chests in an item's saved data).
    for key in ["CustomName", "custom_name"] {
        if let Some(name) = tag.get_mut(key) {
            crate::text::tree(x, name);
        }
    }
    if let Some(items) = tag.get_mut("Items") {
        item_list(x, items);
    }
}

/// An entity's saved NBT (`entity_data`, a spawner's `SpawnData`): its type,
/// name, carried items and passengers. False when the type has no
/// equivalent.
pub fn entity(x: &mut Xc<'_>, tag: &mut Tag) -> bool {
    if let Some(source) = owned_str(tag.get("id")) {
        let Some(mapped) = map_name(x, Reg::EntityType, &source) else {
            return false;
        };
        tag.insert("id", Tag::string(mapped));
    }
    if let Some(name) = tag.get_mut("CustomName") {
        crate::text::tree(x, name);
    }
    for key in ["Item", "item"] {
        if tag.get_mut(key).is_some_and(|i| !item(x, i)) {
            tag.remove(key);
        }
    }
    if let Some(Tag::Compound(slots)) = tag.get_mut("equipment") {
        slots.retain_mut(|(_, stack)| item(x, stack));
    }
    for key in ["Items", "Inventory"] {
        if let Some(items) = tag.get_mut(key) {
            item_list(x, items);
        }
    }
    if let Some(Tag::List(_, passengers)) = tag.get_mut("Passengers") {
        passengers.retain_mut(|p| entity(x, p));
    }
    true
}

/// A text line as a compound, so lines of different shapes (plain string,
/// list, compound) can share one homogeneous NBT list.
#[must_use]
pub fn text_as_compound(line: Tag) -> Tag {
    match line {
        Tag::Compound(_) => line,
        Tag::List(_, extra) => Tag::Compound(vec![
            (b"text".to_vec(), Tag::string("")),
            (b"extra".to_vec(), Tag::List(TAG_COMPOUND, extra)),
        ]),
        other => Tag::Compound(vec![(b"text".to_vec(), other)]),
    }
}
