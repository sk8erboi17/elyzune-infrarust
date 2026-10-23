//! Hashed stacks: what a client claims about a slot after a click.
//!
//! # The problem
//!
//! Since 1.21.5 a container click carries, for every slot it changed and for
//! the cursor, a `HashedStack`: the item id, the count, and per component of
//! the stack's patch a CRC32C of the component's *codec-form* value
//! (`HashOps`). The server compares the claims with its own stacks and
//! resends only the slots that differ.
//!
//! The translator renumbers item and component ids, which is enough while
//! the client's copy of a stack hashes like the server's. It does not when
//! the translation changed the stack:
//!
//! - the item has no equivalent and is shown as a stand-in (26.2 cinnabar
//!   bricks as cut red sandstone before 26.2);
//! - a component the client's release lacks was removed (26.2 sulfur cube
//!   content, 26.1 dye and sound variants before 26.1, map colour in 26.3);
//! - a component's codec form changed (26.3 pot decorations) or its identity
//!   did (26.2 swing animation is 26.3's attack and interact animations).
//!
//! Each such click would then be answered with a resend of every touched
//! slot: correct, but a visible flicker on every click.
//!
//! # The fix
//!
//! When a stack goes to the client changed in one of those ways, the
//! translator records it ([`Sent`]): the client's item and component ids, and
//! for each of the server's components how to obtain its hash: copied from
//! the client's claim (same codec form, so the same hash) or computed here
//! from the server's value. A claim matches the client item, component ids
//! and the expected hashes of rewritten values. Unknown or wrong claims
//! keep the client's numbers: the server resends that slot, as it would
//! have without the record.
//!
//! "Changed" is decided on the hashes, as ViaVersion does before it backs
//! anything up (`storeOriginalHashedItemIfNeeded`): a value whose bytes
//! changed but whose codec value did not (1.21.11's `EitherHolder` byte, a
//! renumbered block entity type, 26.3's instrument without durability
//! damage) is claimed with the client's hash, which is the server's, and
//! 26.3's split swing needs nothing either: its two animations are claimed
//! as the one swing (`item::hashed_stack`). A stack with nothing else
//! changed is neither recorded nor given a backup.
//!
//! # Telling apart stacks the client sees as one
//!
//! A translation can make two different server stacks identical on the
//! client: a component the client lacks is dropped (two papers whose
//! sulfur cube content differs, for 26.1), a stand-in looks like the item
//! it stands in for (cinnabar bricks next to real cut red sandstone, for
//! 1.21.11), two hovers become the same entity. Their claims are then
//! identical too, and no search over the records can say which server
//! stack a claim is about: picking the most recent one answers a click on
//! the other slot with a resend, every time, back and forth.
//!
//! ViaVersion's answer (`StructuredItemRewriter.storeOriginalHashedItemInTag`
//! and `ItemHasherBase`, 5.12.0) is ported here: the client's copy of such a
//! stack carries, in its `custom_data`, the server-side hashes the client
//! cannot reproduce ([`ORIGINAL_HASHES_KEY`], see [`original_hashes`]). Two
//! server stacks that differ in anything the client cannot see then differ
//! in `custom_data`, so the client's claim carries a `custom_data` hash
//! unique to the server stack, and that hash, with the client's item and
//! component ids the claim repeats, is the key of the record
//! ([`RecordKey`]; [`SentStacks`]: [`MAX_SENT`] records, least recently
//! used first, as ViaVersion's cache). The client keeps the key when it
//! moves or splits the stack, because it copies the components. The
//! serverbound direction removes the key again before anything reaches the
//! server (the creative slot, `item::patch`), and a creative stack sent back
//! as a stand-in gets its server item back from the backup, as ViaBackwards
//! restores the id it backs up (`item::untrusted_stack`).
//!
//! Where this differs from ViaVersion, on purpose: the key is the
//! translator's own (`elyzune-via|...`, never `VV|...`), so a client-side
//! ViaVersion (ViaFabricPlus) can never mistake it for its own backup; the
//! backup lists only the hashes the client cannot reproduce (components the
//! client sees in the same codec form are claimed with the client's own
//! hash, which ViaVersion replaces with the server's and so can hide a
//! client that holds something else); and a stand-in item without
//! components is recorded too, which ViaVersion leaves to a resend.
//!
//! The records are cleared on a reconfiguration, when every id they hold
//! goes stale.
//!
//! # HashOps
//!
//! The primitives below are `net.minecraft.util.HashOps` over Guava's
//! CRC32C, as Hyperion's `hyperion-minecraft-proto::item::hash` implements
//! them for the server side (and pins with vectors from the 26.2 jar):
//!
//! ```text
//! byte    6, value               short   7, value (LE)
//! int     8, value (LE)          long    9, value (LE)
//! float  10, bits (LE)           double 11, bits (LE)
//! string 12, UTF-16 length (LE i32), units (LE)
//! list    4, element hashes, 5   map     2, (key, value) hashes sorted unsigned, 3
//! byte[] 14, bytes, 15           int[]  16, values (LE), 17   long[] 18, values (LE), 19
//! ```
//!
//! An NBT value (`block_entity_data`, `custom_data`) is hashed as
//! `NbtOps.convertTo` hands it to `HashOps`: by NBT type ([`nbt`]).

