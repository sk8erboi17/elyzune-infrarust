//! Text components.
//!
//! # Format
//!
//! Since 1.20.3 a text component travels as network NBT
//! (`ComponentSerialization.STREAM_CODEC`): a string tag for plain text, a
//! list tag for a component followed by its siblings, or a compound with one
//! content (`text`, `translate` + `with`, `score`, `selector` + `separator`,
//! `keybind`, `nbt` + `separator`, `object` + `fallback`), the siblings under
//! `extra`, and the style keys (`color`, `bold`, ..., `click_event`,
//! `hover_event`, `insertion`, `font`). A list whose elements have different
//! tag types is written with each element wrapped as `{"": element}`.
//!
//! # What differs between 1.21.11 and 26.3
//!
//! Style, the click and hover events and every content type have the same
//! codecs in the four releases (26.1 added an optional `fallback` to object
//! contents; 1.21.11 ignores the key). What does differ is what a component
//! can *name*:
//!
//! ```text
//! hover_event {action: "show_item", id, count, components}   item and data
//!     component names, and every value inside the components (codec form)
//! hover_event {action: "show_entity", id, uuid, name}        an entity type
//! click_event {action: "show_dialog", dialog}                a dialog name,
//!     or an inline dialog whose bodies hold text and item templates
//! ```
//!
//! A vanilla client decodes text with a strict codec, and a single name it
//! does not know fails the decode of the whole packet, which disconnects it.
//! So every such name is translated (`codec.rs` for items), and a hover whose
//! subject has no equivalent at all is removed rather than sent broken.
//!
//! # Cost
//!
//! Most text has no hover. The NBT bytes are searched for the three action
//! names and copied untouched when none occurs, so plain chat costs one
//! substring scan; only a component that mentions one is parsed into a tree
//! and written back.

use memchr::memmem;

use crate::codec;
use crate::generated::registries::Reg;
use crate::layout::{Xc, Xresult};
use crate::nbt::{self, Tag};
use crate::wire::{Put, Reader};

/// Content keys: a compound with one of these (or a style event) is a text
/// component rather than some other structure holding text.
const CONTENT_KEYS: &[&[u8]] = &[
    b"text",
    b"translate",
    b"score",
    b"selector",
    b"keybind",
    b"nbt",
    b"object",
];

/// One text component (`W::Text`): copied, or rewritten when it names
/// something that differs between the two releases.
pub fn component(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let start = r.pos();
    nbt::skip(r)?;
    write(x, r.since(start), out);
    Ok(())
}

/// Whether network NBT `raw` contains a hover or click action that can name
/// release-dependent data. False positives (the words inside plain text) only
/// cost a parse; a false negative is impossible, because an action is stored
/// as a string tag whose bytes appear verbatim.
#[must_use]
pub fn mentions_versioned_data(raw: &[u8]) -> bool {
    memmem::find_iter(raw, b"show_").any(|at| {
        let rest = &raw[at + 5..];
        rest.starts_with(b"item") || rest.starts_with(b"entity") || rest.starts_with(b"dialog")
    })
}

/// Writes the text component whose network NBT is `raw`, rewritten for the
/// destination when it needs it.
pub fn write(x: &mut Xc<'_>, raw: &[u8], out: &mut Vec<u8>) {
    if !mentions_versioned_data(raw) {
        out.put_bytes(raw);
        return;
    }
    match Tag::read(&mut Reader::new(raw)) {
        Ok(Some(mut tag)) => {
            tree(x, &mut tag);
            tag.write(out);
        }
        // An empty root, or bytes `nbt::skip` accepted but the tree reader
        // did not (they implement the same grammar): nothing to rewrite.
        Ok(None) | Err(_) => out.put_bytes(raw),
    }
}

/// Style keys whose values are booleans: kept as JSON booleans when a JSON
/// text component goes through the NBT tree (NBT has no boolean).
const BOOLEAN_KEYS: &[&str] = &[
    "bold",
    "italic",
    "underlined",
    "strikethrough",
    "obfuscated",
    "interpret",
    "hat",
];

/// A text component in JSON (login disconnect reasons, the status
/// description), rewritten like [`write`]: `None` when nothing in it differs
/// between the releases or it is not JSON, so the caller keeps the original
/// string.
#[must_use]
pub fn json(x: &mut Xc<'_>, text: &str) -> Option<String> {
    if !mentions_versioned_data(text.as_bytes()) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut tag = json_to_tag(&value);
    tree(x, &mut tag);
    serde_json::to_string(&tag_to_json(&tag, "")).ok()
}

fn json_to_tag(value: &serde_json::Value) -> Tag {
    use serde_json::Value as V;
    match value {
        V::Null => Tag::Compound(Vec::new()),
        V::Bool(b) => Tag::Byte(i8::from(*b)),
        V::Number(n) => match n.as_i64().and_then(|i| i32::try_from(i).ok()) {
            Some(i) => Tag::Int(i),
            None => Tag::Double(n.as_f64().unwrap_or(0.0)),
        },
        V::String(s) => Tag::string(s),
        V::Array(items) => {
            let items: Vec<Tag> = items.iter().map(json_to_tag).collect();
            let ty = items.first().map_or(nbt::TAG_END, Tag::type_id);
            Tag::List(ty, items)
        }
        V::Object(map) => Tag::Compound(
            map.iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), json_to_tag(v)))
                .collect(),
        ),
    }
}

