//! A small interpreter for generated wire layouts.
//!
//! Data component values (and a few other nested structures) have no length
//! prefix, so the only way to find where one ends, or to renumber the registry
//! ids inside it, is to walk its full layout. The layouts are generated from
//! the jars (`generated/components.rs`); this module walks them, copying the
//! bytes and remapping every registry id it meets, and calls back into
//! `item.rs` for the codecs a generated layout cannot express (nested item
//! stacks, dispatches on a runtime type).
//!
//! The walker assumes the destination protocol uses the same layout as the
//! source; components whose shape changed are listed in the generated
//! `RESHAPED_*` tables and converted by hand before this module is reached.
//!
//! # Missing ids
//!
//! A registry entry with no equivalent on the other side ([`Xerr::Absent`]) is
//! not an error of the stream. Containers decide what it means: a list or a
//! map skips the element, a holder set leaves the id out, an optional becomes
//! empty, a holder of a sound event becomes an inline sound by name, and
//! anything else propagates up to the component, which is then removed from
//! the item. Every such loss leaves a note, which the packet's handler logs
//! once per connection: nothing is dropped silently.

use crate::generated::Proto;
use crate::generated::registries::Reg;
use crate::nbt;
use crate::session::ViaTranslator;
use crate::wire::{Put, Reader, WireError};

/// A registry reference in a layout.
#[derive(Debug, Clone, Copy)]
pub enum R {
    /// A static registry with generated remap tables.
    Static(Reg),
    /// A synchronised registry, remapped with the ids recorded in configuration.
    Dynamic(&'static str),
    /// A static registry identical in every supported release.
    Same,
}

/// Codecs the generated layouts leave to hand-written code.
#[derive(Debug, Clone, Copy)]
pub enum Custom {
    /// `ItemStackTemplate`: item id, count, component patch (26.1+).
    ItemTemplate,
    /// `ItemStack.STREAM_CODEC`: count (> 0), item id, component patch.
    ItemStack,
    /// `ItemStack.OPTIONAL_STREAM_CODEC`: count, then id and patch if count > 0.
    OptionalItemStack,
    /// `DataComponentPatch`.
    Patch,
    /// `TypedDataComponent`: component type id and its value.
    TypedComponent,
    /// `ParticleTypes.STREAM_CODEC`: particle type id and its option data.
    Particle,
    /// `ConsumeEffect`, dispatched on `consume_effect_type`.
    ConsumeEffect,
    /// `ItemAttributeModifiers.Display`: 0 default, 1 hidden, 2 override + text.
    AttributeDisplay,
}

/// One node of a generated layout.
#[derive(Debug)]
pub enum W {
    VarInt,
    VarLong,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Bool,
    Uuid,
    Str,
    Nbt,
    OptNbt,
    /// A text component (network NBT), rewritten by `text::component`.
    Text,
    ByteArray,
    LongArray,
    VarIntArray,
    /// A fixed-size byte array (a `FixedBitSet` of so many bytes).
    Fixed(usize),
    Unit,
    /// Block position, direction, cursor x/y/z, inside flag, world border flag.
    BlockHitResult,
    Reg(R),
    /// VarInt `id + 1`, or 0 followed by an inline value.
    Holder(R, &'static W),
    /// VarInt `n + 1` followed by n ids, or 0 followed by a tag name.
    HolderSet(R),
    List(&'static W),
    Opt(&'static W),
    /// A VarInt byte length, then a value of that size.
    LenPrefixed(&'static W),
    Map(&'static W, &'static W),
    /// A boolean, then the left value when true, the right one when false.
    Either(&'static W, &'static W),
    Struct(&'static [W]),
    /// A named codec: index into the protocol's type table.
    Ref(u16),
    Custom(Custom),
    /// A part no extractor run or override models.
    Unresolved,
}

/// Why a value could not be transcoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Xerr {
    /// The bytes do not follow the layout: the whole packet is unreadable.
    Wire(WireError),
    /// A registry entry has no equivalent; the caller decides what to drop.
    Absent(&'static str),
}

impl From<WireError> for Xerr {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}

pub type Xresult<T> = Result<T, Xerr>;

/// Which way a value travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// From the 776 backend to the client.
    S2c,
    /// From the client to the 776 backend.
    C2s,
}

/// The layouts of one protocol.
#[derive(Clone, Copy)]
pub struct Layouts {
    pub types: &'static [W],
    pub components: &'static [(&'static str, W)],
}

/// The layouts a protocol's values are read with. 1.21.11 shares 26.1's
/// tables; the components whose shape differs are converted by hand.
#[must_use]
pub fn layouts(proto: Proto) -> Layouts {
    use crate::generated::components as c;
    match proto {
        Proto::V774 | Proto::V775 => Layouts {
            types: c::TYPES_775,
            components: c::COMPONENTS_775,
        },
        Proto::V776 => Layouts {
            types: c::TYPES_776,
            components: c::COMPONENTS_776,
        },
        Proto::V777 => Layouts {
            types: c::TYPES_777,
            components: c::COMPONENTS_777,
        },
    }
}

/// A transcoding pass over one value.
pub struct Xc<'t> {
    pub t: &'t ViaTranslator,
    pub dir: Dir,
    /// Layouts of the protocol being read.
    pub src: Layouts,
    /// Problems worth one log line each (deduplicated by the caller).
    pub notes: Vec<&'static str>,
    /// While set, ids are copied unchanged: used to step over a value that is
    /// being dropped, which must never fail on a missing id.
    identity: bool,
    /// Set when a component patch was changed in a way its `HashedStack`
    /// notices (a component dropped, split, merged or reshaped); see
    /// `hashed.rs`. Callers that care reset it before a stack.
    pub hash_changed: bool,
    /// The server item id a serverbound patch's hash backup recorded
    /// (`hashed::strip_backup`), for the creative slot to undo a stand-in.
    /// Callers that care reset it before a stack.
    pub backup_item: Option<u32>,
}

impl<'t> Xc<'t> {
    #[must_use]
    pub fn new(t: &'t ViaTranslator, dir: Dir) -> Self {
        let src = match dir {
            Dir::S2c => layouts(Proto::V776),
            Dir::C2s => layouts(t.client),
        };
        Self {
            t,
            dir,
            src,
            notes: Vec::new(),
            identity: false,
            hash_changed: false,
            backup_item: None,
        }
    }