use std::collections::{HashMap, VecDeque};

/// Upper bound on recorded stacks per connection: ViaVersion's
/// `ItemHasherBase` cache size. Records are keyed by distinct server stacks
/// (identical stacks share one), so this is far above what a player holds;
/// the oldest record goes first, and a claim about a stack whose record was
/// dropped costs one resend, which records it again.
pub const MAX_SENT: usize = 1024;

/// The `custom_data` key holding a translated stack's server-side hashes.
pub const ORIGINAL_HASHES_KEY: &str = "elyzune-via|original_hashes";

/// Marks a `custom_data` the translator created: the server's stack had
/// none, so the serverbound direction removes the whole component again
/// when nothing else is left in it (ViaVersion's `VV|custom_data`).
pub const CREATED_KEY: &str = "elyzune-via|custom_data";

/// How the server's hash of one component is obtained for a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Same codec form on both sides: the client's hash of its component
    /// with this (client) type id.
    FromClient(u32),
    /// The server's hash, computed from its value when the stack was sent.
    Computed(i32),
    /// A value rewritten for the client: substitute the server hash only
    /// when the client claims the value it was actually sent. Otherwise a
    /// stale or wrong claim must still trigger the backend's correction.
    Rewritten {
        client: u32,
        expected: i32,
        server: i32,
    },
    /// Not computable here; the claim cannot match this component.
    Unknown,
}

/// A stack sent to the client changed in a hash-relevant way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub client_item: u32,
    /// Client component type ids of the patch's added components, sorted.
    pub client_added: Vec<u32>,
    /// Client component type ids of the patch's removed components, sorted.
    pub client_removed: Vec<u32>,
    pub server_item: u32,
    /// Server component type id and how to obtain its hash, per added
    /// component of the server's patch.
    pub server_added: Vec<(u32, Claim)>,
    pub server_removed: Vec<u32>,
}

impl Sent {
    /// Whether a claim can be answered from this record at all: every
    /// server component's hash is obtainable. A record with an
    /// [`Claim::Unknown`] can never match, so it is neither recorded nor
    /// given a backup.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.server_added
            .iter()
            .all(|(_, claim)| *claim != Claim::Unknown)
    }

    /// Whether a claim about `item` with these sorted component ids, and
    /// these (type, hash) claims, describes the stack this record was made
    /// for. A rewritten value must be claimed as it was sent: a stale or
    /// wrong claim must still reach the server as a mismatch.
    fn admits(&self, item: u32, added: &[u32], removed: &[u32], claims: &[(u32, i32)]) -> bool {
        self.client_item == item
            && self.client_added == added
            && self.client_removed == removed
            && self.server_added.iter().all(|(_, claim)| match claim {
                Claim::Unknown => false,
                Claim::Rewritten {
                    client, expected, ..
                } => claims
                    .iter()
                    .any(|(ty, hash)| ty == client && hash == expected),
                _ => true,
            })
    }
}

/// The `custom_data` entry a recorded stack carries to the client: the
/// server's item id, the server-side hashes the client cannot reproduce
/// (computed values and the server's side of rewritten ones), by server
/// component type id, and the server's removed component ids. ViaVersion's
/// `VV|original_hashes` layout, without its `backup_tag` (which chains
/// protocols; there is one translation here).
///
/// Two records that would answer a claim differently differ here (or in the
/// client's own view of the stack), which is what makes the `custom_data`
/// hash a key; two that agree everywhere produce the same entry and share
/// one record, correctly.
#[must_use]
pub fn original_hashes(sent: &Sent) -> crate::nbt::Tag {
    use crate::nbt::Tag;
    let mut entries = vec![(
        b"id".to_vec(),
        Tag::Int(crate::wire::wire_id(sent.server_item)),
    )];
    for (ty, claim) in &sent.server_added {
        let hash = match claim {
            Claim::Computed(hash) | Claim::Rewritten { server: hash, .. } => *hash,
            Claim::FromClient(_) | Claim::Unknown => continue,
        };
        entries.push((ty.to_string().into_bytes(), Tag::Int(hash)));
    }
    entries.push((
        b"removed".to_vec(),
        Tag::IntArray(
            sent.server_removed
                .iter()
                .map(|ty| crate::wire::wire_id(*ty))
                .collect(),
        ),
    ));
    Tag::Compound(entries)
}

/// What [`strip_backup`] left of a serverbound `custom_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stripped {
    /// False when the component goes too: the translator created it and
    /// nothing else is left in it.
    pub keep: bool,
    /// The server's item id the backup recorded, if it held a valid one.
    pub server_item: Option<u32>,
}

/// Removes the translator's backup from a serverbound `custom_data`
/// compound (ViaVersion's `restoreBackupData` + `removeCustomTag`), and
/// reads the server item id it recorded, which ViaBackwards also restores
/// from its own backup (`BackwardsStructuredItemRewriter.restoreBackupData`).
pub fn strip_backup(custom_data: &mut crate::nbt::Tag) -> Stripped {
    use crate::nbt::Tag;
    let server_item = match custom_data.remove(ORIGINAL_HASHES_KEY) {
        Some(backup) => match backup.get("id") {
            Some(Tag::Int(id)) => u32::try_from(*id).ok(),
            _ => None,
        },
        None => None,
    };
    let created = custom_data.remove(CREATED_KEY).is_some();
    Stripped {
        keep: !(created && matches!(custom_data, Tag::Compound(entries) if entries.is_empty())),
        server_item,
    }
}

