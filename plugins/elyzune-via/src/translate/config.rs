//! Configuration state: known packs, synchronised registries, tags.
//!
//! # How registries reach an older or newer client
//!
//! The backend offers the vanilla core pack (`minecraft:core` at "26.2") and
//! sends every synchronised registry as entry names only, relying on the
//! client's own copy of the vanilla data for the contents. The translator
//! keeps that model and makes it hold for other releases:
//!
//! 1. `select_known_packs` (clientbound) offers the client its own release's
//!    core pack instead of 26.2's, so a vanilla client accepts it and loads
//!    the contents from its own jar, which its own codecs can read.
//! 2. The client's answer is rewritten back to 26.2's core pack before it
//!    reaches the backend, which refuses any other answer.
//! 3. `registry_data` keeps only the entries the client's release ships (an
//!    entry without data that the client cannot resolve fails the whole
//!    configuration), drops registries the client does not have, and records
//!    how the backend's ids map to the client's for every later packet that
//!    names a registry entry by id.
//! 4. Before `finish_configuration`, registries the client synchronises and
//!    the backend never sends (26.3's `decorated_pot_pattern`,
//!    `block_transformer`, `worldgen/block_state_provider`) are sent with the
//!    client's vanilla entries.

use bytes::Bytes;
use infrarust_api::filter::TranslatorOutput;
use infrarust_api::types::RawPacket;

use crate::generated::Proto;
use crate::generated::packets::{ConfigCb, ConfigSb};
use crate::layout::{Dir, Xc};
use crate::session::ViaTranslator;
use crate::state::{DynRegistry, core_pack_versions, synced, vanilla_entries};
use crate::wire::{Put, Reader, WireError, WireResult};

use super::play::transcode;
use super::text_packets;

pub fn clientbound(
    t: &mut ViaTranslator,
    kind: ConfigCb,
    body: Bytes,
    out: &mut TranslatorOutput,
) -> WireResult<()> {
    let name = kind.name();
    let body = match kind {
        ConfigCb::SelectKnownPacks => known_packs_to_client(t.client, &body)?,
        ConfigCb::RegistryData => match registry_data(t, &body)? {
            Some(body) => body,
            None => return Ok(()),
        },
        ConfigCb::UpdateTags => super::tags::update_tags(t, &body)?,
        ConfigCb::Disconnect => {
            let Some(body) = transcode(t, name, Dir::S2c, |x, o| {
                text_packets::leading_text(x, &body, o)
            })?
            else {
                return Ok(());
            };
            body
        }
        ConfigCb::ResourcePackPush => {
            let Some(body) = transcode(t, name, Dir::S2c, |x, o| {
                text_packets::resource_pack_push(x, &body, o)
            })?
            else {
                return Ok(());
            };
            body
        }
        ConfigCb::ServerLinks => {
            let Some(body) = transcode(t, name, Dir::S2c, |x, o| {
                text_packets::server_links(x, &body, o)
            })?
            else {
                return Ok(());
            };
            body
        }
        // Configuration sends the dialog itself (`Dialog.CONTEXT_FREE_STREAM_CODEC`).
        ConfigCb::ShowDialog => {
            let Some(body) = transcode(t, name, Dir::S2c, |x, o| {
                text_packets::dialog_nbt(x, &body, o)
            })?
            else {
                return Ok(());
            };
            body
        }
        _ => body,
    };
    t.emit_config_cb(kind, body, out);
    Ok(())
}

pub fn serverbound(
    t: &mut ViaTranslator,
    kind: ConfigSb,
    body: Bytes,
    out: &mut TranslatorOutput,
) -> WireResult<()> {
    let body = match kind {
        ConfigSb::SelectKnownPacks => known_packs_to_server(t, &body)?,
        _ => body,
    };
    t.emit_config_sb(kind, body, out);
    Ok(())
}

/// One `KnownPack`: namespace, id, version.
type Pack<'a> = (&'a [u8], &'a [u8], &'a [u8]);

fn read_packs<'a>(r: &mut Reader<'a>) -> WireResult<Vec<Pack<'a>>> {
    let n = r.len()?;
    let mut packs = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        packs.push((r.str_bytes()?, r.str_bytes()?, r.str_bytes()?));
    }
    Ok(packs)
}

fn is_core(pack: &Pack<'_>) -> bool {
    pack.0 == b"minecraft" && pack.1 == b"core"
}

