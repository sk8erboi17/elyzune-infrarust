//! Recipe displays and advancements.
//!
//! Layouts (26.2):
//!
//! ```text
//! SlotDisplay       VarInt type, then by type name:
//!   empty, any_fuel         nothing
//!   with_any_potion         SlotDisplay                       (26.1+)
//!   only_with_component     SlotDisplay, VarInt component type (26.1+)
//!   item                    VarInt item
//!   item_stack              ItemStackTemplate (1.21.11: ItemStack)
//!   tag                     Identifier (26.3: HolderSet of items)
//!   dyed                    SlotDisplay dye, SlotDisplay target (26.1+)
//!   smithing_trim           SlotDisplay base, SlotDisplay material,
//!                           Holder<TrimPattern>
//!   with_remainder          SlotDisplay input, SlotDisplay remainder
//!   composite               List<SlotDisplay>
//! RecipeDisplay     VarInt type, then:
//!   crafting_shapeless      List<SlotDisplay>, result, station
//!   crafting_shaped         VarInt w, VarInt h, List<SlotDisplay>, result, station
//!   furnace                 ingredient, fuel, result, station, VarInt time, f32 xp
//!   stonecutter             input, result, station
//!   smithing                template, base, addition, result, station
//! ```
//!
//! 1.21.11 lacks the three 26.1 slot displays; each is shown as the display it
//! wraps (the dyed target, the component's source, the potion display).

use crate::generated::Proto;
use crate::generated::registries::{Reg, name_of};
use crate::layout::{Dir, R, W, Xc, Xerr, Xresult};
use crate::wire::{Put, Reader, WireError, wire_id};

/// An item ingredient: a holder set of items.
static INGREDIENT: W = W::HolderSet(R::Static(Reg::Item));
/// Optional crafting requirements of a recipe book entry.
static REQUIREMENTS: W = W::Opt(&W::List(&INGREDIENT));

fn read_u32(r: &mut Reader<'_>) -> Xresult<u32> {
    u32::try_from(r.varint()?).map_err(|_| Xerr::Wire(WireError::Malformed("negative id")))
}

/// One `SlotDisplay`, read in 776 and written for the client.
pub fn slot_display(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let ty = read_u32(r)?;
    let name = name_of(Reg::SlotDisplay, x.src_proto().number().unsigned_abs(), ty)
        .ok_or(Xerr::Wire(WireError::Malformed("unknown slot display")))?;
    let mapped = x.map_static(Reg::SlotDisplay, ty);
    // A display the client lacks is replaced by the display it wraps.
    if mapped.is_none() && x.dir == Dir::S2c {
        return match name {
            "minecraft:with_any_potion" => slot_display(x, r, out),
            "minecraft:only_with_component" => {
                slot_display(x, r, out)?;
                r.varint()?;
                Ok(())
            }
            "minecraft:dyed" => {
                let mut dye = Vec::new();
                slot_display(x, r, &mut dye)?;
                slot_display(x, r, out)
            }
            _ => Err(Xerr::Absent("slot display")),
        };
    }
    let mapped = mapped.ok_or(Xerr::Absent("slot display"))?;
    out.put_varint(wire_id(mapped));
    match name {
        "minecraft:empty" | "minecraft:any_fuel" => {}
        "minecraft:with_any_potion" => slot_display(x, r, out)?,
        "minecraft:only_with_component" => {
            slot_display(x, r, out)?;
            x.value(&W::Reg(R::Static(Reg::DataComponentType)), r, out)?;
        }
        "minecraft:item" => x.value(&W::Reg(R::Static(Reg::Item)), r, out)?,
        "minecraft:item_stack" => match (x.src_proto(), x.dst_proto()) {
            (_, Proto::V774) => {
                let id = read_u32(r)?;
                let count = r.varint()?;
                let mut patch = Vec::new();
                crate::item::patch(x, r, &mut patch, false, Some(id))?;
                let id = x.map_static(Reg::Item, id).ok_or(Xerr::Absent("item"))?;
                out.put_varint(count.max(1));
                out.put_varint(wire_id(id));
                out.put_bytes(&patch);
            }
            _ => crate::item::template(x, r, out)?,
        },
        "minecraft:tag" => {
            let tag = r.str_bytes()?;
            if x.dst_proto() == Proto::V777 {
                // 26.3 sends an item holder set; a tag is its named form.
                out.put_varint(0);
            }
            out.put_str_bytes(tag);
        }
        "minecraft:dyed" | "minecraft:with_remainder" => {
            slot_display(x, r, out)?;
            slot_display(x, r, out)?;
        }
        "minecraft:smithing_trim" => {
            slot_display(x, r, out)?;
            slot_display(x, r, out)?;
            trim_pattern(x, r, out)?;
        }
        "minecraft:composite" => {
            let n = r.len()?;
            let mut body = Vec::new();
            let mut kept = 0usize;
            for _ in 0..n {
                let mark = body.len();
                match slot_display(x, r, &mut body) {
                    Ok(()) => kept += 1,
                    Err(Xerr::Absent(_)) => body.truncate(mark),
                    Err(e) => return Err(e),
                }
            }
            out.put_len(kept);
            out.put_bytes(&body);
        }
        _ => return Err(Xerr::Absent("slot display of an unknown kind")),
    }
    Ok(())
}