/// What a record is found by: the hash of the `custom_data` the client was
/// sent with the stack, and the client's own view of the stack's shape (its
/// item and sorted component ids), all of which its claims repeat.
///
/// The hash alone is not enough. The backup lists only the hashes the
/// client cannot reproduce, so two server stacks that share those (the same
/// rewritten name) but differ in components the client sees as they are (a
/// lore on one of them) carry the same backup: keyed by the hash alone, the
/// later record replaced the earlier one, and clicks on the earlier stack
/// were answered with a resend. Records that share the whole key are
/// interchangeable: same server item, same server hashes for what the
/// client cannot see, and the client's own hashes for the rest, read from
/// the claim.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordKey {
    custom_data: i32,
    item: u32,
    added: Vec<u32>,
    removed: Vec<u32>,
}

impl RecordKey {
    /// The key of a claim: its `custom_data` hash, item and sorted ids.
    #[must_use]
    pub fn of_claim(custom_data: i32, item: u32, added: &[u32], removed: &[u32]) -> Self {
        Self {
            custom_data,
            item,
            added: added.to_vec(),
            removed: removed.to_vec(),
        }
    }

    fn of_record(custom_data: i32, sent: &Sent) -> Self {
        Self::of_claim(
            custom_data,
            sent.client_item,
            &sent.client_added,
            &sent.client_removed,
        )
    }
}

/// The recorded [`Sent`] stacks of one connection, by [`RecordKey`] (see the
/// module docs), in the order they were last recorded or answered a claim.
#[derive(Debug, Default)]
pub struct SentStacks {
    by_key: HashMap<RecordKey, Sent>,
    order: VecDeque<RecordKey>,
}

impl SentStacks {
    /// Records `sent`, which the client was sent with a `custom_data`
    /// hashing to `custom_data`, replacing what its key held (the same
    /// server stack sent again) and evicting the least recently used record
    /// past [`MAX_SENT`].
    pub fn record(&mut self, custom_data: i32, sent: Sent) {
        let key = RecordKey::of_record(custom_data, &sent);
        if self.by_key.insert(key.clone(), sent).is_some()
            && let Some(index) = self.order.iter().position(|k| *k == key)
        {
            self.order.remove(index);
        }
        self.order.push_back(key);
        while self.order.len() > MAX_SENT {
            if let Some(oldest) = self.order.pop_front() {
                self.by_key.remove(&oldest);
            }
        }
    }

    /// The record a claim with this key is about, when the claim also
    /// carries the hashes of the values rewritten for the client as they
    /// were sent. The caller [`touch`](Self::touch)es a record that answered
    /// a claim once the packet is translated.
    #[must_use]
    pub fn find(&self, key: &RecordKey, claims: &[(u32, i32)]) -> Option<&Sent> {
        self.by_key
            .get(key)
            .filter(|sent| sent.admits(key.item, &key.added, &key.removed, claims))
    }

    /// Makes the record under `key` the most recent one, as a read does in
    /// ViaVersion's cache (Guava's size bound evicts the least recently
    /// used entry): a stack the player keeps clicking without the server
    /// sending it again is not the one evicted.
    pub fn touch(&mut self, key: &RecordKey) {
        if let Some(index) = self.order.iter().position(|k| k == key) {
            self.order.remove(index);
            self.order.push_back(key.clone());
        }
    }