fn write_packs(packs: &[Pack<'_>]) -> Bytes {
    let mut out = Vec::new();
    out.put_len(packs.len());
    for (ns, id, version) in packs {
        out.put_str_bytes(ns);
        out.put_str_bytes(id);
        out.put_str_bytes(version);
    }
    out.into()
}

/// Offers the client its own release's core pack in place of 26.2's. Every
/// release id sharing the client's protocol is offered, because the client
/// accepts only an exact match with its own id.
pub fn known_packs_to_client(client: Proto, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let packs = read_packs(&mut r)?;
    let mut rewritten: Vec<Pack<'_>> = Vec::with_capacity(packs.len() + 2);
    for pack in &packs {
        if is_core(pack)
            && core_pack_versions(Proto::V776)
                .iter()
                .any(|v| v.as_bytes() == pack.2)
        {
            for version in core_pack_versions(client) {
                rewritten.push((pack.0, pack.1, version.as_bytes()));
            }
        } else {
            rewritten.push(*pack);
        }
    }
    Ok(write_packs(&rewritten))
}

/// Rewrites the client's selection back to 26.2's core pack.
///
/// The core pack appears once in the answer however many of its aliases the
/// client selected. A 775 client is offered all three 26.1 release ids (see
/// [`known_packs_to_client`]); a vanilla client keeps only its own, but a
/// client that keeps several has still accepted the one pack the backend
/// offered, and the backend compares the answer with its offer as a list
/// (vanilla's `SynchronizeRegistriesTask` does the same), so three copies of
/// `core 26.2` would be refused where one is accepted.
///
/// An empty selection is also answered with the core pack. A vanilla client
/// offered its own core pack always selects it; an empty answer comes from a
/// client without the vanilla data (scripted test clients answer this way).
/// The 26.2 backend has no registry contents to send, so it would refuse that
/// answer and close the connection; claiming the core pack lets such a client
/// in, and it then receives names it does not resolve, which only it can
/// notice. The rewrite is logged once.
fn known_packs_to_server(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let packs = read_packs(&mut r)?;
    let server_core = core_pack_versions(Proto::V776)
        .first()
        .copied()
        .unwrap_or("26.2")
        .as_bytes();
    let mut rewritten: Vec<Pack<'_>> = Vec::with_capacity(packs.len());
    let mut core_seen = false;
    for pack in &packs {
        if !is_core(pack) {
            rewritten.push(*pack);
        } else if !core_seen {
            core_seen = true;
            rewritten.push((pack.0, pack.1, server_core));
        }
    }
    if rewritten.is_empty() {
        t.drop_unsupported(
            "minecraft:select_known_packs",
            "the client selected no known pack; answering with the vanilla core pack",
        );
        rewritten.push((b"minecraft", b"core", server_core));
    }
    Ok(write_packs(&rewritten))
}

/// Filters one `registry_data` for the client and records the id remap.
/// Returns `None` when the client does not have the registry at all.
///
/// Layout: `Identifier registry, VarInt n, n × { Identifier entry,
/// Optional<NBT> data }`.
pub fn registry_data(t: &mut ViaTranslator, body: &[u8]) -> WireResult<Option<Bytes>> {
    let mut r = Reader::new(body);
    let registry = r.string()?.to_owned();
    let count = r.len()?;
    let known = vanilla_entries(t.client, &registry);
    let mut reg = DynRegistry {
        client_has_registry: known.is_some(),
        ..DynRegistry::default()
    };
    let mut out = Vec::with_capacity(body.len());
    out.put_str(&registry);
    let mut read = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let name = r.string()?;
        let has_data = r.bool()?;
        let data = if has_data {
            crate::nbt::Tag::read(&mut r)?
        } else {
            None
        };
        read.push((name, has_data, data));
    }
    if !r.is_empty() {
        return Err(WireError::Malformed("bytes after the last registry entry"));
    }
    // Inline contents are rewritten in the client's shape; an entry the
    // client's release cannot decode is left out like an unknown name.
    let mut notes = Vec::new();
    let mut converted = Vec::with_capacity(read.len());
    {
        let mut x = Xc::new(t, Dir::S2c);
        for (_, _, data) in &mut read {
            converted.push(match data {
                Some(tag) => crate::registry_data::entry(&mut x, &registry, tag),
                None => true,
            });
        }
        notes.append(&mut x.notes);
    }
    for note in notes {
        t.drop_unsupported("minecraft:registry_data", note);
    }
    let mut kept = 0usize;
    let mut entries = Vec::new();
    for ((name, has_data, data), convertible) in read.into_iter().zip(converted) {
        let resolvable = if has_data {
            convertible
        } else {
            known.is_some_and(|k| k.binary_search(&name).is_ok())
        };
        reg.names.push(name.to_owned());
        if known.is_some() && resolvable {
            reg.s2c.push(u32::try_from(kept).ok());
            kept += 1;
            let mut bytes = Vec::new();
            if let Some(tag) = data {
                tag.write(&mut bytes);
            } else if has_data {
                bytes.put_u8(crate::nbt::TAG_END);
            }
            entries.push((name, has_data, bytes));
        } else {
            reg.s2c.push(None);
        }
    }
    let dropped = count - kept;
    t.conn.dynamic.insert(registry.clone(), reg);
    if known.is_none() {
        tracing::debug!(
            registry,
            "elyzune-via: registry unknown to the client, not sent"
        );
        return Ok(None);
    }
    if dropped > 0 {
        tracing::debug!(
            registry,
            dropped,
            "elyzune-via: registry entries unknown to the client removed"
        );
    }
    out.put_len(kept);
    for (name, has_data, data) in entries {
        out.put_str(name);
        out.put_bool(has_data);
        out.put_bytes(&data);
    }
    Ok(Some(out.into()))
}