fn tag_to_json(tag: &Tag, key: &str) -> serde_json::Value {
    use serde_json::Value as V;
    let number = |f: f64| serde_json::Number::from_f64(f).map_or(V::Null, V::Number);
    match tag {
        Tag::Byte(v) if BOOLEAN_KEYS.contains(&key) => V::Bool(*v != 0),
        Tag::Byte(v) => V::from(*v),
        Tag::Short(v) => V::from(*v),
        Tag::Int(v) => V::from(*v),
        Tag::Long(v) => V::from(*v),
        Tag::Float(v) => number(f64::from(*v)),
        Tag::Double(v) => number(*v),
        Tag::String(_) => V::String(tag.as_str().map(str::to_owned).unwrap_or_default()),
        Tag::List(_, items) => V::Array(items.iter().map(|t| tag_to_json(t, "")).collect()),
        Tag::Compound(entries) => V::Object(
            entries
                .iter()
                .map(|(k, v)| {
                    let k = String::from_utf8_lossy(k).into_owned();
                    let value = tag_to_json(v, &k);
                    (k, value)
                })
                .collect(),
        ),
        Tag::ByteArray(v) => V::Array(v.iter().map(|b| V::from(*b)).collect()),
        Tag::IntArray(v) => V::Array(v.iter().map(|i| V::from(*i)).collect()),
        Tag::LongArray(v) => V::Array(v.iter().map(|i| V::from(*i)).collect()),
    }
}

/// Rewrites a text component tree in place.
pub fn tree(x: &mut Xc<'_>, tag: &mut Tag) {
    match tag {
        Tag::List(_, items) => {
            for item in items {
                tree(x, item);
            }
        }
        Tag::Compound(entries) => {
            let mut drop_hover = false;
            let mut drop_click = false;
            for (key, value) in entries.iter_mut() {
                match key.as_slice() {
                    // `extra` and `with` hold components; `with` may also hold
                    // plain numbers and booleans, on which `tree` is a no-op.
                    b"extra" | b"with" => {
                        if let Tag::List(_, items) = value {
                            for item in items {
                                tree(x, item);
                            }
                        }
                    }
                    // `""` is the wrapper of a mixed-type list element.
                    b"separator" | b"fallback" | b"" => tree(x, value),
                    b"hover_event" => drop_hover = !hover(x, value),
                    b"click_event" => drop_click = !click(x, value),
                    _ => {}
                }
            }
            if drop_hover {
                tag.remove("hover_event");
                x.notes
                    .push("a hover naming something with no equivalent was removed");
            }
            if drop_click {
                tag.remove("click_event");
                x.notes
                    .push("a dialog click naming a dialog the peer lacks was removed");
            }
        }
        _ => {}
    }
}

/// Rewrites a `hover_event` value; false when it cannot be shown at all.
fn hover(x: &mut Xc<'_>, event: &mut Tag) -> bool {
    let action = event.get("action").and_then(Tag::as_str).map(str::to_owned);
    match action.as_deref() {
        Some("show_text") => {
            if let Some(value) = event.get_mut("value") {
                tree(x, value);
            }
            true
        }
        // The item's fields sit in the event itself (`ItemStackTemplate.MAP_CODEC`
        // in 26.x, `ItemStack.MAP_CODEC` in 1.21.11: the same keys).
        Some("show_item") => codec::item_fields(x, event),
        Some("show_entity") => {
            let mapped = event
                .get("id")
                .and_then(Tag::as_str)
                .map(|id| codec::map_name(x, Reg::EntityType, id));
            match mapped {
                Some(Some(name)) => event.insert("id", Tag::string(name)),
                Some(None) => return false,
                None => {}
            }
            if let Some(name) = event.get_mut("name") {
                tree(x, name);
            }
            true
        }
        _ => true,
    }
}

/// Rewrites a `click_event` value; false when it names a dialog the peer
/// does not have.
fn click(x: &mut Xc<'_>, event: &mut Tag) -> bool {
    if event.get("action").and_then(Tag::as_str) != Some("show_dialog") {
        return true;
    }
    if let Some(Tag::String(_)) = event.get("dialog") {
        let name = event.get("dialog").and_then(Tag::as_str).unwrap_or("");
        return codec::dynamic_has(x, "minecraft:dialog", name);
    }
    if let Some(dialog) = event.get_mut("dialog") {
        walk(x, dialog);
    }
    true
}

/// Rewrites every text component and item template inside an arbitrary
/// structure that embeds them: an inline dialog, a registry entry. A
/// compound that is itself a text component is handed to [`tree`] whole, so
/// nothing is translated twice; an item body (`{type: "minecraft:item",
/// item}`) has its item translated.
pub fn walk(x: &mut Xc<'_>, tag: &mut Tag) {
    match tag {
        Tag::List(_, items) => {
            for item in items {
                walk(x, item);
            }
        }
        Tag::Compound(entries) => {
            let is_text = entries.iter().any(|(key, _)| {
                CONTENT_KEYS.contains(&key.as_slice())
                    || key.as_slice() == b"hover_event"
                    || key.as_slice() == b"click_event"
            });
            if is_text {
                tree(x, tag);
                return;
            }
            let item_body = tag.get("type").and_then(Tag::as_str) == Some("minecraft:item");
            if let Tag::Compound(entries) = tag {
                for (key, value) in entries.iter_mut() {
                    if item_body && key.as_slice() == b"item" {
                        // Every 776 item has a client equivalent (fallbacks
                        // included); one that does not keeps its old id.
                        codec::item(x, value);
                    } else {
                        walk(x, value);
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_versioned_actions_trigger_a_rewrite() {
        assert!(!mentions_versioned_data(b"\x08\x00\x05hello"));
        assert!(!mentions_versioned_data(b"show_text"));
        assert!(mentions_versioned_data(b"..show_item.."));
        assert!(mentions_versioned_data(b"show_entity"));
        assert!(mentions_versioned_data(b"xx show_dialog"));
        assert!(!mentions_versioned_data(b"show_"));
    }
}