    pub fn clear(&mut self) {
        self.by_key.clear();
        self.order.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

// ---- HashOps ------------------------------------------------------------------

/// CRC-32C (Castagnoli), reflected polynomial `0x82F63B78`, initial and final
/// XOR `!0`: Guava's `Hashing.crc32c()`.
const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0_u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

#[derive(Clone, Copy)]
struct Crc(u32);

impl Crc {
    const fn new() -> Self {
        Self(!0)
    }

    fn bytes(mut self, bytes: &[u8]) -> Self {
        for &byte in bytes {
            self.0 = CRC32C_TABLE[((self.0 ^ u32::from(byte)) & 0xFF) as usize] ^ (self.0 >> 8);
        }
        self
    }

    const fn finish(self) -> u32 {
        !self.0
    }
}

/// `HashOps.createInt`.
#[must_use]
pub fn int(value: i32) -> u32 {
    Crc::new().bytes(&[8]).bytes(&value.to_le_bytes()).finish()
}

/// `HashOps.createString`: length and units in UTF-16, as Java counts them.
#[must_use]
pub fn string(value: &str) -> u32 {
    let units = i32::try_from(value.encode_utf16().count()).unwrap_or(i32::MAX);
    let mut crc = Crc::new().bytes(&[12]).bytes(&units.to_le_bytes());
    for unit in value.encode_utf16() {
        crc = crc.bytes(&unit.to_le_bytes());
    }
    crc.finish()
}

/// NBT strings contain Java UTF-16 units in modified UTF-8. Hash those
/// units directly: supplementary characters are two three-byte surrogates,
/// and NUL is C0 80. Requiring Rust UTF-8 would refuse both valid values.
fn mutf8_string(bytes: &[u8]) -> Option<u32> {
    if bytes.is_ascii() {
        return Some(string(std::str::from_utf8(bytes).ok()?));
    }
    let mut units = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let first = bytes[at];
        let (unit, width) = match first {
            0..=0x7f => (u16::from(first), 1),
            0xc0..=0xdf => {
                let second = *bytes.get(at + 1)?;
                if second & 0xc0 != 0x80 {
                    return None;
                }
                ((u16::from(first & 0x1f) << 6) | u16::from(second & 0x3f), 2)
            }
            0xe0..=0xef => {
                let second = *bytes.get(at + 1)?;
                let third = *bytes.get(at + 2)?;
                if second & 0xc0 != 0x80 || third & 0xc0 != 0x80 {
                    return None;
                }
                (
                    (u16::from(first & 0x0f) << 12)
                        | (u16::from(second & 0x3f) << 6)
                        | u16::from(third & 0x3f),
                    3,
                )
            }
            _ => return None,
        };
        units.push(unit);
        at += width;
    }
    let count = i32::try_from(units.len()).ok()?;
    let mut crc = Crc::new().bytes(&[12]).bytes(&count.to_le_bytes());
    for unit in units {
        crc = crc.bytes(&unit.to_le_bytes());
    }
    Some(crc.finish())
}

fn string_tag(tag: &crate::nbt::Tag) -> Option<u32> {
    match tag {
        crate::nbt::Tag::String(bytes) => mutf8_string(bytes),
        _ => None,
    }
}

/// `HashOps.createByte`.
#[must_use]
pub fn byte(value: i8) -> u32 {
    Crc::new().bytes(&[6]).bytes(&value.to_le_bytes()).finish()
}

/// `HashOps.createShort`.
#[must_use]
pub fn short(value: i16) -> u32 {
    Crc::new().bytes(&[7]).bytes(&value.to_le_bytes()).finish()
}

/// `HashOps.createLong`.
#[must_use]
pub fn long(value: i64) -> u32 {
    Crc::new().bytes(&[9]).bytes(&value.to_le_bytes()).finish()
}

/// `HashOps.createFloat`: the IEEE bits, as Guava's `putFloat`.
#[must_use]
pub fn float(value: f32) -> u32 {
    Crc::new().bytes(&[10]).bytes(&value.to_le_bytes()).finish()
}

/// `HashOps.createDouble`.
#[must_use]
pub fn double(value: f64) -> u32 {
    Crc::new().bytes(&[11]).bytes(&value.to_le_bytes()).finish()
}

#[must_use]
pub fn boolean(value: bool) -> u32 {
    Crc::new().bytes(&[13, u8::from(value)]).finish()
}

/// `HashOps.createByteList`.
#[must_use]
pub fn byte_list(values: &[u8]) -> u32 {
    Crc::new().bytes(&[14]).bytes(values).bytes(&[15]).finish()
}

/// `HashOps.createIntList`.
#[must_use]
pub fn int_list(values: &[i32]) -> u32 {
    let mut crc = Crc::new().bytes(&[16]);
    for value in values {
        crc = crc.bytes(&value.to_le_bytes());
    }
    crc.bytes(&[17]).finish()
}

/// `HashOps.createLongList`.
#[must_use]
pub fn long_list(values: &[i64]) -> u32 {
    let mut crc = Crc::new().bytes(&[18]);
    for value in values {
        crc = crc.bytes(&value.to_le_bytes());
    }
    crc.bytes(&[19]).finish()
}

/// An NBT value as `NbtOps.convertTo` hands it to `HashOps` (the path of
/// `CompoundTag.CODEC`, which `CustomData` and `TypedEntityData` encode
/// through): by NBT type, so a byte stays a byte where the Java side meant a
/// boolean, since the client hashes the tag it decoded. Malformed modified
/// UTF-8 fails closed; valid NUL and surrogate pairs keep their UTF-16 units.
#[must_use]
pub fn nbt(tag: &crate::nbt::Tag) -> Option<u32> {
    use crate::nbt::Tag;
    Some(match tag {
        Tag::Byte(value) => byte(*value),
        Tag::Short(value) => short(*value),
        Tag::Int(value) => int(*value),
        Tag::Long(value) => long(*value),
        Tag::Float(value) => float(*value),
        Tag::Double(value) => double(*value),
        Tag::ByteArray(bytes) => byte_list(bytes),
        Tag::String(bytes) => mutf8_string(bytes)?,
        Tag::List(_, elements) => {
            let mut hashes = Vec::with_capacity(elements.len());
            for element in elements {
                hashes.push(nbt(element)?);
            }
            list(hashes)
        }
        Tag::Compound(entries) => {
            let mut hashes = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                hashes.push((mutf8_string(key)?, nbt(value)?));
            }
            map(hashes)
        }
        Tag::IntArray(values) => int_list(values),
        Tag::LongArray(values) => long_list(values),
    })
}

