//! Item stacks and data components.
//!
//! # Encodings
//!
//! ```text
//! ItemStack (optional):  VarInt count; if count > 0: VarInt item, Patch
//! ItemStackTemplate:     VarInt item, VarInt count, Patch            (26.1+)
//! Patch:                 VarInt added, VarInt removed,
//!                        added × { VarInt type, value }, removed × VarInt type
//! Delimited patch:       as Patch, but each added value is preceded by its
//!                        VarInt byte length (serverbound creative slot)
//! HashedStack:           Bool present; VarInt item, VarInt count,
//!                        Map<VarInt type, i32 hash>, List<VarInt type>
//! ```
//!
//! A component value has no length on the wire, so every value is walked with
//! its generated layout (see `layout.rs`) even when only its type id changes.
//! A component the destination release does not have, or whose value names a
//! registry entry the destination lacks, is removed from the patch; the item
//! itself always survives.
//!
//! # Reshaped components
//!
//! Most components have the same layout in every release. The ones that do
//! not (`generated/components.rs`, `RESHAPED_*`) are converted here by hand,
//! following the two releases' codecs:
//!
//! - 1.21.11 wraps several holders in `EitherHolder` (a Bool, then the holder
//!   or a resource key); 26.1 sends the holder alone.
//! - 1.21.11 sends tag keys where 26.1 sends holder sets.
//! - 1.21.11 nests item stacks (count first) where 26.1 nests item templates
//!   (id first).
//! - 26.3 adds `durability_damage` to inline instruments, replaces an inline
//!   trim material's asset group with a palette id, and sends pot decorations
//!   as four optional item templates instead of four item ids.

use crate::generated::Proto;
use crate::generated::components as gc;
use crate::generated::registries::{Reg, id_of, name_of};
use crate::layout::{Custom, Dir, R, W, Xc, Xerr, Xresult};
use crate::nbt::Tag;
use crate::wire::{Put, Reader, WireError, WireResult, wire_id};

// ---- hand-written layouts used by the reshaping code ----------------------

static SOUND: W = W::Holder(
    R::Static(Reg::SoundEvent),
    &W::Struct(&[W::Str, W::Opt(&W::F32)]),
);
static DAMAGE_TYPES: W = W::HolderSet(R::Dynamic("minecraft:damage_type"));
static DAMAGE_REDUCTION: W = W::Struct(&[W::F32, W::Opt(&DAMAGE_TYPES), W::F32, W::F32]);

/// Dispatches a custom node of a generated layout.
pub fn custom(x: &mut Xc<'_>, c: Custom, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    match c {
        Custom::ItemTemplate => template(x, r, out),
        Custom::ItemStack => stack(x, r, out, false),
        Custom::OptionalItemStack => stack(x, r, out, true),
        Custom::Patch => patch(x, r, out, false, None),
        Custom::TypedComponent => typed_component(x, r, out),
        Custom::Particle => crate::particle::particle(x, r, out),
        Custom::ConsumeEffect => consume_effect(x, r, out),
        Custom::AttributeDisplay => {
            let kind = r.varint()?;
            out.put_varint(kind);
            if kind == 2 {
                x.value(&W::Text, r, out)?;
            }
            Ok(())
        }
    }
}

fn read_u32(r: &mut Reader<'_>) -> Xresult<u32> {
    u32::try_from(r.varint()?).map_err(|_| Xerr::Wire(WireError::Malformed("negative id")))
}

/// The item id, remapped; `None` when the item has no equivalent. An item
/// only a newer client has reaches the backend as the closest 26.2 item
/// (generated `approximated_c2s`), noted once per connection.
fn item(x: &mut Xc<'_>, id: u32) -> Option<u32> {
    if x.dir == Dir::C2s
        && crate::generated::registries::approximated_c2s(
            Reg::Item,
            x.t.client.number().unsigned_abs(),
        )
        .binary_search(&id)
        .is_ok()
    {
        x.notes
            .push("an item only the client's release has reached 26.2 as its closest item");
    }
    x.map_static(Reg::Item, id)
}

/// `ItemStackTemplate`, between two protocols that both use templates.
pub fn template(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let id = read_u32(r)?;
    let count = r.varint()?;
    let mut body = Vec::new();
    patch(x, r, &mut body, false, Some(id))?;
    let id = item(x, id).ok_or(Xerr::Absent("item"))?;
    out.put_varint(wire_id(id));
    out.put_varint(count);
    out.put_bytes(&body);
    Ok(())
}

/// `ItemStack`, between two protocols that both use stacks. An item with no
/// equivalent becomes the empty stack where the codec allows one.
pub fn stack(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>, optional: bool) -> Xresult<()> {
    let count = r.varint()?;
    if count <= 0 {
        if !optional {
            return Err(Xerr::Wire(WireError::Malformed(
                "empty stack where one is required",
            )));
        }
        out.put_varint(0);
        return Ok(());
    }
    let id = read_u32(r)?;
    let mut body = Vec::new();
    patch(x, r, &mut body, false, Some(id))?;
    match item(x, id) {
        Some(id) => {
            out.put_varint(count);
            out.put_varint(wire_id(id));
            out.put_bytes(&body);
            Ok(())
        }
        None if optional => {
            x.notes
                .push("an item with no equivalent was sent as an empty slot");
            out.put_varint(0);
            Ok(())
        }
        None => Err(Xerr::Absent("item")),
    }
}

/// An optional stack in a packet body (container slots, equipment, cursor).
pub fn optional_stack(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    stack(x, r, out, true)
}

/// A slot stack the client may later make claims about (container contents,
/// one slot, the cursor, a player inventory slot): translated like
/// [`optional_stack`], and, when the translation changed what the client
/// will hash, sent with the server's hashes backed up in its `custom_data`
/// and recorded in `records` under that `custom_data`'s hash (`hashed.rs`).
pub fn slot_stack(
    x: &mut Xc<'_>,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
    records: &mut Vec<(i32, crate::hashed::Sent)>,
) -> Xresult<()> {
    let start = r.pos();
    let out_start = out.len();
    x.hash_changed = false;
    stack(x, r, out, true)?;
    let server = r.since(start);
    let client = out.get(out_start..).unwrap_or_default();
    // Cheap test first: a stand-in item or a changed patch. The ids are the
    // first two VarInts (count, item) of both encodings.
    let item_changed = {
        let (mut s, mut c) = (Reader::new(server), Reader::new(client));
        match (s.varint(), s.varint(), c.varint(), c.varint()) {
            (Ok(sc), Ok(si), Ok(cc), Ok(ci)) if sc > 0 && cc > 0 => {
                let (si, ci) = (
                    u32::try_from(si).unwrap_or(0),
                    u32::try_from(ci).unwrap_or(0),
                );
                name_of(Reg::Item, 776, si)
                    != name_of(Reg::Item, x.t.client.number().unsigned_abs(), ci)
            }
            _ => false,
        }
    };
    if !(item_changed || x.hash_changed) {
        return Ok(());
    }
    // The stack itself is already translated: a failure to re-read it for
    // the record only costs the resends the record would have avoided, so
    // it is noted instead of failing the packet.
    let marked = sent_record(x.t, server, client).and_then(|record| match record {
        Some(record) => back_up_hashes(x.t, server, client, record),
        None => Ok(None),
    });
    match marked {
        Ok(Some((key, record, marked))) => {
            out.truncate(out_start);
            out.extend_from_slice(&marked);
            records.push((key, record));
        }
        Ok(None) => {}
        Err(_) => x.notes.push(
            "a translated slot stack could not be re-read for its click claims; \
             clicks on it are corrected by a resend",
        ),
    }
    Ok(())
}