    /// Whether ids are currently copied unchanged (see `with_identity`).
    #[must_use]
    pub const fn is_identity(&self) -> bool {
        self.identity
    }

    /// Runs `f` with id remapping switched off.
    pub fn with_identity<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.identity;
        self.identity = true;
        let result = f(self);
        self.identity = saved;
        result
    }

    /// The protocol the bytes are read in.
    #[must_use]
    pub fn src_proto(&self) -> Proto {
        match self.dir {
            Dir::S2c => Proto::V776,
            Dir::C2s => self.t.client,
        }
    }

    /// The protocol the bytes are written in.
    #[must_use]
    pub fn dst_proto(&self) -> Proto {
        match self.dir {
            Dir::S2c => self.t.client,
            Dir::C2s => Proto::V776,
        }
    }

    /// Maps one registry id in the direction of this pass.
    pub fn map_id(&self, reg: R, id: u32) -> Option<u32> {
        if self.identity {
            return Some(id);
        }
        match (reg, self.dir) {
            (R::Same, _) => Some(id),
            (R::Static(reg), Dir::S2c) => self.t.tables.reg(reg).s2c(id),
            (R::Static(reg), Dir::C2s) => self.t.tables.reg(reg).c2s(id),
            (R::Dynamic(name), Dir::S2c) => self.t.conn.dyn_s2c(name, id),
            (R::Dynamic(name), Dir::C2s) => self.t.conn.dyn_c2s(name, id),
        }
    }

    pub fn map_static(&self, reg: Reg, id: u32) -> Option<u32> {
        self.map_id(R::Static(reg), id)
    }

    fn read_id(r: &mut Reader<'_>) -> Xresult<u32> {
        let raw = r.varint()?;
        u32::try_from(raw).map_err(|_| Xerr::Wire(WireError::Malformed("negative registry id")))
    }

