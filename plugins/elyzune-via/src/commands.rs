//! The `commands` packet: the command graph.
//!
//! ```text
//! VarInt n, n × node, VarInt root
//! node: u8 flags (0x03 type: 0 root, 1 literal, 2 argument; 0x04 executable;
//!       0x08 redirect; 0x10 suggestions; 0x20 restricted),
//!       VarInt children × VarInt, [VarInt redirect],
//!       [String name if literal or argument],
//!       [VarInt parser, parser properties, if argument],
//!       [Identifier suggestions, if 0x10]
//! ```
//!
//! Parser properties (`ArgumentTypeInfos`, unchanged from 1.21.11 to 26.3 for
//! every parser a 26.2 server can send): numeric parsers have a flags byte
//! and optional min (bit 0) / max (bit 1) of their width; `brigadier:string`
//! a VarInt mode; `entity` and `score_holder` a flags byte; `time` an i32
//! minimum; the `resource*` parsers a registry identifier; all others none.
//!
//! Only parser ids change (26.3 inserted five). A parser the client lacks is
//! replaced by a greedy `brigadier:string`, keeping the node and its children
//! so the rest of the graph, and execution through the backend, still work.

use crate::generated::registries::{Reg, id_of, name_of};
use crate::session::ViaTranslator;
use crate::wire::{Put, Reader, WireError, WireResult, wire_id};

/// Copies the properties of parser `name` from `r` to `out`.
fn properties(name: &str, r: &mut Reader<'_>, out: &mut Vec<u8>) -> WireResult<()> {
    let width = match name {
        "brigadier:float" | "brigadier:integer" => Some(4),
        "brigadier:double" | "brigadier:long" => Some(8),
        _ => None,
    };
    if let Some(width) = width {
        let flags = r.u8()?;
        out.put_u8(flags);
        if flags & 1 != 0 {
            out.put_bytes(r.bytes(width)?);
        }
        if flags & 2 != 0 {
            out.put_bytes(r.bytes(width)?);
        }
        return Ok(());
    }
    match name {
        "brigadier:string" => out.put_varint(r.varint()?),
        "minecraft:entity" | "minecraft:score_holder" => out.put_u8(r.u8()?),
        "minecraft:time" => out.put_bytes(r.bytes(4)?),
        "minecraft:resource_or_tag"
        | "minecraft:resource_or_tag_key"
        | "minecraft:resource"
        | "minecraft:resource_key"
        | "minecraft:resource_selector" => out.put_str_bytes(r.str_bytes()?),
        _ => {}
    }
    Ok(())
}

/// Rewrites a 776 command graph for the client.
pub fn rewrite(t: &mut ViaTranslator, body: &[u8], out: &mut Vec<u8>) -> WireResult<()> {
    let client = t.client.number().unsigned_abs();
    let greedy_string = id_of(Reg::CommandArgumentType, client, "brigadier:string");
    let mut r = Reader::new(body);
    let n = r.len()?;
    out.put_len(n);
    let mut replaced = false;
    for _ in 0..n {
        let flags = r.u8()?;
        out.put_u8(flags);
        let children = r.len()?;
        out.put_len(children);
        for _ in 0..children {
            out.put_varint(r.varint()?);
        }
        if flags & 0x08 != 0 {
            out.put_varint(r.varint()?);
        }
        let kind = flags & 0x03;
        if kind == 1 || kind == 2 {
            out.put_str_bytes(r.str_bytes()?);
        }
        if kind == 2 {
            let parser =
                u32::try_from(r.varint()?).map_err(|_| WireError::Malformed("negative parser"))?;
            let name = name_of(Reg::CommandArgumentType, 776, parser)
                .ok_or(WireError::Malformed("unknown argument parser"))?;
            match t.tables.reg(Reg::CommandArgumentType).s2c(parser) {
                Some(mapped) => {
                    out.put_varint(wire_id(mapped));
                    properties(name, &mut r, out)?;
                }
                None => {
                    let mut sink = Vec::new();
                    properties(name, &mut r, &mut sink)?;
                    let string =
                        greedy_string.ok_or(WireError::Malformed("client has no string parser"))?;
                    out.put_varint(wire_id(string));
                    out.put_varint(2);
                    replaced = true;
                }
            }
        }
        if flags & 0x10 != 0 {
            out.put_str_bytes(r.str_bytes()?);
        }
    }
    out.put_varint(r.varint()?);
    if replaced {
        t.drop_unsupported(
            "minecraft:commands",
            "an argument parser the client lacks was sent as a string",
        );
    }
    Ok(())
}