/// The client's stack `client` (a translation of the server's `server`)
/// with the server-side hashes of `record` backed up in its `custom_data`
/// (`hashed::original_hashes` under `hashed::ORIGINAL_HASHES_KEY`, the
/// component created when the server's stack had none), the hash of that
/// `custom_data` as the client will claim it, and the record adjusted to
/// the stack actually sent: the client's component ids include
/// `custom_data`, and the server's own `custom_data`, which the client no
/// longer holds as is, is claimed with its computed hash.
///
/// `None` when the record could never answer a claim (a server component
/// this translator cannot hash, including a `custom_data` it cannot read):
/// the stack is then sent unchanged and claims about it reach the server
/// renumbered, which costs a resend and never a wrong match.
fn back_up_hashes(
    t: &crate::session::ViaTranslator,
    server: &[u8],
    client: &[u8],
    mut record: crate::hashed::Sent,
) -> Xresult<Option<(i32, crate::hashed::Sent, Vec<u8>)>> {
    use crate::hashed::{CREATED_KEY, Claim, ORIGINAL_HASHES_KEY, as_int, original_hashes};
    if !record.usable() {
        return Ok(None);
    }
    let client_number = t.client.number().unsigned_abs();
    let Some(client_type) = id_of(
        Reg::DataComponentType,
        client_number,
        "minecraft:custom_data",
    ) else {
        return Ok(None);
    };
    let (Some(server_stack), Some(client_stack)) = (
        read_stack(t, Proto::V776, server)?,
        read_stack(t, t.client, client)?,
    ) else {
        return Ok(None);
    };
    // The backup is built before the server's own custom_data turns into a
    // computed claim below: what the client can see of it (all of it) is
    // part of the key already.
    let backup = original_hashes(&record);
    for (ty, claim) in &mut record.server_added {
        if component_name(Proto::V776, *ty) == Some("minecraft:custom_data") {
            let Some(entry) = server_stack.added.iter().find(|e| e.ty == *ty) else {
                return Ok(None);
            };
            *claim = server_hash(t, entry);
            if *claim == Claim::Unknown {
                return Ok(None);
            }
        }
    }
    // A patch that removes custom_data cannot also carry one; no vanilla
    // item has a default custom_data, so this is a backend oddity, left to
    // resends.
    if client_stack.removed.contains(&client_type) {
        return Ok(None);
    }
    let existing = client_stack.added.iter().find(|e| e.ty == client_type);
    let mut custom_data = match existing {
        Some(entry) => match Tag::read(&mut Reader::new(entry.value))? {
            Some(tag @ Tag::Compound(_)) => tag,
            _ => return Ok(None),
        },
        None => Tag::Compound(vec![(CREATED_KEY.as_bytes().to_vec(), Tag::Byte(1))]),
    };
    custom_data.insert(ORIGINAL_HASHES_KEY, backup);
    let Some(key) = crate::hashed::nbt(&custom_data) else {
        return Ok(None);
    };
    let mut value = Vec::new();
    custom_data.write(&mut value);

    // Count first, then the item and the patch with custom_data replaced in
    // place or appended (a patch's order carries no meaning to the client).
    let mut marked = Vec::with_capacity(client.len() + value.len() + 2);
    marked.put_varint(Reader::new(client).varint()?);
    marked.put_varint(wire_id(client_stack.item));
    marked.put_len(client_stack.added.len() + usize::from(existing.is_none()));
    marked.put_len(client_stack.removed.len());
    for entry in &client_stack.added {
        marked.put_varint(wire_id(entry.ty));
        if entry.ty == client_type {
            marked.put_bytes(&value);
        } else {
            marked.put_bytes(entry.value);
        }
    }
    if existing.is_none() {
        marked.put_varint(wire_id(client_type));
        marked.put_bytes(&value);
        if let Err(at) = record.client_added.binary_search(&client_type) {
            record.client_added.insert(at, client_type);
        }
    }
    for ty in &client_stack.removed {
        marked.put_varint(wire_id(*ty));
    }
    Ok(Some((as_int(key), record, marked)))
}

/// The serverbound creative-slot stack: optional, with a delimited patch.
pub fn untrusted_stack(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let count = r.varint()?;
    if count <= 0 {
        out.put_varint(0);
        return Ok(());
    }
    let id = read_u32(r)?;
    let mut body = Vec::new();
    x.backup_item = None;
    patch(x, r, &mut body, true, Some(id))?;
    // A stand-in the translator sent (`slot_stack`) goes back as the server
    // item its backup names, as ViaBackwards restores its own backup's id:
    // a creative player moving 26.2 cinnabar bricks on 1.21.11 must not
    // turn them into the cut red sandstone shown for them. Only that
    // substitution is undone: the backup's item must be the one this
    // client is shown as the item it sent.
    let restored = x.backup_item.take().filter(|server| {
        *server != id
            && x.t.tables.reg(Reg::Item).s2c(*server) == Some(id)
            && x.t.tables.reg(Reg::Item).c2s(id) != Some(*server)
    });
    match restored.or_else(|| item(x, id)) {
        Some(id) => {
            out.put_varint(count);
            out.put_varint(wire_id(id));
            out.put_bytes(&body);
        }
        None => {
            x.notes
                .push("an item with no equivalent was sent as an empty slot");
            out.put_varint(0);
        }
    }
    Ok(())
}

/// A template read in the source protocol, written as a (non-empty) stack:
/// 26.x nests templates where 1.21.11 nests stacks.
fn template_to_stack(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let id = read_u32(r)?;
    let count = r.varint()?;
    let mut body = Vec::new();
    patch(x, r, &mut body, false, Some(id))?;
    let id = item(x, id).ok_or(Xerr::Absent("item"))?;
    out.put_varint(count.max(1));
    out.put_varint(wire_id(id));
    out.put_bytes(&body);
    Ok(())
}

/// The reverse of [`template_to_stack`]; an empty stack is `Absent`.
fn stack_to_template(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let count = r.varint()?;
    if count <= 0 {
        return Err(Xerr::Absent("empty stack"));
    }
    let id = read_u32(r)?;
    let mut body = Vec::new();
    patch(x, r, &mut body, false, Some(id))?;
    let id = item(x, id).ok_or(Xerr::Absent("item"))?;
    out.put_varint(wire_id(id));
    out.put_varint(count);
    out.put_bytes(&body);
    Ok(())
}

/// Component name of `id` in protocol `proto`.
fn component_name(proto: Proto, id: u32) -> Option<&'static str> {
    name_of(Reg::DataComponentType, proto.number() as u32, id)
}

/// The layout of the component `name` in the protocol `x` reads.
fn component_layout(x: &Xc<'_>, name: &str) -> Option<&'static W> {
    x.src
        .components
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, w)| w)
}

fn reshaped(client: Proto, name: &str) -> bool {
    let list = match client {
        Proto::V774 => gc::RESHAPED_774,
        Proto::V775 => gc::RESHAPED_775,
        Proto::V777 => gc::RESHAPED_777,
        Proto::V776 => &[][..],
    };
    list.contains(&name)
}

/// Skips one value of `layout` without remapping anything: used to step over
/// a component that is being dropped. Never fails with `Absent`.
fn skip_value(x: &mut Xc<'_>, layout: &W, r: &mut Reader<'_>) -> Xresult<()> {
    let mut sink = Vec::new();
    x.with_identity(|x| x.value(layout, r, &mut sink))
}

/// Transcodes one component value. On `Absent` the reader is left past the
/// value, so the caller can drop the component and continue with the next.
fn component_value(
    x: &mut Xc<'_>,
    name: &'static str,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    let mut probe = r.clone();
    let result = if let Some(kind) = nbt_valued(name) {
        nbt_value(x, kind, &mut probe, out)
    } else if reshaped(x.t.client, name) {
        reshape(x, name, &mut probe, out)
    } else {
        match component_layout(x, name) {
            Some(layout) => x.value(layout, &mut probe, out),
            None => Err(Xerr::Absent("component without a layout")),
        }
    };
    match result {
        Ok(()) => {
            *r = probe;
            Ok(())
        }
        Err(Xerr::Absent(why)) => {
            let layout = component_layout(x, name).ok_or(Xerr::Wire(WireError::Malformed(
                "component without a layout",
            )))?;
            skip_value(x, layout, r)?;
            Err(Xerr::Absent(why))
        }
        Err(e) => Err(e),
    }
}

/// Components whose value is (or ends in) NBT in codec form, which names
/// release-dependent data the layouts treat as opaque.
#[derive(Clone, Copy)]
enum NbtValued {
    /// `TypedEntityData<BlockEntityType>`: type id, then the saved fields.
    BlockEntity,
    /// `TypedEntityData<EntityType>`: type id, then the saved fields.
    Entity,
    /// `bucket_entity_data`: an entity's saved fields without a type.
    BucketEntity,
    /// `map_decorations`: `{key: {type, x, z, rotation}}`.
    MapDecorations,
}

fn nbt_valued(name: &str) -> Option<NbtValued> {
    Some(match name {
        "minecraft:block_entity_data" => NbtValued::BlockEntity,
        "minecraft:entity_data" => NbtValued::Entity,
        "minecraft:bucket_entity_data" => NbtValued::BucketEntity,
        "minecraft:map_decorations" => NbtValued::MapDecorations,
        _ => return None,
    })
}

/// Transcodes a [`NbtValued`] component: the type id is renumbered and the
/// NBT rewritten in codec form (`codec.rs`).
fn nbt_value(
    x: &mut Xc<'_>,
    kind: NbtValued,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    let typed = match kind {
        NbtValued::BlockEntity => Some(Reg::BlockEntityType),
        NbtValued::Entity => Some(Reg::EntityType),
        NbtValued::BucketEntity | NbtValued::MapDecorations => None,
    };
    let mut source_type = None;
    if let Some(reg) = typed {
        let ty = read_u32(r)?;
        let mapped = x
            .map_static(reg, ty)
            .ok_or(Xerr::Absent("type with no equivalent"))?;
        out.put_varint(wire_id(mapped));
        source_type = name_of(reg, x.src_proto().number().unsigned_abs(), ty);
    }
    let Some(mut tag) = Tag::read(r)? else {
        out.put_u8(crate::nbt::TAG_END);
        return Ok(());
    };
    match kind {
        NbtValued::BlockEntity => {
            crate::codec::block_entity(x, source_type.unwrap_or(""), &mut tag)
        }
        NbtValued::Entity | NbtValued::BucketEntity => {
            crate::codec::entity(x, &mut tag);
        }
        NbtValued::MapDecorations => crate::codec::map_decorations(x, &mut tag),
    }
    tag.write(out);
    Ok(())
}

