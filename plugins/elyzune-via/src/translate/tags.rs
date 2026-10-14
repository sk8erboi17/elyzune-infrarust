//! `update_tags`, in configuration and in play.
//!
//! Layout: `VarInt registries, n × { Identifier registry, VarInt tags,
//! m × { Identifier tag, VarInt k, k × VarInt id } }`.
//!
//! Each id is an entry of the registry the block names, so it is remapped with
//! that registry's table (static registries) or with the ids recorded from
//! `registry_data` (synchronised registries). Ids without an equivalent are
//! removed from the tag; registries the client does not have are removed.
//!
//! Then the client's own vanilla tags that the backend did not send are added
//! (see `tools/generate_tables.py`, `gen_tags`): a client discards its built-in
//! tags when this packet arrives, and both older and newer releases read tags
//! 26.2 does not have (`blocks_motion` in 26.3 decides collisions client-side;
//! 1.21.11 still calls `supports_bamboo` `bamboo_plantable_on`).

use std::collections::HashSet;

use bytes::Bytes;

use crate::generated::registries::Reg;
use crate::generated::tags::TagEntries;
use crate::session::ViaTranslator;
use crate::tables::reg_by_name;
use crate::wire::{Put, Reader, WireResult, wire_id};

/// How the ids of one registry block are translated.
enum Ids<'a> {
    Static(Reg),
    Dynamic(&'a str),
}

pub fn update_tags(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let registries = r.len()?;
    let mut blocks: Vec<(String, Vec<u8>, usize)> = Vec::with_capacity(registries + 4);
    let mut sent: HashSet<(String, String)> = HashSet::new();

    for _ in 0..registries {
        let registry = r.string()?.to_owned();
        let tags = r.len()?;
        let ids = classify(t, &registry);
        let mut block = Vec::new();
        let mut written = 0usize;
        for _ in 0..tags {
            let tag = r.string()?;
            let count = r.len()?;
            let mut mapped = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                let id = r.varint()?;
                let Ok(id) = u32::try_from(id) else { continue };
                let client = match &ids {
                    Some(Ids::Static(reg)) => t.tables.reg(*reg).s2c(id),
                    Some(Ids::Dynamic(name)) => t.conn.dyn_s2c(name, id),
                    None => None,
                };
                if let Some(client) = client {
                    mapped.push(client);
                }
            }
            if ids.is_none() {
                continue;
            }
            sent.insert((registry.clone(), tag.to_owned()));
            block.put_str(tag);
            block.put_len(mapped.len());
            for id in mapped {
                block.put_varint(wire_id(id));
            }
            written += 1;
        }
        if ids.is_some() {
            blocks.push((registry, block, written));
        } else {
            t.drop_unsupported(
                "minecraft:update_tags",
                "tags of a registry the client does not have were removed",
            );
        }
    }

    // The client's vanilla tags the backend did not send.
    for (registry, tag, entries) in t.tables.extra_tags {
        if sent.contains(&((*registry).to_owned(), (*tag).to_owned())) {
            continue;
        }
        let ids: Vec<u32> = match entries {
            TagEntries::Ids(ids) => ids.to_vec(),
            TagEntries::Names(names) => match t.conn.dynamic(registry) {
                Some(reg) => names.iter().filter_map(|n| reg.client_id_of(n)).collect(),
                None => continue,
            },
        };
        let index = match blocks.iter().position(|(name, _, _)| name == registry) {
            Some(index) => index,
            None => {
                blocks.push(((*registry).to_owned(), Vec::new(), 0));
                blocks.len() - 1
            }
        };
        let (_, block, written) = &mut blocks[index];
        block.put_str(tag);
        block.put_len(ids.len());
        for id in ids {
            block.put_varint(wire_id(id));
        }
        *written += 1;
    }

    let mut out = Vec::with_capacity(body.len() + 1024);
    out.put_len(blocks.len());
    for (registry, block, written) in blocks {
        out.put_str(&registry);
        out.put_len(written);
        out.put_bytes(&block);
    }
    Ok(out.into())
}

/// How ids of `registry` translate for this client, or `None` when the client
/// does not have the registry.
fn classify<'a>(t: &ViaTranslator, registry: &'a str) -> Option<Ids<'a>> {
    if let Some(reg) = reg_by_name(registry) {
        return Some(Ids::Static(reg));
    }
    if let Some(dynamic) = t.conn.dynamic(registry) {
        return dynamic
            .client_has_registry
            .then_some(Ids::Dynamic(registry));
    }
    // Every static registry that carries tags is in the generated remap set,
    // so anything left is a synchronised registry the backend did not send.
    crate::state::vanilla_entries(t.client, registry)
        .is_some()
        .then_some(Ids::Dynamic(registry))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::generated::Proto;
    use crate::generated::registries::id_of;

    type Tags<'a> = &'a [(&'a str, &'a [i32])];
    type Parsed = Vec<(String, Vec<(String, Vec<i32>)>)>;

    fn tags_body(blocks: &[(&str, Tags<'_>)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_len(blocks.len());
        for (reg, tags) in blocks {
            b.put_str(reg);
            b.put_len(tags.len());
            for (tag, ids) in *tags {
                b.put_str(tag);
                b.put_len(ids.len());
                for id in *ids {
                    b.put_varint(*id);
                }
            }
        }
        b
    }

    fn parse(body: &[u8]) -> Parsed {
        let mut r = Reader::new(body);
        let mut out = Vec::new();
        for _ in 0..r.len().unwrap() {
            let reg = r.string().unwrap().to_owned();
            let mut tags = Vec::new();
            for _ in 0..r.len().unwrap() {
                let tag = r.string().unwrap().to_owned();
                let n = r.len().unwrap();
                tags.push((tag, (0..n).map(|_| r.varint().unwrap()).collect()));
            }
            out.push((reg, tags));
        }
        out
    }

    #[test]
    fn entity_type_ids_are_remapped_for_26_3() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        let player =
            i32::try_from(id_of(Reg::EntityType, 776, "minecraft:player").unwrap()).unwrap();
        let body = tags_body(&[("minecraft:entity_type", &[("minecraft:test", &[player])])]);
        let out = parse(&update_tags(&mut t, &body).unwrap());
        let entity = out
            .iter()
            .find(|(r, _)| r == "minecraft:entity_type")
            .unwrap();
        let test = entity
            .1
            .iter()
            .find(|(tag, _)| tag == "minecraft:test")
            .unwrap();
        assert_eq!(test.1, vec![159]);
        // 26.3's own entity-type tag the 26.2 data pack lacks is added.
        assert!(
            entity
                .1
                .iter()
                .any(|(tag, _)| tag == "minecraft:cannot_be_dismounted_by_item_usage")
        );
        // And its new block tags, under a registry block of their own.
        let blocks = out.iter().find(|(r, _)| r == "minecraft:block").unwrap();
        assert!(
            blocks
                .1
                .iter()
                .any(|(tag, ids)| tag == "minecraft:blocks_motion" && !ids.is_empty())
        );
    }

    #[test]
    fn old_names_of_renamed_tags_are_provided_for_1_21_11() {
        let mut t = ViaTranslator::new(Proto::V774, 1);
        let out = parse(&update_tags(&mut t, &tags_body(&[])).unwrap());
        let blocks = out.iter().find(|(r, _)| r == "minecraft:block").unwrap();
        assert!(
            blocks
                .1
                .iter()
                .any(|(tag, ids)| tag == "minecraft:bamboo_plantable_on" && !ids.is_empty())
        );
    }
}
