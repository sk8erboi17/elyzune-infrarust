//! Clientbound packets whose only release-dependent content is text.
//!
//! Every text component goes through `W::Text` (see `text.rs`), which copies
//! it untouched unless a hover or dialog click names something that differs
//! between releases. The layouts below are 26.2's (`ClientboundXxxPacket`
//! `STREAM_CODEC`s, decompiled 26.2 sources); 1.21.11, 26.1 and 26.3 encode
//! these packets identically, which the golden tests pin per client.
//!
//! Packets with text that are rewritten elsewhere: `player_chat`,
//! `disguised_chat`, `set_player_team`, `update_advancements`, entity data,
//! item components (`play.rs`, `recipe.rs`, `entity.rs`, `item.rs`).

use crate::generated::registries::Reg;
use crate::layout::{R, W, Xc, Xerr, Xresult};
use crate::wire::{Put, Reader, WireError};

static TEXT: W = W::Text;
static OPT_TEXT: W = W::Opt(&W::Text);

/// One text component, then the rest of the body unchanged:
/// `system_chat` (Bool overlay follows), `set_title_text`,
/// `set_subtitle_text`, `set_action_bar_text`, `disconnect`, `server_data`
/// (optional icon follows), `test_instance_block_status` (optional size).
pub fn leading_text(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    x.value(&TEXT, &mut r, out)?;
    out.put_bytes(r.rest());
    Ok(())
}

/// `tab_list`: header, footer.
pub fn tab_list(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    x.value(&TEXT, &mut r, out)?;
    x.value(&TEXT, &mut r, out)
}

/// `player_combat_kill`: VarInt player, message.
pub fn player_combat_kill(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_varint(r.varint()?);
    x.value(&TEXT, &mut r, out)
}

/// `open_screen`: VarInt container, VarInt menu type, title.
pub fn open_screen(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_varint(r.varint()?);
    x.value(&W::Reg(R::Static(Reg::Menu)), &mut r, out)?;
    x.value(&TEXT, &mut r, out)
}

/// `command_suggestions`: VarInt id, start, length, then
/// `List<{String text, Optional<Component> tooltip}>`.
pub fn command_suggestions(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    static ENTRIES: W = W::List(&W::Struct(&[W::Str, W::Opt(&W::Text)]));
    let mut r = Reader::new(body);
    for _ in 0..3 {
        out.put_varint(r.varint()?);
    }
    x.value(&ENTRIES, &mut r, out)
}

/// `map_item_data`: VarInt map id, i8 scale, Bool locked,
/// `Optional<List<MapDecoration>>`, then the colour patch unchanged.
/// `MapDecoration`: VarInt type, i8 x, i8 y, i8 rotation, optional name.
pub fn map_item_data(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    static DECORATIONS: W = W::Opt(&W::List(&W::Struct(&[
        W::Reg(R::Static(Reg::MapDecorationType)),
        W::I8,
        W::I8,
        W::I8,
        W::Opt(&W::Text),
    ])));
    let mut r = Reader::new(body);
    out.put_varint(r.varint()?);
    out.put_bytes(r.bytes(2)?);
    x.value(&DECORATIONS, &mut r, out)?;
    out.put_bytes(r.rest());
    Ok(())
}

/// `boss_event`: UUID, VarInt operation; 0 (add) starts with the title,
/// 3 (update name) is the title alone; the others carry no text.
pub fn boss_event(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_bytes(r.bytes(16)?);
    let operation = r.varint()?;
    out.put_varint(operation);
    if operation == 0 || operation == 3 {
        x.value(&TEXT, &mut r, out)?;
    }
    out.put_bytes(r.rest());
    Ok(())
}

/// `NumberFormat` (score display): VarInt type, then 0 blank: nothing,
/// 1 styled: a `Style` as NBT (which can carry a hover), 2 fixed: a text.
fn number_format(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let kind = r.varint()?;
    out.put_varint(kind);
    match kind {
        0 => Ok(()),
        1 | 2 => x.value(&TEXT, r, out),
        _ => Err(Xerr::Wire(WireError::Malformed("unknown number format"))),
    }
}

fn optional_number_format(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    let present = r.bool()?;
    out.put_bool(present);
    if present {
        number_format(x, r, out)?;
    }
    Ok(())
}

/// `set_objective`: String name, i8 method (0 add, 1 remove, 2 update);
/// add and update carry the display name, VarInt render type and an
/// optional number format.
pub fn set_objective(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_str_bytes(r.str_bytes()?);
    let method = r.i8()?;
    out.put_i8(method);
    if method == 0 || method == 2 {
        x.value(&TEXT, &mut r, out)?;
        out.put_varint(r.varint()?);
        optional_number_format(x, &mut r, out)?;
    }
    out.put_bytes(r.rest());
    Ok(())
}