/// What a patch does with one source component beyond renumbering it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// Renumbered and transcoded (or removed when the destination lacks it).
    Plain,
    /// 26.2 `swing_animation` for 26.3: the same value under
    /// `attack_animation` and `interact_animation`.
    SplitSwing,
    /// 26.3 `attack_animation` / `interact_animation` for 26.2: one
    /// `swing_animation` (the first of the two wins).
    MergeSwing,
    /// A 26.3 sign component for 26.2, folded into `block_entity_data`
    /// under this field of the sign's saved form.
    Sign(&'static str),
}

fn rule(x: &Xc<'_>, name: &str) -> Rule {
    match (x.dir, x.t.client, name) {
        (Dir::S2c, Proto::V777, "minecraft:swing_animation") => Rule::SplitSwing,
        (Dir::C2s, Proto::V777, "minecraft:attack_animation" | "minecraft:interact_animation") => {
            Rule::MergeSwing
        }
        (Dir::C2s, Proto::V777, "minecraft:sign_text_front") => Rule::Sign("front_text"),
        (Dir::C2s, Proto::V777, "minecraft:sign_text_back") => Rule::Sign("back_text"),
        (Dir::C2s, Proto::V777, "minecraft:waxed") => Rule::Sign("is_waxed"),
        _ => Rule::Plain,
    }
}

/// The destination id of component `name`.
fn dst_component(x: &Xc<'_>, name: &str) -> Option<u32> {
    id_of(
        Reg::DataComponentType,
        x.dst_proto().number().unsigned_abs(),
        name,
    )
}

/// A component patch of the item `item` (source id, when the codec has
/// one). `delimited` is the serverbound creative-slot form, where every
/// added value carries its byte length.
///
/// Besides renumbering, a patch is where components that changed identity
/// between releases are rewritten (see [`Rule`]): those cannot be handled
/// one value at a time because they add, merge or create components.
pub fn patch(
    x: &mut Xc<'_>,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
    delimited: bool,
    item: Option<u32>,
) -> Xresult<()> {
    let added = r.len()?;
    let removed = r.len()?;
    let src = x.src_proto();
    let mut body = Vec::new();
    let mut kept_added = 0usize;
    let mut emitted: Vec<u32> = Vec::new();
    let mut sign: Vec<(&'static str, Tag)> = Vec::new();
    // A block_entity_data held back so 26.3 sign parts can join it.
    let mut held_block_entity: Option<(u32, Tag)> = None;
    let push =
        |body: &mut Vec<u8>, kept: &mut usize, emitted: &mut Vec<u32>, id: u32, value: &[u8]| {
            if emitted.contains(&id) {
                return;
            }
            emitted.push(id);
            body.put_varint(wire_id(id));
            if delimited {
                body.put_len(value.len());
            }
            body.put_bytes(value);
            *kept += 1;
        };
    for _ in 0..added {
        let src_type = read_u32(r)?;
        let name = component_name(src, src_type)
            .ok_or(Xerr::Wire(WireError::Malformed("unknown component type")))?;
        let mut sub;
        let value_reader: &mut Reader<'_> = if delimited {
            let n = r.len()?;
            sub = Reader::new(r.bytes(n)?);
            &mut sub
        } else {
            &mut *r
        };
        let rule = rule(x, name);
        if rule != Rule::Plain || (x.t.client == Proto::V777 && name == "minecraft:pot_decorations")
        {
            x.hash_changed = true;
        }
        match rule {
            Rule::Plain => {
                let dst_type = x.map_static(Reg::DataComponentType, src_type);
                let mut value = Vec::new();
                let start = value_reader.pos();
                match (
                    convert_or_skip(x, name, dst_type, value_reader, &mut value),
                    dst_type,
                ) {
                    (Ok(()), Some(dst)) => {
                        // Rewritten text, an inline holder or block entity
                        // data can keep its component id while changing the
                        // codec hash: `sent_record` compares the two hashes.
                        if matches!(
                            name,
                            "minecraft:custom_name"
                                | "minecraft:item_name"
                                | "minecraft:lore"
                                | "minecraft:instrument"
                                | "minecraft:provides_trim_material"
                                | "minecraft:trim"
                                | "minecraft:block_entity_data"
                        ) && value_reader.since(start) != value
                        {
                            x.hash_changed = true;
                        }
                        // The hash backup a translated slot stack was sent
                        // with (`back_up_hashes`) never reaches the server:
                        // a creative client sends the stack back whole.
                        if name == "minecraft:custom_data"
                            && x.dir == Dir::C2s
                            && memchr::memmem::find(&value, b"elyzune-via|").is_some()
                            && let Some(mut tag) = Tag::read(&mut Reader::new(&value))?
                        {
                            let stripped = crate::hashed::strip_backup(&mut tag);
                            x.backup_item = stripped.server_item;
                            if !stripped.keep {
                                continue;
                            }
                            value.clear();
                            tag.write(&mut value);
                        }
                        if name == "minecraft:block_entity_data"
                            && x.dir == Dir::C2s
                            && x.t.client == Proto::V777
                        {
                            let mut v = Reader::new(&value);
                            let ty = read_u32(&mut v)?;
                            if let Some(tag) = Tag::read(&mut v)? {
                                held_block_entity = Some((ty, tag));
                                continue;
                            }
                        }
                        // A 26.3 item carries a sign's text and wax as
                        // components (`crate::codec::unfold_sign`).
                        if name == "minecraft:block_entity_data"
                            && x.dir == Dir::S2c
                            && x.t.client == Proto::V777
                        {
                            let mut v = Reader::new(&value);
                            let ty = read_u32(&mut v)?;
                            let kind = name_of(Reg::BlockEntityType, 777, ty).unwrap_or("");
                            if let Some(mut tag) = Tag::read(&mut v)? {
                                let (parts, rest) = crate::codec::unfold_sign(kind, &mut tag);
                                if !parts.is_empty() {
                                    x.hash_changed = true;
                                    for (component, part) in parts {
                                        let Some(id) = dst_component(x, component) else {
                                            continue;
                                        };
                                        let mut bytes = Vec::new();
                                        if component != "minecraft:waxed" {
                                            sign_text_to_network(&part, &mut bytes);
                                        }
                                        push(&mut body, &mut kept_added, &mut emitted, id, &bytes);
                                    }
                                    if rest {
                                        let mut bytes = Vec::new();
                                        bytes.put_varint(wire_id(ty));
                                        tag.write(&mut bytes);
                                        push(&mut body, &mut kept_added, &mut emitted, dst, &bytes);
                                    }
                                    continue;
                                }
                            }
                        }
                        push(&mut body, &mut kept_added, &mut emitted, dst, &value);
                    }
                    (Ok(()) | Err(Xerr::Absent(_)), _) => {
                        x.hash_changed = true;
                        x.notes
                            .push("an item component with no equivalent was removed");
                    }
                    (Err(e), _) => return Err(e),
                }
            }
            Rule::SplitSwing | Rule::MergeSwing => {
                // The value's layout is `SwingAnimation` in every release.
                let layout = component_layout(x, name).ok_or(Xerr::Wire(WireError::Malformed(
                    "component without a layout",
                )))?;
                let mut value = Vec::new();
                x.value(layout, value_reader, &mut value)?;
                let targets: &[&str] = if rule == Rule::SplitSwing {
                    &["minecraft:attack_animation", "minecraft:interact_animation"]
                } else {
                    &["minecraft:swing_animation"]
                };
                for target in targets {
                    if let Some(dst) = dst_component(x, target) {
                        push(&mut body, &mut kept_added, &mut emitted, dst, &value);
                    }
                }
            }
            Rule::Sign(field) => {
                if field == "is_waxed" {
                    sign.push((field, Tag::Byte(1)));
                } else {
                    sign.push((field, sign_text_to_nbt(x, value_reader)?));
                }
            }
        }
    }
    if !sign.is_empty() || held_block_entity.is_some() {
        let (ty, mut tag) = match held_block_entity {
            Some(held) => held,
            None => {
                let name = item
                    .and_then(|id| name_of(Reg::Item, src.number().unsigned_abs(), id))
                    .unwrap_or("");
                let kind = if name.ends_with("_hanging_sign") {
                    "minecraft:hanging_sign"
                } else {
                    "minecraft:sign"
                };
                let ty = id_of(Reg::BlockEntityType, 776, kind).ok_or(Xerr::Wire(
                    WireError::Malformed("no sign block entity type"),
                ))?;
                (ty, Tag::Compound(Vec::new()))
            }
        };
        for (field, value) in sign {
            tag.insert(field, value);
        }
        if let Some(dst) = dst_component(x, "minecraft:block_entity_data") {
            let mut value = Vec::new();
            value.put_varint(wire_id(ty));
            tag.write(&mut value);
            push(&mut body, &mut kept_added, &mut emitted, dst, &value);
        }
    }
    let mut removed_ids = Vec::with_capacity(removed.min(256));
    for _ in 0..removed {
        let src_type = read_u32(r)?;
        let name = component_name(src, src_type).unwrap_or("");
        let targets: &[&str] = match rule(x, name) {
            Rule::Plain => {
                if let Some(dst) = x.map_static(Reg::DataComponentType, src_type)
                    && !removed_ids.contains(&dst)
                {
                    removed_ids.push(dst);
                }
                continue;
            }
            Rule::SplitSwing => &["minecraft:attack_animation", "minecraft:interact_animation"],
            Rule::MergeSwing => &["minecraft:swing_animation"],
            Rule::Sign(_) => {
                x.notes
                    .push("a removed 26.3 sign component has no 26.2 counterpart");
                continue;
            }
        };
        for target in targets {
            if let Some(dst) = dst_component(x, target)
                && !removed_ids.contains(&dst)
            {
                removed_ids.push(dst);
            }
        }
    }
    out.put_len(kept_added);
    out.put_len(removed_ids.len());
    out.put_bytes(&body);
    for id in removed_ids {
        out.put_varint(wire_id(id));
    }
    Ok(())
}