/// A text component through its persistent codec. The shape is already
/// NBT, but style flags are BOOL in the codec, not NBT bytes. Unmodelled
/// contents or nested component codecs return None rather than invent a
/// hash. The budget also covers text reached through item hover patches.
pub fn text(tag: &crate::nbt::Tag) -> Option<u32> {
    text_at(tag, 64)
}

fn identifier(value: &str) -> u32 {
    string(&crate::codec::qualified(value))
}

fn int_tag(tag: &crate::nbt::Tag) -> Option<i32> {
    use crate::nbt::Tag;
    match *tag {
        Tag::Byte(value) => Some(i32::from(value)),
        Tag::Short(value) => Some(i32::from(value)),
        Tag::Int(value) => Some(value),
        Tag::Long(value) => i32::try_from(value).ok(),
        _ => None,
    }
}

fn text_at(tag: &crate::nbt::Tag, depth: usize) -> Option<u32> {
    use crate::nbt::Tag;
    let depth = depth.checked_sub(1)?;
    if matches!(tag, Tag::String(_)) {
        return string_tag(tag);
    }
    let Tag::Compound(entries) = tag else {
        return None;
    };
    if !["text", "translate", "keybind"]
        .iter()
        .any(|key| tag.get(key).is_some())
    {
        return None;
    }
    let mut fields = Vec::new();
    for (key, value) in entries {
        let key = std::str::from_utf8(key).ok()?;
        let hash = match key {
            "text" | "translate" | "fallback" | "keybind" | "insertion" => string_tag(value)?,
            "font" => identifier(value.as_str()?),
            "color" => {
                let color = value.as_str()?;
                if let Some(rgb) = color.strip_prefix('#') {
                    string(&format!("#{:06X}", u32::from_str_radix(rgb, 16).ok()?))
                } else {
                    string(color)
                }
            }
            "shadow_color" => int(int_tag(value)?),
            "bold" | "italic" | "underlined" | "strikethrough" | "obfuscated" => {
                boolean(int_tag(value)? != 0)
            }
            "extra" | "with" => {
                let Tag::List(_, values) = value else {
                    return None;
                };
                if values.is_empty() {
                    continue;
                }
                let mut hashes = Vec::with_capacity(values.len());
                for value in values {
                    // ListTag wraps mixed primitive entries in {"": value}.
                    let value = match value {
                        Tag::Compound(fields) if fields.len() == 1 && fields[0].0.is_empty() => {
                            &fields[0].1
                        }
                        _ => value,
                    };
                    hashes.push(
                        if key == "with" && !matches!(value, Tag::Compound(_) | Tag::List(..)) {
                            nbt(value)?
                        } else {
                            text_at(value, depth)?
                        },
                    );
                }
                list(hashes)
            }
            "click_event" => click(value)?,
            "hover_event" => hover(value, depth)?,
            "type" => continue,
            _ => return None,
        };
        fields.push((string(key), hash));
    }
    if fields.len() == 1 && fields[0].0 == string("text") {
        return Some(fields[0].1);
    }
    Some(map(fields))
}

fn click(tag: &crate::nbt::Tag) -> Option<u32> {
    let action = tag.get("action")?.as_str()?;
    let mut fields = vec![(string("action"), string(action))];
    let (key, hash) = match action {
        "open_url" => ("url", string_tag(tag.get("url")?)?),
        "run_command" | "suggest_command" => ("command", string_tag(tag.get("command")?)?),
        "change_page" => ("page", int(int_tag(tag.get("page")?)?)),
        "copy_to_clipboard" => ("value", string_tag(tag.get("value")?)?),
        "show_dialog" => ("dialog", identifier(tag.get("dialog")?.as_str()?)),
        "custom" => {
            if let Some(payload) = tag.get("payload") {
                fields.push((string("payload"), nbt(payload)?));
            }
            ("id", identifier(tag.get("id")?.as_str()?))
        }
        _ => return None,
    };
    fields.push((string(key), hash));
    Some(map(fields))
}

fn hover(tag: &crate::nbt::Tag, depth: usize) -> Option<u32> {
    use crate::nbt::Tag;
    let action = tag.get("action")?.as_str()?;
    let mut fields = vec![(string("action"), string(action))];
    match action {
        "show_text" => fields.push((string("value"), text_at(tag.get("value")?, depth)?)),
        "show_entity" => {
            fields.push((string("id"), identifier(tag.get("id")?.as_str()?)));
            let uuid = tag.get("uuid")?;
            fields.push((
                string("uuid"),
                match uuid {
                    Tag::IntArray(words) if words.len() == 4 => int_list(words),
                    Tag::String(_) => {
                        let bytes = uuid::Uuid::parse_str(uuid.as_str()?).ok()?.into_bytes();
                        let words: Vec<_> = bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .copied()
                            .map(i32::from_be_bytes)
                            .collect();
                        int_list(&words)
                    }
                    _ => return None,
                },
            ));
            if let Some(name) = tag.get("name") {
                fields.push((string("name"), text_at(name, depth)?));
            }
        }
        "show_item" => {
            fields.push((string("id"), identifier(tag.get("id")?.as_str()?)));
            let count = match tag.get("count") {
                Some(value) => Some(int_tag(value)?),
                None => None,
            };
            if let Some(count) = count
                && count != 1
            {
                fields.push((string("count"), int(count)));
            }
            if let Some(components) = tag.get("components") {
                let Tag::Compound(entries) = components else {
                    return None;
                };
                if !entries.is_empty() {
                    let mut patch = Vec::with_capacity(entries.len());
                    for (name, value) in entries {
                        let name = std::str::from_utf8(name).ok()?;
                        let (removed, name) = name
                            .strip_prefix('!')
                            .map_or((false, name), |name| (true, name));
                        let name = crate::codec::qualified(name);
                        let key = if removed {
                            format!("!{name}")
                        } else {
                            name.to_string()
                        };
                        patch.push((
                            string(&key),
                            if removed {
                                map(Vec::new())
                            } else {
                                component_tag(&name, value, depth)?
                            },
                        ));
                    }
                    fields.push((string("components"), map(patch)));
                }
            }
        }
        _ => return None,
    }
    Some(map(fields))
}