/// `Holder<TrimPattern>`: inline patterns are an asset id, a description and
/// a decal flag, unchanged from 1.21.11 to 26.3.
fn trim_pattern(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let pattern = W::Holder(
        R::Dynamic("minecraft:trim_pattern"),
        &W::Struct(&[W::Str, W::Text, W::Bool]),
    );
    x.value(&pattern, r, out)
}

fn slot_displays(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>, n: usize) -> Xresult<()> {
    for _ in 0..n {
        slot_display(x, r, out)?;
    }
    Ok(())
}

/// One `RecipeDisplay`.
pub fn recipe_display(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let ty = read_u32(r)?;
    let name = name_of(
        Reg::RecipeDisplay,
        x.src_proto().number().unsigned_abs(),
        ty,
    )
    .ok_or(Xerr::Wire(WireError::Malformed("unknown recipe display")))?;
    let mapped = x
        .map_static(Reg::RecipeDisplay, ty)
        .ok_or(Xerr::Absent("recipe display"))?;
    out.put_varint(wire_id(mapped));
    match name {
        "minecraft:crafting_shapeless" => {
            let n = r.len()?;
            out.put_len(n);
            slot_displays(x, r, out, n + 2)?;
        }
        "minecraft:crafting_shaped" => {
            out.put_varint(r.varint()?);
            out.put_varint(r.varint()?);
            let n = r.len()?;
            out.put_len(n);
            slot_displays(x, r, out, n + 2)?;
        }
        "minecraft:furnace" => {
            slot_displays(x, r, out, 4)?;
            out.put_varint(r.varint()?);
            out.put_bytes(r.bytes(4)?);
        }
        "minecraft:stonecutter" => slot_displays(x, r, out, 3)?,
        "minecraft:smithing" => slot_displays(x, r, out, 5)?,
        _ => return Err(Xerr::Absent("recipe display of an unknown kind")),
    }
    Ok(())
}

/// `recipe_book_add`: List<{ RecipeDisplayEntry, i8 flags }>, Bool replace.
/// RecipeDisplayEntry: VarInt id, RecipeDisplay, OptVarInt group, VarInt
/// category, Optional<List<Ingredient>> crafting requirements.
pub fn recipe_book_add(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    let n = r.len()?;
    let mut entries = Vec::new();
    let mut kept = 0usize;
    for _ in 0..n {
        let mut entry = Vec::new();
        entry.put_varint(r.varint()?);
        let display = recipe_display(x, &mut r, &mut entry);
        // The rest must be read whatever happened to the display.
        let mut tail = Vec::new();
        tail.put_varint(r.varint()?);
        x.value(
            &W::Reg(R::Static(Reg::RecipeBookCategory)),
            &mut r,
            &mut tail,
        )?;
        x.value(&REQUIREMENTS, &mut r, &mut tail)?;
        tail.put_u8(r.u8()?);
        match display {
            Ok(()) => {
                entries.put_bytes(&entry);
                entries.put_bytes(&tail);
                kept += 1;
            }
            Err(Xerr::Absent(_)) => x
                .notes
                .push("a recipe whose display has no equivalent was left out"),
            Err(e) => return Err(e),
        }
    }
    out.put_len(kept);
    out.put_bytes(&entries);
    out.put_u8(r.u8()?);
    Ok(())
}