/// A 26.3 `SignText` (network form: four lines, four optional filtered
/// lines, VarInt dye colour, Bool glowing) as the codec-form compound 26.2
/// stores under a sign's `front_text`/`back_text`.
fn sign_text_to_nbt(x: &mut Xc<'_>, r: &mut Reader<'_>) -> Xresult<Tag> {
    fn lines(x: &mut Xc<'_>, r: &mut Reader<'_>) -> Xresult<Tag> {
        let mut lines = Vec::with_capacity(4);
        for _ in 0..4 {
            let mut line = Tag::read(r)?.unwrap_or_else(|| Tag::string(""));
            crate::text::tree(x, &mut line);
            lines.push(crate::codec::text_as_compound(line));
        }
        Ok(Tag::List(crate::nbt::TAG_COMPOUND, lines))
    }
    let mut fields = vec![(b"messages".to_vec(), lines(x, r)?)];
    if r.bool()? {
        fields.push((b"filtered_messages".to_vec(), lines(x, r)?));
    }
    let color = usize::try_from(r.varint()?).unwrap_or(15);
    let color = crate::codec::DYE_COLORS
        .get(color)
        .copied()
        .unwrap_or("black");
    fields.push((b"color".to_vec(), Tag::string(color)));
    fields.push((b"has_glowing_text".to_vec(), Tag::Byte(i8::from(r.bool()?))));
    Ok(Tag::Compound(fields))
}

/// The reverse of [`sign_text_to_nbt`]: a `SignText` in codec form (as a
/// sign's `front_text`/`back_text`, its lines already in the client's text
/// form) written in 26.3's network form.
fn sign_text_to_network(text: &Tag, out: &mut Vec<u8>) {
    fn lines(text: &Tag, key: &str) -> Option<Vec<Tag>> {
        match text.get(key) {
            Some(Tag::List(_, lines)) => Some(lines.clone()),
            _ => None,
        }
    }
    fn put_lines(out: &mut Vec<u8>, lines: &[Tag]) {
        for index in 0..4 {
            lines
                .get(index)
                .cloned()
                .unwrap_or_else(|| Tag::string(""))
                .write(out);
        }
    }
    put_lines(out, &lines(text, "messages").unwrap_or_default());
    match lines(text, "filtered_messages") {
        Some(filtered) => {
            out.put_bool(true);
            put_lines(out, &filtered);
        }
        None => out.put_bool(false),
    }
    let color = text
        .get("color")
        .and_then(Tag::as_str)
        .and_then(|name| {
            let name = name.strip_prefix("minecraft:").unwrap_or(name);
            crate::codec::DYE_COLORS.iter().position(|dye| *dye == name)
        })
        .unwrap_or(15);
    out.put_varint(i32::try_from(color).unwrap_or(15));
    out.put_bool(matches!(text.get("has_glowing_text"), Some(Tag::Byte(glowing)) if *glowing != 0));
}

/// Converts one component value, or steps over it when the destination has
/// no such component (reported as `Absent`). Either way the reader ends past
/// the value.
fn convert_or_skip(
    x: &mut Xc<'_>,
    name: &'static str,
    dst_type: Option<u32>,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    if dst_type.is_some() {
        return component_value(x, name, r, out);
    }
    let layout = component_layout(x, name).ok_or(Xerr::Wire(WireError::Malformed(
        "component without a layout",
    )))?;
    skip_value(x, layout, r)?;
    Err(Xerr::Absent("component missing in the destination release"))
}

/// `TypedDataComponent`: a component type id and its value.
fn typed_component(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let src_type = read_u32(r)?;
    let name = component_name(x.src_proto(), src_type)
        .ok_or(Xerr::Wire(WireError::Malformed("unknown component type")))?;
    let dst = x.map_static(Reg::DataComponentType, src_type);
    let mut value = Vec::new();
    component_value(x, name, r, &mut value)?;
    let dst = dst.ok_or(Xerr::Absent("component missing in the destination release"))?;
    out.put_varint(wire_id(dst));
    out.put_bytes(&value);
    Ok(())
}

/// `ConsumeEffect`, dispatched by its type's name. 26.3 added a Bool to
/// `teleport_randomly`; it defaults to false going up and is dropped going down.
fn consume_effect(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let src_type = read_u32(r)?;
    let src = x.src_proto();
    let name = name_of(Reg::ConsumeEffectType, proto_number(src), src_type)
        .ok_or(Xerr::Wire(WireError::Malformed("unknown consume effect")))?;
    let dst = x
        .map_static(Reg::ConsumeEffectType, src_type)
        .ok_or(Xerr::Absent("consume effect"))?;
    out.put_varint(wire_id(dst));
    match name {
        "minecraft:apply_effects" => {
            let effect = mob_effect_instance();
            x.value(&W::List(effect), r, out)?;
            out.put_bytes(r.bytes(4)?);
        }
        "minecraft:remove_effects" => x.value(&W::HolderSet(R::Static(Reg::MobEffect)), r, out)?,
        "minecraft:clear_all_effects" => {}
        "minecraft:teleport_randomly" => {
            out.put_bytes(r.bytes(4)?);
            let src_has_flag = src == Proto::V777;
            let dst_has_flag = x.dst_proto() == Proto::V777;
            let flag = if src_has_flag { r.bool()? } else { false };
            if dst_has_flag {
                out.put_bool(flag);
            }
        }
        "minecraft:play_sound" => x.value(&SOUND, r, out)?,
        _ => return Err(Xerr::Absent("consume effect of an unknown kind")),
    }
    Ok(())
}

fn proto_number(p: Proto) -> u32 {
    p.number().unsigned_abs()
}

/// `MobEffectInstance`: effect id, then `Details` (amplifier, duration,
/// ambient, particles, icon, optional hidden `Details`, recursively).
fn mob_effect_instance() -> &'static W {
    static DETAILS: W = W::Struct(&[
        W::VarInt,
        W::VarInt,
        W::Bool,
        W::Bool,
        W::Bool,
        W::Opt(&DETAILS),
    ]);
    static INSTANCE: W = W::Struct(&[
        W::Reg(R::Static(Reg::MobEffect)),
        W::Struct(&[
            W::VarInt,
            W::VarInt,
            W::Bool,
            W::Bool,
            W::Bool,
            W::Opt(&DETAILS),
        ]),
    ]);
    &INSTANCE
}

// ---- reshaping -------------------------------------------------------------

/// Converts a component whose layout differs between the two protocols.
fn reshape(
    x: &mut Xc<'_>,
    name: &'static str,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    match x.t.client {
        Proto::V774 => reshape_774(x, name, r, out),
        Proto::V777 => reshape_777(x, name, r, out),
        _ => Err(Xerr::Absent("reshaped component without a converter")),
    }
}