fn component_tag(name: &str, tag: &crate::nbt::Tag, depth: usize) -> Option<u32> {
    use crate::nbt::Tag;
    Some(match name {
        "minecraft:custom_name" | "minecraft:item_name" => text_at(tag, depth)?,
        "minecraft:lore" => {
            let Tag::List(_, values) = tag else {
                return None;
            };
            list(
                values
                    .iter()
                    .map(|value| text_at(value, depth))
                    .collect::<Option<Vec<_>>>()?,
            )
        }
        "minecraft:enchantment_glint_override" => boolean(int_tag(tag)? != 0),
        "minecraft:max_stack_size"
        | "minecraft:max_damage"
        | "minecraft:damage"
        | "minecraft:repair_cost"
        | "minecraft:map_color"
        | "minecraft:dyed_color" => int(int_tag(tag)?),
        "minecraft:custom_data"
        | "minecraft:entity_data"
        | "minecraft:bucket_entity_data"
        | "minecraft:block_entity_data" => nbt(tag)?,
        "minecraft:item_model" | "minecraft:tooltip_style" | "minecraft:note_block_sound" => {
            identifier(tag.as_str()?)
        }
        "minecraft:rarity" | "minecraft:dye" => string(tag.as_str()?),
        "minecraft:unbreakable" | "minecraft:glider" => {
            let Tag::Compound(fields) = tag else {
                return None;
            };
            if !fields.is_empty() {
                return None;
            }
            map(Vec::new())
        }
        _ => return None,
    })
}

/// `HashOps.createList`.
#[must_use]
pub fn list(elements: impl IntoIterator<Item = u32>) -> u32 {
    let mut crc = Crc::new().bytes(&[4]);
    for element in elements {
        crc = crc.bytes(&element.to_le_bytes());
    }
    crc.bytes(&[5]).finish()
}

/// `HashOps.createMap`: entries sorted by key hash, then value hash, both
/// unsigned (`HashCode.padToLong`).
#[must_use]
pub fn map(mut entries: Vec<(u32, u32)>) -> u32 {
    entries.sort_unstable();
    let mut crc = Crc::new().bytes(&[2]);
    for (key, value) in entries {
        crc = crc.bytes(&key.to_le_bytes()).bytes(&value.to_le_bytes());
    }
    crc.bytes(&[3]).finish()
}