    /// Transcodes one value of layout `w` from `r` into `out`. On `Absent`,
    /// `out` may hold a partial value: callers that recover write into a
    /// scratch buffer first.
    pub fn value(&mut self, w: &W, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
        match w {
            W::VarInt => out.put_varint(r.varint()?),
            W::VarLong => out.put_varlong(r.varlong()?),
            W::I8 | W::Bool => out.put_u8(r.u8()?),
            W::I16 => out.put_bytes(r.bytes(2)?),
            W::I32 | W::F32 => out.put_bytes(r.bytes(4)?),
            W::I64 | W::F64 => out.put_bytes(r.bytes(8)?),
            W::Uuid => out.put_bytes(r.bytes(16)?),
            W::Str => out.put_str_bytes(r.str_bytes()?),
            W::Nbt | W::OptNbt => {
                let start = r.pos();
                nbt::skip(r)?;
                out.put_bytes(r.since(start));
            }
            W::Text if self.identity => {
                let start = r.pos();
                nbt::skip(r)?;
                out.put_bytes(r.since(start));
            }
            W::Text => crate::text::component(self, r, out)?,
            W::ByteArray => {
                let n = r.len()?;
                out.put_len(n);
                out.put_bytes(r.bytes(n)?);
            }
            W::LongArray => {
                let n = r.len()?;
                out.put_len(n);
                out.put_bytes(r.bytes(n.checked_mul(8).ok_or(WireError::BadLength)?)?);
            }
            W::VarIntArray => {
                let n = r.len()?;
                out.put_len(n);
                for _ in 0..n {
                    out.put_varint(r.varint()?);
                }
            }
            W::Fixed(n) => out.put_bytes(r.bytes(*n)?),
            W::Unit => {}
            W::BlockHitResult => {
                out.put_bytes(r.bytes(8)?);
                out.put_varint(r.varint()?);
                out.put_bytes(r.bytes(12)?);
                out.put_bytes(r.bytes(2)?);
            }
            W::Reg(reg) => {
                let id = Self::read_id(r)?;
                let mapped = self
                    .map_id(*reg, id)
                    .ok_or(Xerr::Absent("registry entry"))?;
                out.put_varint(crate::wire::wire_id(mapped));
            }
            W::Holder(reg, inline) => self.holder(*reg, inline, r, out)?,
            W::HolderSet(reg) => self.holder_set(*reg, r, out)?,
            W::List(elem) => {
                let n = r.len()?;
                let mut body = Vec::new();
                let mut kept = 0usize;
                for _ in 0..n {
                    let mark = body.len();
                    match self.value(elem, r, &mut body) {
                        Ok(()) => kept += 1,
                        Err(Xerr::Absent(_)) => {
                            body.truncate(mark);
                            self.notes.push(
                                "a list element with no equivalent on the other side was removed",
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
                out.put_len(kept);
                out.put_bytes(&body);
            }
            W::Opt(inner) => {
                if r.bool()? {
                    let mut body = Vec::new();
                    match self.value(inner, r, &mut body) {
                        Ok(()) => {
                            out.put_bool(true);
                            out.put_bytes(&body);
                        }
                        Err(Xerr::Absent(_)) => {
                            out.put_bool(false);
                            self.notes
                                .push("an optional value with no equivalent on the other side was left empty");
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    out.put_bool(false);
                }
            }
            W::LenPrefixed(inner) => {
                let n = r.len()?;
                let mut sub = Reader::new(r.bytes(n)?);
                let mut body = Vec::new();
                self.value(inner, &mut sub, &mut body)?;
                out.put_len(body.len());
                out.put_bytes(&body);
            }
            W::Map(key, value) => {
                let n = r.len()?;
                let mut body = Vec::new();
                let mut kept = 0usize;
                for _ in 0..n {
                    let mark = body.len();
                    let pair = self
                        .value(key, r, &mut body)
                        .and_then(|()| self.value(value, r, &mut body));
                    match pair {
                        Ok(()) => kept += 1,
                        Err(Xerr::Absent(_)) => {
                            body.truncate(mark);
                            self.notes.push(
                                "a map entry with no equivalent on the other side was removed",
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
                out.put_len(kept);
                out.put_bytes(&body);
            }
            W::Either(left, right) => {
                let is_left = r.bool()?;
                out.put_bool(is_left);
                self.value(if is_left { left } else { right }, r, out)?;
            }
            W::Struct(fields) => {
                for field in *fields {
                    self.value(field, r, out)?;
                }
            }
            W::Ref(_) => {
                let target = self.resolve(w)?;
                self.value(target, r, out)?;
            }
            W::Custom(custom) => crate::item::custom(self, *custom, r, out)?,
            W::Unresolved => return Err(Xerr::Absent("a value the translator does not model")),
        }
        Ok(())
    }

    /// Follows `W::Ref` links to the node they name.
    pub fn resolve<'w>(&self, mut w: &'w W) -> Xresult<&'w W>
    where
        'static: 'w,
    {
        for _ in 0..64 {
            let W::Ref(index) = w else {
                return Ok(w);
            };
            w = self
                .src
                .types
                .get(usize::from(*index))
                .ok_or(Xerr::Wire(WireError::Malformed(
                    "dangling layout reference",
                )))?;
        }
        Err(Xerr::Wire(WireError::Malformed("layout reference loop")))
    }

    fn holder(&mut self, reg: R, inline: &W, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
        let raw = Self::read_id(r)?;
        if raw == 0 {
            out.put_varint(0);
            return self.value(inline, r, out);
        }
        let id = raw - 1;
        if let Some(mapped) = self.map_id(reg, id) {
            out.put_varint(crate::wire::wire_id(mapped) + 1);
            return Ok(());
        }
        // A sound the client does not have keeps playing by name: resource
        // packs (and newer sound assets) are looked up by identifier.
        if let (R::Static(Reg::SoundEvent), Dir::S2c) = (reg, self.dir)
            && let Some(name) = crate::generated::registries::sound_name_776(id)
        {
            out.put_varint(0);
            out.put_str(name);
            out.put_bool(false);
            return Ok(());
        }
        Err(Xerr::Absent("holder"))
    }

    fn holder_set(&mut self, reg: R, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
        let n = Self::read_id(r)?;
        if n == 0 {
            out.put_varint(0);
            out.put_str_bytes(r.str_bytes()?);
            return Ok(());
        }
        let count = usize::try_from(n - 1).map_err(|_| WireError::BadLength)?;
        if count > crate::wire::MAX_LENGTH {
            return Err(Xerr::Wire(WireError::BadLength));
        }
        let mut ids = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let id = Self::read_id(r)?;
            match self.map_id(reg, id) {
                Some(mapped) => ids.push(mapped),
                None => self
                    .notes
                    .push("a holder set id with no equivalent on the other side was removed"),
            }
        }
        out.put_len(ids.len() + 1);
        for id in ids {
            out.put_varint(crate::wire::wire_id(id));
        }
        Ok(())
    }
}