fn reshape_774(
    x: &mut Xc<'_>,
    name: &'static str,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    let layout = component_layout_776(name).ok_or(Xerr::Absent("component without a layout"))?;
    match (name, x.dir) {
        // EitherHolder: Bool (true = holder), then the holder or a resource key.
        (
            "minecraft:jukebox_playable"
            | "minecraft:instrument"
            | "minecraft:provides_trim_material"
            | "minecraft:chicken/variant"
            | "minecraft:zombie_nautilus/variant"
            | "minecraft:damage_type",
            Dir::S2c,
        ) => {
            out.put_bool(true);
            x.value(layout, r, out)
        }
        (
            "minecraft:jukebox_playable"
            | "minecraft:instrument"
            | "minecraft:provides_trim_material"
            | "minecraft:chicken/variant"
            | "minecraft:zombie_nautilus/variant"
            | "minecraft:damage_type",
            Dir::C2s,
        ) => {
            if r.bool()? {
                x.value(layout, r, out)
            } else {
                // A bare resource key: find the entry by name in the
                // registry the backend sent and send its id.
                let key = r.string()?;
                let registry = either_registry(name);
                let id =
                    x.t.conn
                        .dynamic(registry)
                        .and_then(|reg| reg.names.iter().position(|n| n == key))
                        .and_then(|i| u32::try_from(i).ok())
                        .ok_or(Xerr::Absent("resource key unknown to the backend"))?;
                let holder_style = !matches!(
                    name,
                    "minecraft:chicken/variant"
                        | "minecraft:zombie_nautilus/variant"
                        | "minecraft:damage_type"
                );
                out.put_varint(wire_id(id) + i32::from(holder_style));
                Ok(())
            }
        }
        // Tag key (1.21.11) versus holder set (26.1).
        ("minecraft:damage_resistant" | "minecraft:provides_banner_patterns", Dir::S2c) => {
            holder_set_to_tag(r, out)
        }
        ("minecraft:damage_resistant" | "minecraft:provides_banner_patterns", Dir::C2s) => {
            out.put_varint(0);
            out.put_str_bytes(r.str_bytes()?);
            Ok(())
        }
        ("minecraft:blocks_attacks", dir) => {
            // blockDelaySeconds, disableCooldownScale, damage reductions,
            // item damage function (three floats)
            out.put_bytes(r.bytes(8)?);
            x.value(&W::List(&DAMAGE_REDUCTION), r, out)?;
            out.put_bytes(r.bytes(12)?);
            // bypassedBy: Optional<HolderSet> (26.1) / Optional<TagKey> (1.21.11)
            if r.bool()? {
                match dir {
                    Dir::S2c => {
                        let mut tag = Vec::new();
                        match holder_set_to_tag(r, &mut tag) {
                            Ok(()) => {
                                out.put_bool(true);
                                out.put_bytes(&tag);
                            }
                            Err(Xerr::Absent(_)) => out.put_bool(false),
                            Err(e) => return Err(e),
                        }
                    }
                    Dir::C2s => {
                        out.put_bool(true);
                        out.put_varint(0);
                        out.put_str_bytes(r.str_bytes()?);
                    }
                }
            } else {
                out.put_bool(false);
            }
            // block sound, disable sound
            x.value(&W::Opt(&SOUND), r, out)?;
            x.value(&W::Opt(&SOUND), r, out)
        }
        ("minecraft:container", Dir::S2c) => {
            let n = r.len()?;
            out.put_len(n);
            for _ in 0..n {
                if r.bool()? {
                    let mut slot = Vec::new();
                    match template_to_stack(x, r, &mut slot) {
                        Ok(()) => out.put_bytes(&slot),
                        Err(Xerr::Absent(_)) => out.put_varint(0),
                        Err(e) => return Err(e),
                    }
                } else {
                    out.put_varint(0);
                }
            }
            Ok(())
        }
        ("minecraft:container", Dir::C2s) => {
            let n = r.len()?;
            out.put_len(n);
            for _ in 0..n {
                let mut slot = Vec::new();
                match stack_to_template(x, r, &mut slot) {
                    Ok(()) => {
                        out.put_bool(true);
                        out.put_bytes(&slot);
                    }
                    Err(Xerr::Absent(_)) => out.put_bool(false),
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        ("minecraft:bundle_contents" | "minecraft:charged_projectiles", dir) => {
            let n = r.len()?;
            let mut body = Vec::new();
            let mut kept = 0usize;
            for _ in 0..n {
                let mark = body.len();
                let result = match dir {
                    Dir::S2c => template_to_stack(x, r, &mut body),
                    Dir::C2s => stack_to_template(x, r, &mut body),
                };
                match result {
                    Ok(()) => kept += 1,
                    Err(Xerr::Absent(_)) => body.truncate(mark),
                    Err(e) => return Err(e),
                }
            }
            out.put_len(kept);
            out.put_bytes(&body);
            Ok(())
        }
        ("minecraft:use_remainder", Dir::S2c) => template_to_stack(x, r, out),
        ("minecraft:use_remainder", Dir::C2s) => stack_to_template(x, r, out),
        _ => Err(Xerr::Absent("reshaped component without a converter")),
    }
}

/// The synchronised registry an `EitherHolder` component refers to.
fn either_registry(component: &str) -> &'static str {
    match component {
        "minecraft:jukebox_playable" => "minecraft:jukebox_song",
        "minecraft:instrument" => "minecraft:instrument",
        "minecraft:provides_trim_material" => "minecraft:trim_material",
        "minecraft:chicken/variant" => "minecraft:chicken_variant",
        "minecraft:zombie_nautilus/variant" => "minecraft:zombie_nautilus_variant",
        _ => "minecraft:damage_type",
    }
}

/// A holder set that names a tag becomes that tag's key; a direct list has
/// no tag-key form and is `Absent`.
fn holder_set_to_tag(r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let n = r.len()?;
    if n == 0 {
        out.put_str_bytes(r.str_bytes()?);
        return Ok(());
    }
    for _ in 0..n - 1 {
        r.varint()?;
    }
    Err(Xerr::Absent("a direct holder set has no tag-key form"))
}

/// 776 layout of a component by name (the 774 converters read 776 bytes
/// going down, and 26.1-shaped bytes, which 776 shares, going up).
fn component_layout_776(name: &str) -> Option<&'static W> {
    gc::COMPONENTS_776
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, w)| w)
}

fn reshape_777(
    x: &mut Xc<'_>,
    name: &'static str,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
) -> Xresult<()> {
    match name {
        "minecraft:instrument" => instrument_777(x, r, out),
        "minecraft:provides_trim_material" => trim_material_777(x, r, out),
        "minecraft:trim" => {
            trim_material_777(x, r, out)?;
            // The pattern holder kept its layout: take it from the source
            // protocol's `trim` codec (material, pattern).
            let layout =
                component_layout(x, name).ok_or(Xerr::Absent("component without a layout"))?;
            let W::Struct(fields) = x.resolve(layout)? else {
                return Err(Xerr::Absent("trim is not a struct"));
            };
            let pattern = fields
                .get(1)
                .ok_or(Xerr::Absent("trim without a pattern"))?;
            x.value(pattern, r, out)
        }
        "minecraft:pot_decorations" => match x.dir {
            Dir::S2c => {
                // Four item ids (back, left, right, front; brick when empty)
                // become four optional templates of count 1.
                let n = r.len()?;
                let brick_776 = id_of(Reg::Item, 776, "minecraft:brick");
                let mut sides = Vec::with_capacity(4);
                for _ in 0..n {
                    sides.push(read_u32(r)?);
                }
                for i in 0..4 {
                    let id = sides.get(i).copied().or(brick_776);
                    match id.and_then(|id| item(x, id)) {
                        Some(client) => {
                            out.put_bool(true);
                            out.put_varint(wire_id(client));
                            out.put_varint(1);
                            out.put_varint(0);
                            out.put_varint(0);
                        }
                        None => out.put_bool(false),
                    }
                }
                Ok(())
            }
            Dir::C2s => {
                let brick_777 = id_of(Reg::Item, 777, "minecraft:brick");
                let mut ids = Vec::with_capacity(4);
                for _ in 0..4 {
                    let id = if r.bool()? {
                        let id = read_u32(r)?;
                        r.varint()?;
                        let mut sink = Vec::new();
                        patch(x, r, &mut sink, false, Some(id))?;
                        Some(id)
                    } else {
                        brick_777
                    };
                    ids.push(id.and_then(|id| item(x, id)));
                }
                out.put_len(4);
                for id in ids {
                    let id = id.ok_or(Xerr::Absent("pot decoration"))?;
                    out.put_varint(wire_id(id));
                }
                Ok(())
            }
        },
        _ => Err(Xerr::Absent("reshaped component without a converter")),
    }
}

/// `Holder<Instrument>`; 26.3's inline form has `durability_damage` after
/// the range (0 when coming from 26.2, dropped going back).
fn instrument_777(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let raw = read_u32(r)?;
    if raw != 0 {
        let id = x
            .map_id(R::Dynamic("minecraft:instrument"), raw - 1)
            .ok_or(Xerr::Absent("instrument"))?;
        out.put_varint(wire_id(id) + 1);
        return Ok(());
    }
    out.put_varint(0);
    x.value(&SOUND, r, out)?;
    out.put_bytes(r.bytes(8)?);
    match x.dir {
        Dir::S2c => out.put_varint(0),
        Dir::C2s => {
            r.varint()?;
        }
    }
    x.value(&W::Text, r, out)
}