/// The hash as the `int` a `HashedStack` carries.
#[must_use]
pub const fn as_int(hash: u32) -> i32 {
    hash as i32
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Vectors the 26.2 jar produced (`HashOps.CRC32C_INSTANCE`), from
    /// Hyperion's `hyperion-minecraft-proto/tests/data/hashed_stack_vectors.json`:
    /// `max_stack_size 64` (an int), `item_model "minecraft:paper"` (a
    /// string), `unbreakable` (Unit: an empty map), `lore []` (an empty list).
    #[test]
    fn primitives_match_vectors_from_the_26_2_jar() {
        assert_eq!(as_int(int(64)), 733_160_003);
        assert_eq!(as_int(string("minecraft:paper")), 890_011_776);
        assert_eq!(as_int(map(Vec::new())), -982_207_288);
        assert_eq!(as_int(list([])), -1_978_007_022);
        // CRC32C("123456789") = 0xE3069283, the standard check value.
        assert_eq!(Crc::new().bytes(b"123456789").finish(), 0xE306_9283);
        // Map entry order does not depend on insertion order.
        assert_eq!(map(vec![(1, 2), (3, 4)]), map(vec![(3, 4), (1, 2)]));
        assert_ne!(list([1, 2]), list([2, 1]));
    }

    /// The jar's hash of a nested `custom_data` (longs, ints, a byte, lists
    /// of strings, an int array, a double, a short, a float, a compound and a
    /// non-ASCII string), from Hyperion's `hashed_stack_vectors.json`
    /// ("custom_data nested"): the NBT conversion by type.
    #[test]
    fn nbt_matches_the_jar_vector() {
        let hex = "0a0c00056c6f6e6773000000020000000000000004fffffffffffffffb0a00125075626c6963\
                   42756b6b697456616c75657303000c6e6f7374616c653a766e756d0000000f0800096f7261\
                   78656e3a6964000c6e6f735f73776f72645f313500010004666c616701090007737472696e\
                   67730800000002000161000262620b0004696e747300000003000000010000000200000003\
                   060006646f75626c65bff400000000000002000573686f7274fffe080007756e69636f6465\
                   000d43697474c3a020c3a820e280a2050005666c6f61743f0000000400046c6f6e67000001\
                   000000000000";
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let mut reader = crate::wire::Reader::new(&bytes);
        let tag = crate::nbt::Tag::read(&mut reader).unwrap().unwrap();
        assert_eq!(nbt(&tag).map(as_int), Some(2_058_887_930));
    }
    // Network bytes and expected hashes emitted by the official 26.2 jar,
    // using the existing Hyperion ItemData.java generator. They cover the
    // shared codec boundary independently of either Rust implementation.
    #[test]
    fn text_events_match_the_official_26_2_vectors() {
        let vectors = [
            ("custom_name plain", "0800055370616461", 1894353999_i32),
            (
                "custom_name not italic",
                "0a08000474657874000553706164610100066974616c69630000",
                524456081_i32,
            ),
            (
                "custom_name palette",
                "0a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000",
                -1004057365_i32,
            ),
            (
                "custom_name named colour",
                "0a080005636f6c6f720004676f6c640800047465787400034f726f00",
                -644388977_i32,
            ),
            (
                "custom_name nested",
                "0a09000565787472610a00000002080005636f6c6f720007234630453844380800047465787400044c762e2000080005636f6c6f7200072346314339373501000a756e6465726c696e6564010800047465787400023135000800047465787400000100066974616c69630000",
                1576502423_i32,
            ),
            (
                "custom_name translatable",
                "0a0800097472616e736c617465001c6974656d2e6d696e6563726166742e6469616d6f6e645f73776f726400",
                1855830573_i32,
            ),
            (
                "custom_name translatable with args",
                "0a090004776974680a000000020800000005506970706f000800047465787400046369616f010004626f6c6401000800097472616e736c617465000e636861742e747970652e7465787400",
                464439329_i32,
            ),
            (
                "custom_name shadow strike obfuscated",
                "0a03000c736861646f775f636f6c6f72ff1122330800047465787400054f6d62726101000d737472696b657468726f7567680001000a6f6266757363617465640100",
                -436491860_i32,
            ),
            (
                "custom_name keybind",
                "0a0800076b657962696e6400086b65792e6a756d7000",
                1240463282_i32,
            ),
            (
                "custom_name insertion",
                "0a080009696e73657274696f6e0003696e730800047465787400017800",
                615149910_i32,
            ),
            (
                "custom_name click URL",
                "0a0a000b636c69636b5f6576656e74080006616374696f6e00086f70656e5f75726c08000375726c001e68747470733a2f2f6578616d706c652e696e76616c69642f736f7572636500080004746578740005537061646100",
                2062367003_i32,
            ),
            (
                "custom_name click command",
                "0a0a000b636c69636b5f6576656e74080006616374696f6e000b72756e5f636f6d6d616e64080007636f6d6d616e6400072f736f7572636500080004746578740005537061646100",
                -1935547325_i32,
            ),
            (
                "custom_name suggest command",
                "0a0a000b636c69636b5f6576656e74080006616374696f6e000f737567676573745f636f6d6d616e64080007636f6d6d616e64000d2f717565737420747261636b2000080004746578740005537061646100",
                754732841_i32,
            ),
            (
                "custom_name copy clipboard",
                "0a0a000b636c69636b5f6576656e74080006616374696f6e0011636f70795f746f5f636c6970626f61726408000576616c7565000a43697474c3a020e280a200080004746578740005537061646100",
                1861718162_i32,
            ),
            (
                "custom_name change page",
                "0a0a000b636c69636b5f6576656e74080006616374696f6e000b6368616e67655f70616765030004706167650000000200080004746578740005537061646100",
                582576238_i32,
            ),
            (
                "custom_name custom click",
                "0a0a000b636c69636b5f6576656e740a00077061796c6f6164010004666c61670100080006616374696f6e0006637573746f6d0800026964000c656c797a756e653a7069636b00080004746578740005537061646100",
                2105985186_i32,
            ),
            (
                "custom_name hover text",
                "0a08000474657874000553706164610a000b686f7665725f6576656e74080006616374696f6e000973686f775f746578740a000576616c7565080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c696300000000",
                738279084_i32,
            ),
            (
                "custom_name hover entity",
                "0a08000474657874000553706164610a000b686f7665725f6576656e740a00046e616d65080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000080006616374696f6e000b73686f775f656e74697479080002696400106d696e6563726166743a7a6f6d6269650b00047575696400000004069a79f444e94726a5befca90e38aaf50000",
                -1485167369_i32,
            ),
            (
                "custom_name hover item",
                "0a08000474657874000553706164610a000b686f7665725f6576656e74080006616374696f6e000973686f775f6974656d0800026964000f6d696e6563726166743a70617065720000",
                1997842887_i32,
            ),
            (
                "custom_name hover item components",
                "0a08000474657874000553706164610a000b686f7665725f6576656e740a000a636f6d706f6e656e74730a001e216d696e6563726166743a6174747269627574655f6d6f64696669657273000300186d696e6563726166743a6d61785f737461636b5f73697a65000000010a00156d696e6563726166743a637573746f6d5f6e616d6508000474657874000553706164610a000b686f7665725f6576656e740a00046e616d65080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000080006616374696f6e000b73686f775f656e74697479080002696400106d696e6563726166743a7a6f6d6269650b00047575696400000004069a79f444e94726a5befca90e38aaf500000100246d696e6563726166743a656e6368616e746d656e745f676c696e745f6f76657272696465000a00156d696e6563726166743a637573746f6d5f64617461010004666c6167010000030005636f756e7400000003080006616374696f6e000973686f775f6974656d0800026964000f6d696e6563726166743a70617065720000",
                -1638129596_i32,
            ),
            (
                "item_name palette",
                "0a080005636f6c6f7200072338384330453808000474657874000c53706164612064656c205265010004626f6c64010100066974616c69630000",
                -1004057365_i32,
            ),
        ];
        for (name, hex, expected) in vectors {
            let bytes: Vec<_> = (0..hex.len())
                .step_by(2)
                .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
                .collect();
            let mut reader = crate::wire::Reader::new(&bytes);
            let tag = crate::nbt::Tag::read(&mut reader).unwrap().unwrap();
            assert!(reader.is_empty(), "{name}");
            assert_eq!(text(&tag).map(as_int), Some(expected), "{name}");
        }
    }

    fn sent(server_item: u32) -> Sent {
        Sent {
            client_item: 1,
            client_added: vec![0],
            client_removed: Vec::new(),
            server_item,
            server_added: Vec::new(),
            server_removed: Vec::new(),
        }
    }

    /// The records behave as ViaVersion's 1024-entry cache: keyed by the
    /// `custom_data` hash, the same key replaces its record, the least
    /// recently recorded or answered record goes first.
    #[test]
    fn records_are_keyed_bounded_and_least_recently_used_first() {
        let key = |hash: i32| RecordKey::of_claim(hash, 1, &[0], &[]);
        let mut stacks = SentStacks::default();
        for hash in 0..i32::try_from(MAX_SENT).unwrap() {
            stacks.record(hash, sent(hash.unsigned_abs()));
        }
        assert_eq!(stacks.len(), MAX_SENT);
        // Answering key 0 keeps it; recording one more evicts key 1.
        assert!(stacks.find(&key(0), &[]).is_some());
        stacks.touch(&key(0));
        stacks.record(-1, sent(9));
        assert_eq!(stacks.len(), MAX_SENT);
        assert!(stacks.find(&key(0), &[]).is_some());
        assert!(stacks.find(&key(1), &[]).is_none());
        // The same key sent again replaces its record and refreshes it.
        stacks.record(2, sent(77));
        stacks.record(-2, sent(9));
        assert_eq!(stacks.find(&key(2), &[]).map(|s| s.server_item), Some(77));
        assert!(stacks.find(&key(3), &[]).is_none());
        // A claim that does not describe the recorded stack is not answered.
        assert!(
            stacks
                .find(&RecordKey::of_claim(2, 2, &[0], &[]), &[])
                .is_none()
        );
        assert!(
            stacks
                .find(&RecordKey::of_claim(2, 1, &[], &[]), &[])
                .is_none()
        );
        // One backup hash, two shapes (a component the client sees as it is
        // on one of them): both records stay.
        let mut wider = sent(55);
        wider.client_added = vec![0, 6];
        stacks.record(2, wider);
        assert_eq!(stacks.find(&key(2), &[]).map(|s| s.server_item), Some(77));
        assert_eq!(
            stacks
                .find(&RecordKey::of_claim(2, 1, &[0, 6], &[]), &[])
                .map(|s| s.server_item),
            Some(55)
        );
        stacks.clear();
        assert!(stacks.is_empty());
    }

    #[test]
    fn the_backup_never_survives_the_way_back() {
        use crate::nbt::Tag;
        let backup = original_hashes(&Sent {
            server_item: 48,
            server_added: vec![(78, Claim::Computed(5)), (6, Claim::FromClient(6))],
            server_removed: vec![3],
            ..sent(48)
        });
        // Only the hashes the client cannot reproduce are listed.
        assert_eq!(
            backup,
            Tag::Compound(vec![
                (b"id".to_vec(), Tag::Int(48)),
                (b"78".to_vec(), Tag::Int(5)),
                (b"removed".to_vec(), Tag::IntArray(vec![3])),
            ])
        );
        let mut created = Tag::Compound(vec![(CREATED_KEY.as_bytes().to_vec(), Tag::Byte(1))]);
        created.insert(ORIGINAL_HASHES_KEY, backup.clone());
        assert_eq!(
            strip_backup(&mut created),
            Stripped {
                keep: false,
                server_item: Some(48)
            }
        );
        let mut own = Tag::Compound(vec![(b"ely".to_vec(), Tag::Int(7))]);
        own.insert(ORIGINAL_HASHES_KEY, backup);
        assert_eq!(
            strip_backup(&mut own),
            Stripped {
                keep: true,
                server_item: Some(48)
            }
        );
        assert_eq!(own, Tag::Compound(vec![(b"ely".to_vec(), Tag::Int(7))]));
    }
}