/// `set_score`: String owner, String objective, VarInt score, optional
/// display name, optional number format.
pub fn set_score(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_str_bytes(r.str_bytes()?);
    out.put_str_bytes(r.str_bytes()?);
    out.put_varint(r.varint()?);
    x.value(&OPT_TEXT, &mut r, out)?;
    optional_number_format(x, &mut r, out)
}

/// `player_info_update`: an `EnumSet<Action>` (one byte: eight actions, the
/// same in 1.21.11 to 26.3), then `List<{UUID, per set action in order}>`.
/// Only `UPDATE_DISPLAY_NAME` (bit 5) carries text; the others are copied:
///
/// ```text
/// 0 ADD_PLAYER           String(16) name, List<{String, String, Optional<String>}>
/// 1 INITIALIZE_CHAT      Optional<{UUID, i64, byte[], byte[]}>
/// 2 UPDATE_GAME_MODE     VarInt
/// 3 UPDATE_LISTED        Bool
/// 4 UPDATE_LATENCY       VarInt
/// 5 UPDATE_DISPLAY_NAME  Optional<Component>
/// 6 UPDATE_LIST_ORDER    VarInt
/// 7 UPDATE_HAT           Bool
/// ```
pub fn player_info_update(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    let actions = r.u8()?;
    out.put_u8(actions);
    let n = r.len()?;
    out.put_len(n);
    for _ in 0..n {
        out.put_bytes(r.bytes(16)?);
        for bit in 0..8 {
            if actions & (1 << bit) == 0 {
                continue;
            }
            match bit {
                0 => {
                    out.put_str_bytes(r.str_bytes()?);
                    let properties = r.len()?;
                    out.put_len(properties);
                    for _ in 0..properties {
                        out.put_str_bytes(r.str_bytes()?);
                        out.put_str_bytes(r.str_bytes()?);
                        x.value(&W::Opt(&W::Str), &mut r, out)?;
                    }
                }
                1 => {
                    let present = r.bool()?;
                    out.put_bool(present);
                    if present {
                        out.put_bytes(r.bytes(24)?);
                        x.value(&W::ByteArray, &mut r, out)?;
                        x.value(&W::ByteArray, &mut r, out)?;
                    }
                }
                2 | 4 | 6 => out.put_varint(r.varint()?),
                3 | 7 => out.put_u8(r.u8()?),
                _ => x.value(&OPT_TEXT, &mut r, out)?,
            }
        }
    }
    Ok(())
}

/// `resource_pack_push`: UUID, String url, String hash, Bool required,
/// optional prompt.
pub fn resource_pack_push(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    out.put_bytes(r.bytes(16)?);
    out.put_str_bytes(r.str_bytes()?);
    out.put_str_bytes(r.str_bytes()?);
    out.put_u8(r.u8()?);
    x.value(&OPT_TEXT, &mut r, out)
}

/// `server_links`: `List<{Either<VarInt known type, Component label>,
/// String url}>` (`ByteBufCodecs.either`: Bool, true for the left side).
pub fn server_links(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    static LINKS: W = W::List(&W::Struct(&[W::Either(&W::VarInt, &W::Text), W::Str]));
    x.value(&LINKS, &mut Reader::new(body), out)
}

/// `show_dialog` in play: `Holder<Dialog>` (VarInt id + 1, or 0 and the
/// dialog as NBT). Configuration sends the dialog itself as NBT
/// ([`dialog_nbt`]).
pub fn show_dialog(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    let mut r = Reader::new(body);
    let raw = r.varint()?;
    if raw != 0 {
        let id = u32::try_from(raw - 1).map_err(|_| WireError::Malformed("negative dialog id"))?;
        let mapped = x
            .map_id(R::Dynamic("minecraft:dialog"), id)
            .ok_or(Xerr::Absent("a dialog the client lacks"))?;
        out.put_varint(crate::wire::wire_id(mapped) + 1);
        return Ok(());
    }
    out.put_varint(0);
    dialog_nbt_from(x, &mut r, out)
}

/// An inline dialog (network NBT), its text and item bodies rewritten.
pub fn dialog_nbt(x: &mut Xc<'_>, body: &[u8], out: &mut Vec<u8>) -> Xresult<()> {
    dialog_nbt_from(x, &mut Reader::new(body), out)
}

fn dialog_nbt_from(x: &mut Xc<'_>, r: &mut Reader<'_>, out: &mut Vec<u8>) -> Xresult<()> {
    match crate::nbt::Tag::read(r)? {
        Some(mut dialog) => {
            crate::text::walk(x, &mut dialog);
            dialog.write(out);
        }
        None => out.put_u8(crate::nbt::TAG_END),
    }
    Ok(())
}