/// `Holder<TrimMaterial>`; 26.3's inline material is a palette id where 26.2
/// had an asset group (base suffix and per-equipment overrides). The palette
/// is `minecraft:trim/<suffix>`; overrides have no 26.3 form and are dropped.
fn trim_material_777(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let raw = read_u32(r)?;
    if raw != 0 {
        let id = x
            .map_id(R::Dynamic("minecraft:trim_material"), raw - 1)
            .ok_or(Xerr::Absent("trim material"))?;
        out.put_varint(wire_id(id) + 1);
        return Ok(());
    }
    out.put_varint(0);
    match x.dir {
        Dir::S2c => {
            let suffix = r.string()?;
            let overrides = r.len()?;
            for _ in 0..overrides {
                r.str_bytes()?;
                r.str_bytes()?;
            }
            out.put_str(&format!("minecraft:trim/{suffix}"));
        }
        Dir::C2s => {
            let palette = r.string()?;
            let suffix = palette.rsplit('/').next().unwrap_or(palette);
            out.put_str(suffix);
            out.put_varint(0);
        }
    }
    x.value(&W::Text, r, out)
}

// ---- hashed stacks ---------------------------------------------------------

/// One added component of a stack: type id, name, value bytes.
struct Entry<'a> {
    ty: u32,
    name: &'static str,
    value: &'a [u8],
}

/// A slot stack read without translating: item id, added components,
/// removed component type ids.
struct RawStack<'a> {
    item: u32,
    added: Vec<Entry<'a>>,
    removed: Vec<u32>,
}

/// A top-level stack's item and patch, read in protocol `proto` without
/// translating (`ItemStack.OPTIONAL_STREAM_CODEC`): `None` for an empty slot.
///
/// `proto` is 776 (the server's bytes) or the client's release (the bytes
/// this translator just wrote for it). 1.21.11 reads with 26.1's tables
/// except for its reshaped components (`RESHAPED_774`), whose values are
/// stepped over with the hand-written 1.21.11 -> 26.2 converter in identity
/// mode: reading them with 26.1's layout would stop inside the value (an
/// `EitherHolder` is one Bool longer, a nested stack is count first) and
/// misread every component after it. Bytes left over after the patch are an
/// error for the same reason: a stack that does not read to its last byte
/// was not understood, and nothing may be recorded or re-encoded from it.
fn read_stack<'a>(
    t: &crate::session::ViaTranslator,
    proto: Proto,
    bytes: &'a [u8],
) -> Xresult<Option<RawStack<'a>>> {
    let mut x = Xc::new(
        t,
        if proto == Proto::V776 {
            Dir::S2c
        } else {
            Dir::C2s
        },
    );
    let mut r = Reader::new(bytes);
    if r.varint()? <= 0 {
        return Ok(None);
    }
    let item = read_u32(&mut r)?;
    let added = r.len()?;
    let removed = r.len()?;
    let mut entries = Vec::with_capacity(added.min(64));
    for _ in 0..added {
        let ty = read_u32(&mut r)?;
        let name = component_name(proto, ty)
            .ok_or(Xerr::Wire(WireError::Malformed("unknown component type")))?;
        let start = r.pos();
        if proto == Proto::V774 && reshaped(Proto::V774, name) {
            let mut sink = Vec::new();
            x.with_identity(|x| reshape_774(x, name, &mut r, &mut sink))?;
        } else {
            let layout = component_layout(&x, name).ok_or(Xerr::Wire(WireError::Malformed(
                "component without a layout",
            )))?;
            skip_value(&mut x, layout, &mut r)?;
        }
        entries.push(Entry {
            ty,
            name,
            value: r.since(start),
        });
    }
    let mut gone = Vec::with_capacity(removed.min(64));
    for _ in 0..removed {
        gone.push(read_u32(&mut r)?);
    }
    if !r.is_empty() {
        return Err(Xerr::Wire(WireError::Malformed(
            "bytes left after a stack's patch",
        )));
    }
    Ok(Some(RawStack {
        item,
        added: entries,
        removed: gone,
    }))
}

/// The record a later claim needs for a slot stack the server sent as
/// `server` (776 bytes) and the client received as `client`, when the
/// translation changed what the client will hash; `None` when the client's
/// claim will hash like the server's stack anyway (see `hashed.rs`).
pub fn sent_record(
    t: &crate::session::ViaTranslator,
    server: &[u8],
    client: &[u8],
) -> Xresult<Option<crate::hashed::Sent>> {
    use crate::hashed::{Claim, Sent};
    let (Some(server), Some(client)) = (
        read_stack(t, Proto::V776, server)?,
        read_stack(t, t.client, client)?,
    ) else {
        return Ok(None);
    };
    let (s_item, s_added, s_removed) = (server.item, server.added, server.removed);
    let (c_item, c_added, c_removed) = (client.item, client.added, client.removed);
    let client_number = t.client.number().unsigned_abs();
    let mut changed = name_of(Reg::Item, 776, s_item) != name_of(Reg::Item, client_number, c_item);
    let x = Xc::new(t, Dir::S2c);
    // The client component ids some server component accounts for: its
    // counterpart, and for 26.3's split swing the interact animation too
    // (a claim's two animations go back as one swing: `hashed_stack`).
    let split_extra = id_of(
        Reg::DataComponentType,
        client_number,
        "minecraft:interact_animation",
    );
    let counterpart_of = |name: &str, ty: u32| {
        if rule(&x, name) == Rule::SplitSwing {
            id_of(
                Reg::DataComponentType,
                client_number,
                "minecraft:attack_animation",
            )
        } else {
            x.map_static(Reg::DataComponentType, ty)
        }
    };
    let mut explained = Vec::with_capacity(s_added.len() + 1);
    let mut server_added = Vec::with_capacity(s_added.len());
    for entry in &s_added {
        // The client component this one became, if any.
        let counterpart =
            counterpart_of(entry.name, entry.ty).filter(|c| c_added.iter().any(|e| e.ty == *c));
        if let Some(c) = counterpart {
            explained.push(c);
            if rule(&x, entry.name) == Rule::SplitSwing {
                explained.extend(split_extra);
            }
        }
        // Values the translation can rewrite in place (text through its
        // hovers, inline holders, block entity data: renumbered types,
        // stand-in items inside, 26.3 sign text taken out).
        let rewritten_value = matches!(
            entry.name,
            "minecraft:custom_name"
                | "minecraft:item_name"
                | "minecraft:lore"
                | "minecraft:instrument"
                | "minecraft:provides_trim_material"
                | "minecraft:trim"
                | "minecraft:block_entity_data"
        ) && counterpart
            .and_then(|id| c_added.iter().find(|value| value.ty == id))
            .is_some_and(|value| value.value != entry.value);
        let same_form = !(rewritten_value
            || t.client == Proto::V777 && entry.name == "minecraft:pot_decorations");
        let claim = match counterpart {
            Some(c) if rewritten_value => {
                match (
                    server_hash(t, entry),
                    c_added
                        .iter()
                        .find(|value| value.ty == c)
                        .map(|value| value_hash(t, t.client, value)),
                ) {
                    // Different bytes, same codec value (1.21.11's
                    // EitherHolder byte, 26.3's durability damage left at
                    // its default, a block entity type renumbered): the
                    // client's hash already is the server's, so nothing
                    // about this component needs answering for it.
                    // ViaVersion compares the hashes in the same way before
                    // it backs anything up (`storeOriginalHashedItemIfNeeded`).
                    (Claim::Computed(server), Some(Claim::Computed(expected)))
                        if server == expected =>
                    {
                        Claim::FromClient(c)
                    }
                    (Claim::Computed(server), Some(Claim::Computed(expected))) => {
                        Claim::Rewritten {
                            client: c,
                            expected,
                            server,
                        }
                    }
                    _ => Claim::Unknown,
                }
            }
            Some(c) if same_form => Claim::FromClient(c),
            _ => server_hash(t, entry),
        };
        changed |= !matches!(claim, Claim::FromClient(_));
        server_added.push((entry.ty, claim));
    }
    // A component the client holds that no server component accounts for
    // (26.3's sign text taken out of block entity data) makes the claims
    // differ, and so does a removal the client cannot claim (a component
    // its release lacks) or one it claims that the server's patch does not
    // hold. 26.3's split swing is neither: its two animations, added or
    // removed, go back as the one swing.
    changed |= c_added.iter().any(|e| !explained.contains(&e.ty));
    let mut removal_explained = Vec::with_capacity(s_removed.len() + 1);
    for ty in &s_removed {
        let name = component_name(Proto::V776, *ty).unwrap_or("");
        match counterpart_of(name, *ty).filter(|c| c_removed.contains(c)) {
            Some(c) => {
                removal_explained.push(c);
                if rule(&x, name) == Rule::SplitSwing {
                    removal_explained.extend(split_extra);
                }
            }
            None => changed = true,
        }
    }
    changed |= c_removed.iter().any(|ty| !removal_explained.contains(ty));
    if !changed {
        return Ok(None);
    }
    let mut client_added: Vec<u32> = c_added.iter().map(|e| e.ty).collect();
    client_added.sort_unstable();
    let mut client_removed = c_removed;
    client_removed.sort_unstable();
    Ok(Some(Sent {
        client_item: c_item,
        client_added,
        client_removed,
        server_item: s_item,
        server_added,
        server_removed: s_removed,
    }))
}