/// `place_ghost_recipe`: container id, RecipeDisplay.
pub fn place_ghost_recipe(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_varint(r.varint()?);
    recipe_display(x, &mut r, out)
}

/// `update_recipes`: Map<Identifier, List<VarInt item>> property sets, then
/// stonecutter recipes: List<{ Ingredient, SlotDisplay }>.
pub fn update_recipes(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    x.value(
        &W::Map(&W::Str, &W::List(&W::Reg(R::Static(Reg::Item)))),
        &mut r,
        out,
    )?;
    let n = r.len()?;
    let mut body_out = Vec::new();
    let mut kept = 0usize;
    for _ in 0..n {
        let mut entry = Vec::new();
        x.value(&INGREDIENT, &mut r, &mut entry)?;
        match slot_display(x, &mut r, &mut entry) {
            Ok(()) => {
                body_out.put_bytes(&entry);
                kept += 1;
            }
            Err(Xerr::Absent(_)) => {}
            Err(e) => return Err(e),
        }
    }
    out.put_len(kept);
    out.put_bytes(&body_out);
    Ok(())
}

/// `update_advancements`: Bool reset, List<{ Identifier id, Advancement }>,
/// List<Identifier> removed, Map<Identifier, Map<String, Optional<i64>>>
/// progress, Bool show.
///
/// Advancement: Optional<Identifier> parent, Optional<DisplayInfo>,
/// List<List<String>> requirements, Bool telemetry. DisplayInfo: NBT title,
/// NBT description, icon (26.1+ ItemStackTemplate, 1.21.11 ItemStack),
/// VarInt frame, i32 flags, Identifier background if flags & 1, f32 x, f32 y.
/// 26.3 moved x/y out of DisplayInfo to after the telemetry flag, present for
/// every advancement (0, 0 without a display).
pub fn advancements(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_u8(r.u8()?);
    let n = r.len()?;
    out.put_len(n);
    let to_777 = x.dst_proto() == Proto::V777;
    for _ in 0..n {
        out.put_str_bytes(r.str_bytes()?);
        x.value(&W::Opt(&W::Str), &mut r, out)?;
        let mut position = None;
        if r.bool()? {
            out.put_bool(true);
            x.value(&W::Text, &mut r, out)?;
            x.value(&W::Text, &mut r, out)?;
            icon(x, &mut r, out)?;
            out.put_varint(r.varint()?);
            let flags = r.i32()?;
            out.put_i32(flags);
            if flags & 1 != 0 {
                out.put_str_bytes(r.str_bytes()?);
            }
            let xy = r.bytes(8)?;
            if to_777 {
                position = Some(xy);
            } else {
                out.put_bytes(xy);
            }
        } else {
            out.put_bool(false);
        }
        x.value(&W::List(&W::List(&W::Str)), &mut r, out)?;
        out.put_u8(r.u8()?);
        if to_777 {
            out.put_bytes(position.unwrap_or(&[0; 8]));
        }
    }
    x.value(&W::List(&W::Str), &mut r, out)?;
    x.value(
        &W::Map(&W::Str, &W::Map(&W::Str, &W::Opt(&W::I64))),
        &mut r,
        out,
    )?;
    out.put_u8(r.u8()?);
    Ok(())
}

/// An advancement icon: a template from 26.1, a stack in 1.21.11. Every
/// 776 item has a client equivalent (fallbacks included), so the icon always
/// survives.
fn icon(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let id = read_u32(r)?;
    let count = r.varint()?;
    let mut patch = Vec::new();
    crate::item::patch(x, r, &mut patch, false, Some(id))?;
    let id = x.map_static(Reg::Item, id).ok_or(Xerr::Absent("item"))?;
    if x.dst_proto() == Proto::V774 {
        out.put_varint(count.max(1));
        out.put_varint(wire_id(id));
    } else {
        out.put_varint(wire_id(id));
        out.put_varint(count);
    }
    out.put_bytes(&patch);
    Ok(())
}