/// Sends the synchronised registries the client expects and the backend did
/// not send, with the client's vanilla entries (names only: the client
/// accepted its core pack, so it resolves them itself).
pub fn before_finish(t: &mut ViaTranslator, out: &mut TranslatorOutput) {
    let Some(id) = ConfigCb::RegistryData.id(t.client) else {
        return;
    };
    for (registry, entries) in synced(t.client) {
        if t.conn.dynamic.contains_key(*registry) {
            continue;
        }
        let mut body = Vec::new();
        body.put_str(registry);
        body.put_len(entries.len());
        for entry in *entries {
            body.put_str(entry);
            body.put_bool(false);
        }
        let reg = DynRegistry {
            names: entries.iter().map(|e| (*e).to_owned()).collect(),
            s2c: (0..entries.len()).map(|i| u32::try_from(i).ok()).collect(),
            client_has_registry: true,
        };
        t.conn.dynamic.insert((*registry).to_owned(), reg);
        out.to_client(RawPacket::new(id, body.into()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn packs(list: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_len(list.len());
        for (a, c, d) in list {
            b.put_str(a);
            b.put_str(c);
            b.put_str(d);
        }
        b
    }

    #[test]
    fn the_client_is_offered_its_own_core_pack() {
        let body = packs(&[("minecraft", "core", "26.2")]);
        let to_774 = known_packs_to_client(Proto::V774, &body).unwrap();
        assert_eq!(
            to_774.as_ref(),
            packs(&[("minecraft", "core", "1.21.11")]).as_slice()
        );
        let to_775 = known_packs_to_client(Proto::V775, &body).unwrap();
        assert_eq!(
            to_775.as_ref(),
            packs(&[
                ("minecraft", "core", "26.1"),
                ("minecraft", "core", "26.1.1"),
                ("minecraft", "core", "26.1.2"),
            ])
            .as_slice()
        );
    }

    #[test]
    fn the_backend_sees_its_own_core_pack() {
        let mut t = ViaTranslator::new(Proto::V775, 1);
        let answer =
            known_packs_to_server(&mut t, &packs(&[("minecraft", "core", "26.1.2")])).unwrap();
        assert_eq!(
            answer.as_ref(),
            packs(&[("minecraft", "core", "26.2")]).as_slice()
        );
        let empty = known_packs_to_server(&mut t, &packs(&[])).unwrap();
        assert_eq!(
            empty.as_ref(),
            packs(&[("minecraft", "core", "26.2")]).as_slice()
        );
        // Every alias it was offered, echoed back, is still one core pack.
        let all = known_packs_to_server(
            &mut t,
            &packs(&[
                ("minecraft", "core", "26.1"),
                ("minecraft", "core", "26.1.1"),
                ("minecraft", "core", "26.1.2"),
            ]),
        )
        .unwrap();
        assert_eq!(
            all.as_ref(),
            packs(&[("minecraft", "core", "26.2")]).as_slice()
        );
    }

    fn registry(name: &str, entries: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_str(name);
        b.put_len(entries.len());
        for e in entries {
            b.put_str(e);
            b.put_bool(false);
        }
        b
    }

    #[test]
    fn entries_the_client_lacks_are_removed_and_ids_shift() {
        // 1.21.11 has no `minecraft:spear`-era additions missing, but it lacks
        // 26.2's new damage type; any name it does not ship must go.
        let mut t = ViaTranslator::new(Proto::V774, 1);
        let body = registry(
            "minecraft:damage_type",
            &[
                "minecraft:arrow",
                "minecraft:not_in_any_release",
                "minecraft:cactus",
            ],
        );
        let filtered = registry_data(&mut t, &body).unwrap().unwrap();
        assert_eq!(
            filtered.as_ref(),
            registry(
                "minecraft:damage_type",
                &["minecraft:arrow", "minecraft:cactus"]
            )
            .as_slice()
        );
        let reg = t.conn.dynamic("minecraft:damage_type").unwrap();
        assert_eq!(reg.s2c(0), Some(0));
        assert_eq!(reg.s2c(1), None);
        assert_eq!(reg.s2c(2), Some(1));
    }

    #[test]
    fn registries_the_client_lacks_are_not_sent() {
        let mut t = ViaTranslator::new(Proto::V774, 1);
        let body = registry("minecraft:world_clock", &["minecraft:overworld"]);
        assert!(registry_data(&mut t, &body).unwrap().is_none());
        assert_eq!(t.conn.dyn_s2c("minecraft:world_clock", 0), None);
    }

    #[test]
    fn missing_client_registries_are_added_before_finishing() {
        let mut t = ViaTranslator::new(Proto::V777, 1);
        for (name, entries) in synced(Proto::V776) {
            registry_data(&mut t, &registry(name, entries)).unwrap();
        }
        let mut out = TranslatorOutput::new();
        before_finish(&mut t, &mut out);
        let added: Vec<String> = out
            .take_to_client()
            .map(|p| Reader::new(&p.data).string().unwrap().to_owned())
            .collect();
        assert_eq!(
            added,
            vec![
                "minecraft:decorated_pot_pattern",
                "minecraft:block_transformer",
                "minecraft:worldgen/block_state_provider"
            ]
        );
    }
}