/// The server's `HashOps` hash of a 776 component value, for the components
/// a translation drops or reshapes and whose codec form is simple enough to
/// hash here. Everything else is [`Claim::Unknown`](crate::hashed::Claim).
fn server_hash(t: &crate::session::ViaTranslator, entry: &Entry<'_>) -> crate::hashed::Claim {
    value_hash(t, Proto::V776, entry)
}

fn value_hash(
    t: &crate::session::ViaTranslator,
    proto: Proto,
    entry: &Entry<'_>,
) -> crate::hashed::Claim {
    use crate::hashed::{Claim, as_int, int, list, map, string};
    let mut r = Reader::new(entry.value);
    let hash = (|| -> WireResult<Option<u32>> {
        Ok(match entry.name {
            "minecraft:instrument" | "minecraft:provides_trim_material" | "minecraft:trim" => {
                // 1.21.11 wraps these two component holders in EitherHolder;
                // ArmorTrim's nested material/pattern never has that wrapper.
                if proto == Proto::V774 && entry.name != "minecraft:trim" && !r.bool()? {
                    Some(hash_identifier(r.string()?))
                } else {
                    match entry.name {
                        "minecraft:instrument" => instrument_hash(t, proto, &mut r)?,
                        "minecraft:provides_trim_material" => trim_material_hash(t, proto, &mut r)?,
                        _ => {
                            match (
                                trim_material_hash(t, proto, &mut r)?,
                                trim_pattern_hash(t, proto, &mut r)?,
                            ) {
                                (Some(material), Some(pattern)) => Some(map(vec![
                                    (string("material"), material),
                                    (string("pattern"), pattern),
                                ])),
                                _ => None,
                            }
                        }
                    }
                }
            }
            "minecraft:custom_name" | "minecraft:item_name" => {
                Tag::read(&mut r)?.as_ref().and_then(crate::hashed::text)
            }
            "minecraft:lore" => {
                let count = r.len()?;
                let mut lines = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    let Some(hash) = Tag::read(&mut r)?.as_ref().and_then(crate::hashed::text)
                    else {
                        return Ok(None);
                    };
                    lines.push(hash);
                }
                Some(list(lines))
            }
            "minecraft:custom_data" => Tag::read(&mut r)?.as_ref().and_then(crate::hashed::nbt),
            // MapItemColor: Codec.INT.
            "minecraft:map_color" => Some(int(r.i32()?)),
            // DyeColor: its name.
            "minecraft:dye" => {
                let id = usize::try_from(r.varint()?).map_err(|_| WireError::BadLength)?;
                crate::codec::DYE_COLORS.get(id).map(|n| string(n))
            }
            // Holders of synchronised registries: the entry's name.
            "minecraft:pig/sound_variant"
            | "minecraft:cow/sound_variant"
            | "minecraft:chicken/sound_variant"
            | "minecraft:cat/sound_variant" => {
                let registry = match entry.name {
                    "minecraft:pig/sound_variant" => "minecraft:pig_sound_variant",
                    "minecraft:cow/sound_variant" => "minecraft:cow_sound_variant",
                    "minecraft:chicken/sound_variant" => "minecraft:chicken_sound_variant",
                    _ => "minecraft:cat_sound_variant",
                };
                let id = usize::try_from(r.varint()?).map_err(|_| WireError::BadLength)?;
                t.conn
                    .dynamic(registry)
                    .and_then(|reg| reg.names.get(id))
                    .map(|n| string(n))
            }
            // ItemStackTemplate: {id, count unless 1}; a template with a
            // component patch would need every component's codec.
            "minecraft:sulfur_cube_content" => {
                let item = read_wire_u32(&mut r)?;
                let count = r.varint()?;
                let (added, removed) = (r.varint()?, r.varint()?);
                match name_of(Reg::Item, 776, item) {
                    Some(name) if added == 0 && removed == 0 => {
                        let mut fields = vec![(string("id"), string(name))];
                        if count != 1 {
                            fields.push((string("count"), int(count)));
                        }
                        Some(map(fields))
                    }
                    _ => None,
                }
            }
            // `TypedEntityData`: the saved fields with the type's name put
            // back under `id`, through `CompoundTag.CODEC` (NBT by type).
            "minecraft:block_entity_data" => {
                let ty = read_wire_u32(&mut r)?;
                let Some(kind) = name_of(Reg::BlockEntityType, proto.number().unsigned_abs(), ty)
                else {
                    return Ok(None);
                };
                match Tag::read(&mut r)? {
                    Some(mut tag @ Tag::Compound(_)) => {
                        tag.insert("id", Tag::string(kind));
                        crate::hashed::nbt(&tag)
                    }
                    _ => None,
                }
            }
            // 26.2 pot decorations: `PotDecorations.ordered()`, the four
            // sides' item names with brick for an empty side.
            "minecraft:pot_decorations" => {
                let n = r.len()?;
                let mut names = Vec::with_capacity(4);
                for _ in 0..n {
                    match name_of(Reg::Item, 776, read_wire_u32(&mut r)?) {
                        Some(name) => names.push(string(name)),
                        None => return Ok(None),
                    }
                }
                while names.len() < 4 {
                    names.push(string("minecraft:brick"));
                }
                Some(list(names))
            }
            _ => None,
        })
    })();
    match hash {
        Ok(Some(h)) if r.is_empty() => Claim::Computed(as_int(h)),
        _ => Claim::Unknown,
    }
}

fn hash_identifier(name: &str) -> u32 {
    crate::hashed::string(&crate::codec::qualified(name))
}

/// A holder reference hashes as its registry key, not its protocol id.
/// Client ids may have been compacted during configuration.
fn holder_reference_hash(
    t: &crate::session::ViaTranslator,
    proto: Proto,
    registry: &str,
    raw: u32,
) -> Option<u32> {
    let registry = t.conn.dynamic(registry)?;
    let id = raw.checked_sub(1)?;
    let id = if proto == Proto::V776 {
        id
    } else {
        registry.c2s(id)?
    };
    registry
        .names
        .get(id as usize)
        .map(|name| hash_identifier(name))
}

fn sound_hash(proto: Proto, r: &mut Reader<'_>) -> WireResult<Option<u32>> {
    use crate::hashed::{float, map, string};
    let raw = read_wire_u32(r)?;
    if raw > 0 {
        return Ok(
            name_of(Reg::SoundEvent, proto.number().unsigned_abs(), raw - 1).map(hash_identifier),
        );
    }
    let mut fields = vec![(string("sound_id"), hash_identifier(r.string()?))];
    if r.bool()? {
        fields.push((string("range"), float(r.f32()?)));
    }
    Ok(Some(map(fields)))
}

fn instrument_hash(
    t: &crate::session::ViaTranslator,
    proto: Proto,
    r: &mut Reader<'_>,
) -> WireResult<Option<u32>> {
    use crate::hashed::{float, int, map, string, text};
    let raw = read_wire_u32(r)?;
    if raw > 0 {
        return Ok(holder_reference_hash(t, proto, "minecraft:instrument", raw));
    }
    let sound = sound_hash(proto, r)?;
    let duration = r.f32()?;
    let range = r.f32()?;
    let damage = if proto == Proto::V777 { r.varint()? } else { 0 };
    let description = Tag::read(r)?.as_ref().and_then(text);
    let (Some(sound), Some(description)) = (sound, description) else {
        return Ok(None);
    };
    let mut fields = vec![
        (string("sound_event"), sound),
        (string("use_duration"), float(duration)),
        (string("range"), float(range)),
        (string("description"), description),
    ];
    if damage != 0 {
        fields.push((string("durability_damage"), int(damage)));
    }
    Ok(Some(map(fields)))
}

fn trim_material_hash(
    t: &crate::session::ViaTranslator,
    proto: Proto,
    r: &mut Reader<'_>,
) -> WireResult<Option<u32>> {
    use crate::hashed::{map, string, text};
    let raw = read_wire_u32(r)?;
    if raw > 0 {
        return Ok(holder_reference_hash(
            t,
            proto,
            "minecraft:trim_material",
            raw,
        ));
    }
    let mut fields = Vec::new();
    if proto == Proto::V777 {
        fields.push((string("palette_id"), hash_identifier(r.string()?)));
    } else {
        fields.push((string("asset_name"), string(r.string()?)));
        let count = r.len()?;
        let mut overrides = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            overrides.push((hash_identifier(r.string()?), string(r.string()?)));
        }
        if !overrides.is_empty() {
            fields.push((string("override_armor_assets"), map(overrides)));
        }
    }
    let Some(description) = Tag::read(r)?.as_ref().and_then(text) else {
        return Ok(None);
    };
    fields.push((string("description"), description));
    Ok(Some(map(fields)))
}

fn trim_pattern_hash(
    t: &crate::session::ViaTranslator,
    proto: Proto,
    r: &mut Reader<'_>,
) -> WireResult<Option<u32>> {
    use crate::hashed::{boolean, map, string, text};
    let raw = read_wire_u32(r)?;
    if raw > 0 {
        return Ok(holder_reference_hash(
            t,
            proto,
            "minecraft:trim_pattern",
            raw,
        ));
    }
    let asset = hash_identifier(r.string()?);
    let description = Tag::read(r)?.as_ref().and_then(text);
    let decal = boolean(r.bool()?);
    Ok(description.map(|description| {
        map(vec![
            (string("asset_id"), asset),
            (string("description"), description),
            (string("decal"), decal),
        ])
    }))
}

fn read_wire_u32(r: &mut Reader<'_>) -> WireResult<u32> {
    u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative id"))
}

/// `HashedStack` (serverbound container clicks): `Bool present; VarInt item,
/// VarInt count, Map<VarInt type, i32 hash> added, List<VarInt type>
/// removed`.
///
/// A claim about a stack the translation changed (`hashed.rs`), found by
/// the hash of the `custom_data` it was sent with, becomes the claim about
/// the server's stack: its item, its component ids, and for each component
/// the client's hash (same codec form) or the hash computed when the stack
/// was sent; the backup in `custom_data` never reaches the server. Any
/// other claim has its ids renumbered and keeps the client's hashes, which
/// match whenever the codec forms agree; 26.3's attack and interact
/// animations are claimed as 26.2's swing animation (same codec, same
/// hash).
pub fn hashed_stack(
    x: &mut Xc<'_>,
    r: &mut Reader<'_>,
    out: &mut Vec<u8>,
    answered: &mut Vec<crate::hashed::RecordKey>,
) -> Xresult<()> {
    use crate::hashed::Claim;
    if !r.bool()? {
        out.put_bool(false);
        return Ok(());
    }
    let id = read_u32(r)?;
    let count = r.varint()?;
    let added = r.len()?;
    let mut claims = Vec::with_capacity(added.min(64));
    for _ in 0..added {
        let ty = read_u32(r)?;
        let hash = r.i32()?;
        claims.push((ty, hash));
    }
    let removed = r.len()?;
    let mut removed_types = Vec::with_capacity(removed.min(64));
    for _ in 0..removed {
        removed_types.push(read_u32(r)?);
    }

    let mut added_types: Vec<u32> = claims.iter().map(|(ty, _)| *ty).collect();
    added_types.sort_unstable();
    let mut removed_sorted = removed_types.clone();
    removed_sorted.sort_unstable();
    // The record is found by the claimed custom_data hash: a stack sent
    // with a backup carries it there (`back_up_hashes`), and nothing else
    // can tell two server stacks apart that the client holds identically.
    let custom_data = id_of(
        Reg::DataComponentType,
        x.t.client.number().unsigned_abs(),
        "minecraft:custom_data",
    );
    let record = claims
        .iter()
        .find(|(ty, _)| Some(*ty) == custom_data)
        .and_then(|(_, hash)| {
            let key = crate::hashed::RecordKey::of_claim(*hash, id, &added_types, &removed_sorted);
            x.t.conn
                .sent_stacks
                .find(&key, &claims)
                .map(|sent| (key, sent))
        });
    if let Some((key, sent)) = record {
        answered.push(key);
        out.put_bool(true);
        out.put_varint(wire_id(sent.server_item));
        out.put_varint(count);
        out.put_len(sent.server_added.len());
        for (ty, claim) in &sent.server_added {
            let hash = match claim {
                Claim::FromClient(c) => claims
                    .iter()
                    .find(|(ty, _)| ty == c)
                    .map_or(0, |(_, hash)| *hash),
                Claim::Computed(hash) => *hash,
                Claim::Rewritten { server, .. } => *server,
                Claim::Unknown => 0,
            };
            out.put_varint(wire_id(*ty));
            out.put_i32(hash);
        }
        out.put_len(sent.server_removed.len());
        for ty in &sent.server_removed {
            out.put_varint(wire_id(*ty));
        }
        return Ok(());
    }

    let mut body = Vec::new();
    let mut kept = Vec::with_capacity(claims.len());
    for (ty, hash) in claims {
        let name = component_name(x.src_proto(), ty).unwrap_or("");
        let mapped = if rule(x, name) == Rule::MergeSwing {
            dst_component(x, "minecraft:swing_animation")
        } else {
            x.map_static(Reg::DataComponentType, ty)
        };
        if let Some(mapped) = mapped
            && !kept.contains(&mapped)
        {
            kept.push(mapped);
            body.put_varint(wire_id(mapped));
            body.put_i32(hash);
        }
    }
    let mut removed_ids = Vec::new();
    for ty in removed_types {
        let name = component_name(x.src_proto(), ty).unwrap_or("");
        let mapped = if rule(x, name) == Rule::MergeSwing {
            dst_component(x, "minecraft:swing_animation")
        } else {
            x.map_static(Reg::DataComponentType, ty)
        };
        if let Some(mapped) = mapped
            && !removed_ids.contains(&mapped)
        {
            removed_ids.push(mapped);
        }
    }
    match item(x, id) {
        Some(id) => {
            out.put_bool(true);
            out.put_varint(wire_id(id));
            out.put_varint(count);
            out.put_len(kept.len());
            out.put_bytes(&body);
            out.put_len(removed_ids.len());
            for id in removed_ids {
                out.put_varint(wire_id(id));
            }
        }
        None => {
            x.notes
                .push("a claim about an item with no equivalent was sent as an empty slot");
            out.put_bool(false);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod hash_tests {
    use super::*;

    // Bytes and hashes emitted by the official 26.2/26.3 jars using the
    // existing ItemData.java generator (26.3 constructor shapes adapted).
    #[test]
    fn inline_and_unicode_components_match_official_776_and_777_vectors() {
        let translator = crate::session::ViaTranslator::new(Proto::V777, 1);
        for (proto, name, hex, expected) in [
            (
                Proto::V776,
                "minecraft:custom_name",
                "08000f4d757369636120eda0b4edb49ec080",
                -1487372295,
            ),
            (
                Proto::V776,
                "minecraft:custom_data",
                "0a08000c636869617665eda0b4edb49e000e76616c6f7265c080eda0b4edb49e00",
                -714588058,
            ),
            (
                Proto::V776,
                "minecraft:instrument",
                "00000c656c797a756e653a686f726e0040e0000043800000080005436f726e6f",
                -140489936,
            ),
            (
                Proto::V776,
                "minecraft:instrument",
                "00000c656c797a756e653a686f726e01430000003fc00000420000000a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000",
                -221739880,
            ),
            (
                Proto::V776,
                "minecraft:provides_trim_material",
                "00076d69746872696c000800074d69746872696c",
                -219312053,
            ),
            (
                Proto::V776,
                "minecraft:provides_trim_material",
                "00076d69746872696c010e6d696e6563726166743a69726f6e0e6d69746872696c5f6461726b65720a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000",
                1643090981,
            ),
            (
                Proto::V776,
                "minecraft:trim",
                "00076d69746872696c000800074d69746872696c000d656c797a756e653a7370697265080005537069726500",
                820957339,
            ),
            (
                Proto::V776,
                "minecraft:trim",
                "00076d69746872696c010e6d696e6563726166743a69726f6e0e6d69746872696c5f6461726b65720a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000000d656c797a756e653a73706972650a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c6963000001",
                1287871806,
            ),
            (
                Proto::V777,
                "minecraft:custom_name",
                "08000f4d757369636120eda0b4edb49ec080",
                -1487372295,
            ),
            (
                Proto::V777,
                "minecraft:custom_data",
                "0a08000c636869617665eda0b4edb49e000e76616c6f7265c080eda0b4edb49e00",
                -714588058,
            ),
            (
                Proto::V777,
                "minecraft:instrument",
                "00000c656c797a756e653a686f726e0040e000004380000000080005436f726e6f",
                -140489936,
            ),
            (
                Proto::V777,
                "minecraft:instrument",
                "00000c656c797a756e653a686f726e01430000003fc0000042000000000a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000",
                -221739880,
            ),
            (
                Proto::V777,
                "minecraft:provides_trim_material",
                "00166d696e6563726166743a7472696d2f6d69746872696c0800074d69746872696c",
                -342855194,
            ),
            (
                Proto::V777,
                "minecraft:provides_trim_material",
                "00166d696e6563726166743a7472696d2f6d69746872696c0a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000",
                -1168115561,
            ),
            (
                Proto::V777,
                "minecraft:trim",
                "00166d696e6563726166743a7472696d2f6d69746872696c0800074d69746872696c000d656c797a756e653a7370697265080005537069726500",
                1334830686,
            ),
            (
                Proto::V777,
                "minecraft:trim",
                "00166d696e6563726166743a7472696d2f6d69746872696c0a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000000d656c797a756e653a73706972650a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c6963000001",
                -1089483449,
            ),
        ] {
            let bytes: Vec<_> = (0..hex.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
                .collect();
            let entry = Entry {
                ty: 0,
                name,
                value: &bytes,
            };
            assert_eq!(
                value_hash(&translator, proto, &entry),
                crate::hashed::Claim::Computed(expected),
                "{proto:?} {name}"
            );
        }
    }
}
