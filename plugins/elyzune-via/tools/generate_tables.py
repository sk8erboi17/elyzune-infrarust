#!/usr/bin/env python3
"""Generate elyzune-via's version tables from the vanilla server jars.

elyzune-via translates between a 26.2 backend (protocol 776) and clients on
1.21.11 (774), 26.1 (775) and 26.3 (777). Everything that is a fact about a
Minecraft release (packet ids, registry ids, block states, entity data layouts,
tags, synchronised registry entries, particle option kinds) is generated here
from Mojang's own artifacts, never transcribed by hand. Facts this script cannot
read off a jar (which new block looks most like an old one) are written below
as small named tables, each with the reason it exists.

Inputs (a directory prepared by ``tools/via_data.py``, never committed):

    <data>/gen/<release>/generated/reports/{packets,registries,blocks}.json
    <data>/gen/<release>/generated/data/minecraft/...    datapack registries, tags
    <data>/decomp/<release>/...                          CFR output (codec classes)
    <data>/ent/<release>/...                             CFR output (entity classes)
    <data>/proto/<release>.json                          hyperion's extract-protocol output

Output: Rust modules under ``src/generated/`` (``--out``). The output is a
total function of the inputs: running the generator twice gives identical
files, and a version bump is reviewed as the diff of those files.

Releases: 774 = 1.21.11, 775 = 26.1.2 (26.1 and 26.1.1 have byte-identical
reports), 776 = 26.2 (the backend), 777 = 26.3.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

RELEASES = {774: "1.21.11", 775: "26.1.2", 776: "26.2", 777: "26.3"}
SERVER = 776
CLIENTS = (774, 775, 777)

# Patch releases sharing a protocol number. A vanilla client reports its own
# release id as the version of the `minecraft:core` known pack, so the
# translator offers every id of the protocol and lets the client pick its own.
KNOWN_PACK_VERSIONS = {
    774: ["1.21.11"],
    775: ["26.1", "26.1.1", "26.1.2"],
    776: ["26.2"],
    777: ["26.3"],
}

# ---------------------------------------------------------------------------
# Fallbacks for entries a client release does not have.
#
# A backend running 26.2 can send blocks, items and entities that 1.21.11 and
# 26.1 never had. Dropping a block state is not an option (a chunk palette
# entry must name *some* state), so each new entry is shown as the closest
# older one: same shape (slab, stairs, wall, flower), similar colour. Property
# values carry over wherever the fallback has a property of the same name.
# Nothing in the Elyzune world uses these blocks today (the world is a
# converted 1.21.11 map); the table exists so a new block placed later renders
# as something sensible instead of air.
# ---------------------------------------------------------------------------
BLOCK_FALLBACK = {
    "minecraft:sulfur": "minecraft:sandstone",
    "minecraft:potent_sulfur": "minecraft:sandstone",
    "minecraft:chiseled_sulfur": "minecraft:chiseled_sandstone",
    "minecraft:polished_sulfur": "minecraft:smooth_sandstone",
    "minecraft:polished_sulfur_slab": "minecraft:smooth_sandstone_slab",
    "minecraft:polished_sulfur_stairs": "minecraft:smooth_sandstone_stairs",
    "minecraft:polished_sulfur_wall": "minecraft:sandstone_wall",
    "minecraft:sulfur_bricks": "minecraft:cut_sandstone",
    "minecraft:sulfur_brick_slab": "minecraft:cut_sandstone_slab",
    "minecraft:sulfur_brick_stairs": "minecraft:sandstone_stairs",
    "minecraft:sulfur_brick_wall": "minecraft:sandstone_wall",
    "minecraft:sulfur_slab": "minecraft:sandstone_slab",
    "minecraft:sulfur_stairs": "minecraft:sandstone_stairs",
    "minecraft:sulfur_wall": "minecraft:sandstone_wall",
    "minecraft:sulfur_spike": "minecraft:pointed_dripstone",
    "minecraft:cinnabar": "minecraft:red_sandstone",
    "minecraft:chiseled_cinnabar": "minecraft:chiseled_red_sandstone",
    "minecraft:polished_cinnabar": "minecraft:smooth_red_sandstone",
    "minecraft:polished_cinnabar_slab": "minecraft:smooth_red_sandstone_slab",
    "minecraft:polished_cinnabar_stairs": "minecraft:smooth_red_sandstone_stairs",
    "minecraft:polished_cinnabar_wall": "minecraft:red_sandstone_wall",
    "minecraft:cinnabar_bricks": "minecraft:cut_red_sandstone",
    "minecraft:cinnabar_brick_slab": "minecraft:cut_red_sandstone_slab",
    "minecraft:cinnabar_brick_stairs": "minecraft:red_sandstone_stairs",
    "minecraft:cinnabar_brick_wall": "minecraft:red_sandstone_wall",
    "minecraft:cinnabar_slab": "minecraft:red_sandstone_slab",
    "minecraft:cinnabar_stairs": "minecraft:red_sandstone_stairs",
    "minecraft:cinnabar_wall": "minecraft:red_sandstone_wall",
    "minecraft:golden_dandelion": "minecraft:dandelion",
    "minecraft:potted_golden_dandelion": "minecraft:potted_dandelion",
}

# Items follow their block where there is one; these are the item-only ones.
ITEM_FALLBACK = {
    "minecraft:music_disc_bounce": "minecraft:music_disc_cat",
    "minecraft:sulfur_cube_bucket": "minecraft:slime_ball",
    "minecraft:sulfur_cube_spawn_egg": "minecraft:slime_spawn_egg",
}

# Registries whose entries are matched by name across releases, plus the
# fallback table for names the client lacks. Anything else without a match is
# "absent" and the runtime decides per use (drop the entry, the component, the
# packet), logging once per connection.
REGISTRY_FALLBACK = {
    "minecraft:block": BLOCK_FALLBACK,
    "minecraft:item": {**BLOCK_FALLBACK, **ITEM_FALLBACK},
    "minecraft:entity_type": {"minecraft:sulfur_cube": "minecraft:slime"},
    # A renamed argument parser with the same properties.
    "minecraft:command_argument_type": {"minecraft:team_color": "minecraft:color"},
    # Data-less particles stand in for the 26.2 sulfur/geyser ones. The
    # translator still drops the source's option data when the fallback takes
    # none, see particles.rs.
    "minecraft:particle_type": {
        "minecraft:geyser": "minecraft:cloud",
        "minecraft:geyser_base": "minecraft:cloud",
        "minecraft:geyser_plume": "minecraft:cloud",
        "minecraft:geyser_poof": "minecraft:poof",
        "minecraft:noxious_gas": "minecraft:smoke",
        "minecraft:noxious_gas_cloud": "minecraft:smoke",
        "minecraft:sulfur_bubbles": "minecraft:bubble",
        "minecraft:sulfur_cube_goo": "minecraft:item_slime",
        "minecraft:pause_mob_growth": "minecraft:happy_villager",
        "minecraft:reset_mob_growth": "minecraft:happy_villager",
    },
}

# The bed block entity was removed in 26.2: 774 and 775 clients still render a
# bed only when its block entity exists, so the translator adds one per bed
# block (ViaBackwards does the same).
BED_BLOCK_SUFFIX = "_bed"

# Static registries the translator remaps on the wire. Each gets a 776<->client
# table; identical registries get none and cost nothing at runtime.
WIRE_REGISTRIES = [
    "minecraft:attribute",
    "minecraft:block",
    "minecraft:block_entity_type",
    "minecraft:command_argument_type",
    "minecraft:consume_effect_type",
    "minecraft:custom_stat",
    "minecraft:data_component_predicate_type",
    "minecraft:data_component_type",
    "minecraft:entity_type",
    "minecraft:fluid",
    "minecraft:game_event",
    "minecraft:item",
    "minecraft:map_decoration_type",
    "minecraft:menu",
    "minecraft:mob_effect",
    "minecraft:particle_type",
    "minecraft:point_of_interest_type",
    "minecraft:position_source_type",
    "minecraft:potion",
    "minecraft:recipe_book_category",
    "minecraft:recipe_display",
    "minecraft:recipe_serializer",
    "minecraft:slot_display",
    "minecraft:sound_event",
    "minecraft:stat_type",
    "minecraft:villager_profession",
    "minecraft:villager_type",
]

# Registries whose entry names the runtime needs per protocol (`id_of`,
# `name_of`): entries handled by name in packets, and every registry whose
# entries appear by name in NBT (items, entities, components and block entity
# types inside text hovers, block entities and item components).
NAMED_REGISTRIES = [
    "minecraft:entity_type", "minecraft:data_component_type", "minecraft:block_entity_type",
    "minecraft:particle_type", "minecraft:slot_display",
    "minecraft:consume_effect_type", "minecraft:position_source_type",
    "minecraft:recipe_display", "minecraft:menu", "minecraft:stat_type",
    "minecraft:command_argument_type", "minecraft:item", "minecraft:map_decoration_type",
    "minecraft:mob_effect", "minecraft:attribute", "minecraft:block", "minecraft:sound_event",
]

# Static registries that never travel by id but whose entry names appear in
# inline registry data: an environment attribute the client lacks fails the
# whole dimension type or biome, so the translator removes it by name.
NAME_ONLY_REGISTRIES = [
    "minecraft:environment_attribute",
]

STATES = [
    ("handshake", "Handshake"),
    ("status", "Status"),
    ("login", "Login"),
    ("configuration", "Config"),
    ("play", "Play"),
]
DIRECTIONS = [("clientbound", "Cb"), ("serverbound", "Sb")]


def die(message: str) -> None:
    print(f"generate_tables: {message}", file=sys.stderr)
    sys.exit(1)


def pascal(name: str) -> str:
    """`minecraft:debug/block_value` -> `DebugBlockValue`."""
    bare = name.split(":", 1)[1]
    return "".join(part.capitalize() for part in re.split(r"[/_]", bare) if part)


def const_name(name: str) -> str:
    bare = name.split(":", 1)[1] if ":" in name else name
    return re.sub(r"[^A-Za-z0-9]", "_", bare).upper()


def rust_str(s: str) -> str:
    return json.dumps(s)


def arr(elements, indent: int = 4, width: int = 100) -> str:
    """A Rust array literal, wrapped at `width` columns.

    The generated modules are excluded from rustfmt (which would put every
    element on a line of its own and triple their size), so the generator
    lays them out itself: short lists on one line, long ones filled to the
    width, one indented block per array.
    """
    items = [str(e) for e in elements]
    flat = "[" + ", ".join(items) + "]"
    if len(flat) <= width - indent:
        return flat
    pad = " " * (indent + 4)
    lines, line = [], ""
    for item in items:
        piece = item + ","
        if line and len(pad) + len(line) + 1 + len(piece) > width:
            lines.append(pad + line)
            line = piece
        else:
            line = piece if not line else line + " " + piece
    if line:
        lines.append(pad + line)
    return "[\n" + "\n".join(lines) + "\n" + " " * indent + "]"


class Release:
    def __init__(self, data: Path, protocol: int):
        self.protocol = protocol
        self.name = RELEASES[protocol]
        root = data / "gen" / self.name / "generated"
        reports = root / "reports"
        self.packets = json.loads((reports / "packets.json").read_text())
        self.registries = json.loads((reports / "registries.json").read_text())
        self.blocks = json.loads((reports / "blocks.json").read_text())
        self.datadir = root / "data" / "minecraft"
        self.decomp = data / "decomp" / (self.name if protocol != 774 else "1.21.11-named")
        self.entities = data / "ent" / self.name
        proto = data / "proto" / f"{self.name}.json"
        self.proto = json.loads(proto.read_text()) if proto.exists() else None

    def registry(self, name: str) -> list[str] | None:
        reg = self.registries.get(name)
        if reg is None:
            return None
        entries = sorted(reg["entries"].items(), key=lambda kv: kv[1]["protocol_id"])
        names = [n for n, _ in entries]
        for index, (_, entry) in enumerate(entries):
            if entry["protocol_id"] != index:
                die(f"{self.name} {name}: ids are not dense at {index}")
        return names

    def packet_names(self, state: str, direction: str) -> list[str]:
        table = self.packets.get(state, {}).get(direction, {})
        ordered = sorted(table.items(), key=lambda kv: kv[1]["protocol_id"])
        for index, (name, entry) in enumerate(ordered):
            if entry["protocol_id"] != index:
                die(f"{self.name} {state}/{direction}: packet ids are not dense at {index} ({name})")
        return [n for n, _ in ordered]

    def block_states(self) -> list[tuple[str, dict[str, str]]]:
        """Every block state in global palette order: (block, properties)."""
        states: dict[int, tuple[str, dict[str, str]]] = {}
        for block, body in self.blocks.items():
            for state in body["states"]:
                states[state["id"]] = (block, state.get("properties", {}))
        if sorted(states) != list(range(len(states))):
            die(f"{self.name}: block state ids are not dense")
        return [states[i] for i in range(len(states))]

    def default_state(self, block: str) -> int:
        for state in self.blocks[block]["states"]:
            if state.get("default"):
                return state["id"]
        die(f"{self.name}: {block} has no default state")
        return 0

    def datapack_entries(self, path: str) -> list[str] | None:
        base = self.datadir / path
        if not base.is_dir():
            return None
        names = []
        for dirpath, _, files in os.walk(base):
            for f in files:
                if f.endswith(".json"):
                    rel = os.path.relpath(os.path.join(dirpath, f), base)[: -len(".json")]
                    names.append("minecraft:" + rel.replace(os.sep, "/"))
        return sorted(names)

    def tags(self) -> dict[str, dict[str, list]]:
        """`{registry path: {tag name: raw values}}` from the vanilla data pack."""
        base = self.datadir / "tags"
        out: dict[str, dict[str, list]] = {}
        for dirpath, _, files in os.walk(base):
            for f in files:
                if not f.endswith(".json"):
                    continue
                rel = os.path.relpath(os.path.join(dirpath, f), base)[: -len(".json")].replace(os.sep, "/")
                values = json.loads(Path(dirpath, f).read_text())["values"]
                # The registry path can itself contain '/', e.g. worldgen/biome.
                for registry in self._tag_registries():
                    if rel.startswith(registry + "/"):
                        out.setdefault(registry, {})["minecraft:" + rel[len(registry) + 1 :]] = values
                        break
        return out

    def _tag_registries(self) -> list[str]:
        base = self.datadir / "tags"
        regs = []
        for top in sorted(os.listdir(base)):
            if top == "worldgen":
                for sub in sorted(os.listdir(base / top)):
                    regs.append(f"worldgen/{sub}")
            else:
                regs.append(top)
        # Longest first so `worldgen/biome` wins over a hypothetical `worldgen`.
        return sorted(regs, key=len, reverse=True)


def synchronized_registries(release: Release) -> list[str]:
    """`RegistryDataLoader.SYNCHRONIZED_REGISTRIES`, in declaration order."""
    path = release.decomp / "net/minecraft/resources/RegistryDataLoader.java"
    if path.exists():
        m = re.search(r"SYNCHRONIZED_REGISTRIES = List\.of\((.*?)\);\n", path.read_text(), re.S)
        if m:
            keys = re.findall(r"Registries\.([A-Z_]+)", m.group(1))
            return [registry_key_name(k) for k in keys]
    die(f"{release.name}: SYNCHRONIZED_REGISTRIES not found in {path}")
    return []


def registry_key_name(constant: str) -> str:
    special = {"BIOME": "worldgen/biome", "BLOCK_STATE_PROVIDER": "worldgen/block_state_provider"}
    return "minecraft:" + special.get(constant, constant.lower())


# ---------------------------------------------------------------------------
# Runs: a remap table stored as maximal runs of consecutive ids.
# ---------------------------------------------------------------------------


def runs_of(mapping: list[int | None]) -> list[tuple[int, int, int]]:
    """[(from_start, to_start, len)] covering every mapped entry."""
    out: list[tuple[int, int, int]] = []
    for src, dst in enumerate(mapping):
        if dst is None:
            continue
        if out:
            s, d, n = out[-1]
            if s + n == src and d + n == dst:
                out[-1] = (s, d, n + 1)
                continue
        out.append((src, dst, 1))
    return out


def emit_runs(name: str, runs: list[tuple[int, int, int]]) -> str:
    body = arr([f"({s}, {d}, {n})" for s, d, n in runs], 0)
    return f"pub static {name}: &[(u32, u32, u32)] = &{body};\n"


# ---------------------------------------------------------------------------
# Packets
# ---------------------------------------------------------------------------


def gen_packets(rel: dict[int, Release]) -> str:
    out = [HEADER, "//! Packet ids of every state and direction, per protocol.\n",
           "use super::Proto;\n\n"]
    for state, state_rust in STATES:
        for direction, dir_rust in DIRECTIONS:
            enum = f"{state_rust}{dir_rust}"
            per = {p: rel[p].packet_names(state, direction) for p in rel}
            union: list[str] = []
            for p in sorted(per):
                for n in per[p]:
                    if n not in union:
                        union.append(n)
            if not union:
                continue
            union.sort()
            out.append(f"/// `{state}` {direction} packets of every supported protocol.\n")
            out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\n#[repr(u16)]\n")
            out.append(f"pub enum {enum} {{\n")
            for n in union:
                out.append(f"    /// `{n}`\n    {pascal(n)},\n")
            out.append("}\n\n")
            for p in sorted(per):
                kinds = arr((f"{enum}::{pascal(n)}" for n in per[p]), 0)
                out.append(f"static {const_name(enum)}_BY_ID_{p}: &[{enum}] = &{kinds};\n")
                ids = arr((str(per[p].index(n)) if n in per[p] else "-1" for n in union), 0)
                out.append(f"static {const_name(enum)}_ID_{p}: &[i16] = &{ids};\n")
            names = arr((rust_str(n) for n in union), 0)
            out.append(f"static {const_name(enum)}_NAMES: &[&str] = &{names};\n\n")
            out.append(f"impl {enum} {{\n")
            out.append("    /// The packet with wire id `id` in protocol `proto`.\n")
            out.append("    pub fn from_id(proto: Proto, id: i32) -> Option<Self> {\n")
            out.append("        let table = match proto {\n")
            for p in sorted(per):
                out.append(f"            Proto::V{p} => {const_name(enum)}_BY_ID_{p},\n")
            out.append("        };\n        usize::try_from(id).ok().and_then(|i| table.get(i)).copied()\n    }\n\n")
            out.append("    /// The wire id of this packet in protocol `proto`, if it exists there.\n")
            out.append("    pub fn id(self, proto: Proto) -> Option<i32> {\n")
            out.append("        let table = match proto {\n")
            for p in sorted(per):
                out.append(f"            Proto::V{p} => {const_name(enum)}_ID_{p},\n")
            out.append("        };\n        let id = table[self as usize];\n        (id >= 0).then_some(i32::from(id))\n    }\n\n")
            out.append("    /// The resource name, for logs.\n")
            out.append(f"    pub fn name(self) -> &'static str {{\n        {const_name(enum)}_NAMES[self as usize]\n    }}\n}}\n\n")
    return "".join(out)


# ---------------------------------------------------------------------------
# Static registries
# ---------------------------------------------------------------------------


def registry_mapping(server: list[str], client: list[str], fallback: dict[str, str],
                     fallback_up: dict[str, str] | None = None) -> tuple[list, list, list, list]:
    index = {n: i for i, n in enumerate(client)}
    s2c: list[int | None] = []
    fallen: list[str] = []
    for n in server:
        if n in index:
            s2c.append(index[n])
        elif n in fallback and fallback[n] in index:
            s2c.append(index[fallback[n]])
            fallen.append(n)
        else:
            s2c.append(None)
    sindex = {n: i for i, n in enumerate(server)}
    c2s: list[int | None] = []
    approximated: list[int] = []
    for i, n in enumerate(client):
        if n in sindex:
            c2s.append(sindex[n])
        elif fallback_up and n in fallback_up and fallback_up[n] in sindex:
            c2s.append(sindex[fallback_up[n]])
            approximated.append(i)
        else:
            c2s.append(None)
    return s2c, c2s, fallen, approximated


def item_fallback_up(client_items: list[str], server_items: list[str]) -> dict[str, str]:
    """The closest 26.2 item for each item only a newer client has.

    A 26.3 client can create such an item itself (the creative inventory)
    and send it; 26.2 has no id for it, so it reaches the backend as the
    closest item 26.2 has, by the rules below (same colour and material for
    the new slabs, stairs and cushions; birch for the poplar wood set;
    `filled_map` for the 26.3 explorer map items). A newer item no rule
    covers stops the build, so it cannot become air unnoticed.
    """
    server = set(server_items)
    out: dict[str, str] = {}
    explicit = {
        "minecraft:red_shrub": "minecraft:bush",
        "minecraft:shelf_mushroom": "minecraft:brown_mushroom",
        "minecraft:straw_bed": "minecraft:yellow_bed",
        "minecraft:orange_poplar_leaves": "minecraft:birch_leaves",
        "minecraft:red_poplar_leaves": "minecraft:birch_leaves",
        "minecraft:yellow_poplar_leaves": "minecraft:birch_leaves",
    }
    for name in client_items:
        if name in server:
            continue
        path = name.removeprefix("minecraft:")
        if name in explicit:
            target = explicit[name]
        elif m := re.fullmatch(r"(\w+?)_(concrete|wool)_(slab|stairs)", path):
            target = f"minecraft:{m.group(1)}_{m.group(2)}"
        elif m := re.fullmatch(r"(\w+?)_cushion", path):
            target = f"minecraft:{m.group(1)}_carpet"
        elif "poplar" in path:
            target = "minecraft:" + path.replace("poplar", "birch")
        elif path.endswith("_map"):
            target = "minecraft:filled_map"
        else:
            die(f"no 26.2 stand-in for the newer item {name}")
        if target not in server:
            die(f"stand-in {target} for {name} is not a 26.2 item")
        out[name] = target
    return out


def gen_registries(rel: dict[int, Release], report: list[str]) -> str:
    out = [HEADER, "//! Static registry id remaps between 776 and each client protocol.\n",
           "//!\n//! A table is a list of runs `(from, to, len)`: ids `from..from+len` map to\n",
           "//! `to..to+len`. Ids covered by no run have no equivalent on the other side.\n",
           "//! Registries identical in two protocols have no table at all.\n\n"]
    enum_entries = []
    for reg in WIRE_REGISTRIES:
        server = rel[SERVER].registry(reg)
        if server is None:
            die(f"776 lacks {reg}")
        enum_entries.append(reg)
    out.append("/// The static registries the translator remaps.\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\npub enum Reg {\n")
    for reg in enum_entries:
        out.append(f"    /// `{reg}`\n    {pascal(reg)},\n")
    out.append("}\n\n")
    out.append("impl Reg {\n    pub const ALL: &'static [Reg] = &")
    out.append(arr((f"Reg::{pascal(r)}" for r in enum_entries), 4))
    out.append(";\n\n    pub fn name(self) -> &'static str {\n        match self {\n")
    for reg in enum_entries:
        out.append(f"            Reg::{pascal(reg)} => {rust_str(reg)},\n")
    out.append("        }\n    }\n}\n\n")

    # (registry, client) -> (s2c runs name, c2s runs name, server len, client len)
    out.append("/// `(server->client runs, client->server runs, server count, client count)`,\n")
    out.append("/// or `None` when the registry is identical in both protocols.\n")
    out.append("pub type RegTables = Option<(&'static [(u32, u32, u32)], &'static [(u32, u32, u32)], u32, u32)>;\n\n")
    tables = {}
    approximated_ids: dict[tuple[str, int], list[int]] = {}
    for reg in enum_entries:
        server = rel[SERVER].registry(reg)
        for client in CLIENTS:
            names = rel[client].registry(reg)
            if names is None:
                die(f"{rel[client].name} lacks {reg}")
            if names == server:
                tables[(reg, client)] = None
                continue
            up = item_fallback_up(names, server) if reg == "minecraft:item" else None
            s2c, c2s, fallen, approximated = registry_mapping(server, names, REGISTRY_FALLBACK.get(reg, {}), up)
            missing = [server[i] for i, d in enumerate(s2c) if d is None]
            report.append(f"{reg} 776->{client}: {len(fallen)} fallback, {len(missing)} absent {missing[:8]}")
            if up:
                report.append(f"{reg} {client}->776: {len(approximated)} newer entries shown as the closest 26.2 one")
            base = f"{const_name(reg)}_{client}"
            out.append(emit_runs(f"{base}_S2C", runs_of(s2c)))
            out.append(emit_runs(f"{base}_C2S", runs_of(c2s)))
            if approximated:
                approximated_ids[(reg, client)] = approximated
            tables[(reg, client)] = (base, len(server), len(names))
    out.append("/// Client ids that reach the backend as the closest 26.2 entry rather\n")
    out.append("/// than an equivalent (sorted), per registry and client protocol.\n")
    out.append("pub fn approximated_c2s(reg: Reg, client: u32) -> &'static [u32] {\n    match (reg, client) {\n")
    for (reg, client) in sorted(approximated_ids):
        out.append(f"        (Reg::{pascal(reg)}, {client}) => {const_name(reg)}_{client}_APPROXIMATED,\n")
    out.append("        _ => &[],\n    }\n}\n\n")
    for (reg, client), ids in sorted(approximated_ids.items()):
        out.append(f"static {const_name(reg)}_{client}_APPROXIMATED: &[u32] = &{arr((str(i) for i in sorted(ids)), 0)};\n")
    # Sounds some client lacks, by 776 id, so the translator can send them
    # inline by name (a resource pack may still define them).
    server_sounds = rel[SERVER].registry("minecraft:sound_event")
    absent = set()
    for client in CLIENTS:
        names = set(rel[client].registry("minecraft:sound_event"))
        absent |= {i for i, n in enumerate(server_sounds) if n not in names}
    out.append("/// 776 sound events missing from at least one client release, by 776 id.\n")
    out.append("static ABSENT_SOUNDS_776: &[(u32, &str)] = &")
    out.append(arr((f"({i}, {rust_str(server_sounds[i])})" for i in sorted(absent)), 0))
    out.append(";\n\n")
    out.append("/// The name of 776 sound `id`, when some client release lacks it.\n")
    out.append("pub fn sound_name_776(id: u32) -> Option<&'static str> {\n")
    out.append("    ABSENT_SOUNDS_776.binary_search_by_key(&id, |(i, _)| *i).ok().map(|i| ABSENT_SOUNDS_776[i].1)\n}\n\n")
    out.append("\n/// The tables of `reg` for client protocol `client` (774, 775 or 777).\n")
    out.append("pub fn tables(reg: Reg, client: u32) -> RegTables {\n    match (reg, client) {\n")
    for (reg, client), t in tables.items():
        if t is None:
            continue
        base, sl, cl = t
        out.append(f"        (Reg::{pascal(reg)}, {client}) => Some(({base}_S2C, {base}_C2S, {sl}, {cl})),\n")
    out.append("        _ => None,\n    }\n}\n\n")
    # Name lookups the runtime needs for a few well-known entries.
    out.append("/// Ids of entries the translator handles by name, per protocol.\n")
    out.append("pub fn id_of(reg: Reg, proto: u32, name: &str) -> Option<u32> {\n")
    out.append("    let names: &[&str] = match (reg, proto) {\n")
    for reg in NAMED_REGISTRIES:
        for p in sorted(rel):
            out.append(f"        (Reg::{pascal(reg)}, {p}) => {const_name(reg)}_NAMES_{p},\n")
    out.append("        _ => return None,\n    };\n")
    out.append("    names.iter().position(|n| *n == name).and_then(|i| u32::try_from(i).ok())\n}\n\n")
    out.append("/// The name of entry `id`, for the registries [`id_of`] knows.\n")
    out.append("pub fn name_of(reg: Reg, proto: u32, id: u32) -> Option<&'static str> {\n")
    out.append("    let names: &[&str] = match (reg, proto) {\n")
    for reg in NAMED_REGISTRIES:
        for p in sorted(rel):
            out.append(f"        (Reg::{pascal(reg)}, {p}) => {const_name(reg)}_NAMES_{p},\n")
    out.append("        _ => return None,\n    };\n")
    out.append("    names.get(usize::try_from(id).ok()?).copied()\n}\n\n")
    for reg in NAMED_REGISTRIES:
        for p in sorted(rel):
            names = rel[p].registry(reg)
            out.append(f"static {const_name(reg)}_NAMES_{p}: &[&str] = &{arr((rust_str(n) for n in names), 0)};\n")
    # Static registries never sent by id that NBT names entries of: sorted
    # name lists, for membership tests (`static_has`).
    out.append("\n/// Sorted entry names of static registries that appear only by name in NBT.\n")
    out.append("pub fn static_names(registry: &str, proto: u32) -> Option<&'static [&'static str]> {\n")
    out.append("    Some(match (registry, proto) {\n")
    for reg in NAME_ONLY_REGISTRIES:
        for p in sorted(rel):
            out.append(f"        ({rust_str(reg)}, {p}) => {const_name(reg)}_SORTED_{p},\n")
    out.append("        _ => return None,\n    })\n}\n\n")
    for reg in NAME_ONLY_REGISTRIES:
        for p in sorted(rel):
            names = sorted(rel[p].registry(reg))
            out.append(f"static {const_name(reg)}_SORTED_{p}: &[&str] = &{arr((rust_str(n) for n in names), 0)};\n")
    return "".join(out)


# ---------------------------------------------------------------------------
# Block states
# ---------------------------------------------------------------------------


def gen_blocks(rel: dict[int, Release], report: list[str]) -> str:
    out = [HEADER, "//! Block state remaps, palette widths and bed states.\n\n"]
    server_states = rel[SERVER].block_states()
    for p in sorted(rel):
        count = len(rel[p].block_states())
        bits = (count - 1).bit_length()
        out.append(f"/// Block states in protocol {p}; a direct palette needs {bits} bits.\n")
        out.append(f"pub const STATE_COUNT_{p}: u32 = {count};\n")
        out.append(f"pub const STATE_BITS_{p}: u8 = {bits};\n")
    for client in CLIENTS:
        cr = rel[client]
        client_states = cr.block_states()
        lookup = {}
        for i, (block, props) in enumerate(client_states):
            lookup[(block, tuple(sorted(props.items())))] = i
        mapping: list[int | None] = []
        approximated = 0
        for block, props in server_states:
            key = (block, tuple(sorted(props.items())))
            if key in lookup:
                mapping.append(lookup[key])
                continue
            target = block if block in cr.blocks else BLOCK_FALLBACK.get(block)
            if target is None or target not in cr.blocks:
                die(f"776 block {block} has no equivalent in {cr.name}; add a BLOCK_FALLBACK entry")
            # Same block with different properties, or a fallback block: keep
            # every property the target has, default the rest.
            best = cr.default_state(target)
            best_score = -1
            for state in cr.blocks[target]["states"]:
                sp = state.get("properties", {})
                score = sum(1 for k, v in props.items() if sp.get(k) == v)
                if score > best_score:
                    best, best_score = state["id"], score
            mapping.append(best)
            approximated += 1
        report.append(f"block states 776->{client}: {approximated} approximated of {len(server_states)}")
        out.append(emit_runs(f"STATES_{client}_S2C", runs_of(mapping)))
    beds = [i for i, (block, _) in enumerate(server_states) if block.endswith(BED_BLOCK_SUFFIX)]
    bed_runs = runs_of([i if i in set(beds) else None for i in range(len(server_states))])
    out.append("/// 776 block states that are beds, as `(first, _, count)` runs.\n")
    out.append(emit_runs("BED_STATES_776", bed_runs))
    return "".join(out)


# ---------------------------------------------------------------------------
# Synchronised (data-driven) registries and tags
# ---------------------------------------------------------------------------


def gen_dynamic(rel: dict[int, Release], report: list[str]) -> str:
    out = [HEADER, "//! Synchronised registries each protocol knows, with their vanilla entries.\n",
           "//!\n//! The backend sends entry names only (the client accepted the vanilla core\n",
           "//! pack), so a client can resolve exactly the entries its own release ships.\n\n"]
    out.append("/// `(registry, sorted vanilla entries)` of every synchronised registry.\n")
    out.append("pub type Synced = &'static [(&'static str, &'static [&'static str])];\n\n")
    for p in sorted(rel):
        regs = synchronized_registries(rel[p])
        out.append(f"/// Protocol {p}, in `SYNCHRONIZED_REGISTRIES` order.\n")
        out.append(f"pub static SYNCED_{p}: Synced = &[\n")
        for reg in regs:
            entries = rel[p].datapack_entries(reg.split(":", 1)[1])
            if entries is None:
                die(f"{rel[p].name}: no data for synchronised registry {reg}")
            out.append(f"    ({rust_str(reg)}, &{arr((rust_str(e) for e in entries), 4)}),\n")
        out.append("];\n\n")
        report.append(f"synchronised registries {p}: {len(regs)}")
    out.append("/// Ids the `minecraft:core` known pack can carry for each protocol.\n")
    for p in sorted(rel):
        out.append(f"pub static CORE_PACK_{p}: &[&str] = &{arr((rust_str(v) for v in KNOWN_PACK_VERSIONS[p]), 0)};\n")
    return "".join(out)


def expand_tag(values: list, tags: dict[str, list], seen: frozenset = frozenset()) -> list[str]:
    out: list[str] = []
    for value in values:
        required = True
        if isinstance(value, dict):
            required = value.get("required", True)
            value = value["id"]
        if value.startswith("#"):
            name = value[1:]
            if name in seen:
                die(f"tag cycle through {name}")
            if name not in tags:
                if required:
                    die(f"tag references missing tag {name}")
                continue
            out.extend(expand_tag(tags[name], tags, seen | {name}))
        else:
            out.append(value if ":" in value else "minecraft:" + value)
    # Keep first occurrence order; duplicates are harmless but wasteful.
    seen_entries = set()
    return [e for e in out if not (e in seen_entries or seen_entries.add(e))]


def gen_tags(rel: dict[int, Release], report: list[str]) -> str:
    """Vanilla tags each client release has and the 26.2 data pack does not.

    The backend sends 26.2's tag set. A client throws its own tags away when
    `update_tags` arrives, and 26.3 moved several block behaviours that used to
    be hard-coded (`blocks_motion`, `causes_suffocation`, ...) into tags, while
    1.21.11 uses older names for some 26.2 tags. Tags the client knows and the
    server does not send are therefore filled in from the client's own vanilla
    data. Static-registry entries are emitted as client ids; data-driven
    registry entries as names, resolved against the entries the backend sent.
    """
    out = [HEADER, "//! Client vanilla tags absent from the 26.2 data pack.\n\n"]
    server_tags = rel[SERVER].tags()
    out.append("/// A tag the client's release ships: `(registry, tag, entries)`.\n")
    out.append("pub enum TagEntries {\n    /// Ids in the client's numbering of a static registry.\n    Ids(&'static [u32]),\n")
    out.append("    /// Names in a synchronised registry, resolved at runtime.\n    Names(&'static [&'static str]),\n}\n\n")
    for client in CLIENTS:
        ctags = rel[client].tags()
        out.append(f"pub static EXTRA_TAGS_{client}: &[(&str, &str, TagEntries)] = &[\n")
        count = 0
        for registry in sorted(ctags):
            reg_name = "minecraft:" + registry
            static_names = rel[client].registry(reg_name)
            for tag in sorted(ctags[registry]):
                if tag in server_tags.get(registry, {}):
                    continue
                entries = expand_tag(ctags[registry][tag], ctags[registry])
                if static_names is not None:
                    index = {n: i for i, n in enumerate(static_names)}
                    ids = [index[e] for e in entries if e in index]
                    body = f"TagEntries::Ids(&{arr(ids, 4)})"
                else:
                    body = f"TagEntries::Names(&{arr((rust_str(e) for e in entries), 4)})"
                out.append(f"    ({rust_str(reg_name)}, {rust_str(tag)}, {body}),\n")
                count += 1
        out.append("];\n\n")
        report.append(f"extra client tags {client}: {count}")
    return "".join(out)



# ---------------------------------------------------------------------------
# Entity data
#
# `SynchedEntityData.defineId(Owner.class, serializer)` hands out indices per
# class in static-initialiser order, continuing from the superclass's last
# index. An entity type's layout is therefore the concatenation, from `Entity`
# down, of each class's `defineId` fields in declaration order. Indices shift
# whenever any ancestor gains a field (26.1 added `AgeableMob.AGE_LOCKED`, 26.2
# made slimes ageable), so the remap is computed per entity type by matching
# fields across releases.
# ---------------------------------------------------------------------------

CLASS_RE = re.compile(r"\b(?:class|record)\s+(\w+)(?:<[^{]*?>)?\s+extends\s+([\w.]+)")
DEFINE_RE = re.compile(
    r"(\w+)\s*=\s*SynchedEntityData\.defineId\(\s*([\w.]+)\.class\s*,\s*EntityDataSerializers\.(\w+)\s*\)"
)


def entity_classes(release: Release) -> tuple[dict[str, str], dict[str, list[tuple[str, str]]]]:
    extends: dict[str, str] = {}
    defines: dict[str, list[tuple[str, str]]] = {}
    for dirpath, _, files in os.walk(release.entities):
        for f in sorted(files):
            if not f.endswith(".java"):
                continue
            text = Path(dirpath, f).read_text()
            for m in CLASS_RE.finditer(text):
                child, parent = m.group(1), m.group(2).split(".")[-1]
                if child in extends and extends[child] != parent:
                    die(f"{release.name}: two entity classes named {child}")
                extends[child] = parent
            for m in DEFINE_RE.finditer(text):
                field, owner, serializer = m.group(1), m.group(2).split(".")[-1], m.group(3)
                defines.setdefault(owner, []).append((field, serializer))
    return extends, defines


def entity_type_classes(release: Release) -> dict[str, str]:
    base = release.entities / "net/minecraft/world/entity"
    out: dict[str, str] = {}
    text = (base / "EntityType.java").read_text()
    for m in re.finditer(r'EntityType<([\w.]+)>\s+\w+\s*=\s*EntityType\.register\("([\w/]+)"', text):
        out["minecraft:" + m.group(2)] = m.group(1).split(".")[-1]
    if (base / "EntityTypes.java").exists():
        ids = {
            m.group(1): m.group(2)
            for m in re.finditer(r'(\w+)\s*=\s*EntityTypeIds\.create\("([\w/]+)"\)', (base / "EntityTypeIds.java").read_text())
        }
        for m in re.finditer(r"EntityType<([\w.]+)>\s+\w+\s*=\s*EntityTypes\.register\(EntityTypeIds\.(\w+)", (base / "EntityTypes.java").read_text()):
            out["minecraft:" + ids[m.group(2)]] = m.group(1).split(".")[-1]
    return out


def entity_layouts(release: Release) -> dict[str, list[tuple[str, str, str]]]:
    """`{entity type: [(owner class, field, serializer)]}` in index order."""
    extends, defines = entity_classes(release)
    out = {}
    types = entity_type_classes(release)
    for name in release.registry("minecraft:entity_type"):
        cls = types.get(name)
        if cls is None:
            die(f"{release.name}: entity type {name} has no class")
        chain = []
        while cls is not None:
            chain.append(cls)
            if cls == "Entity":
                break
            cls = extends.get(cls)
        if chain[-1] != "Entity":
            die(f"{release.name}: {name} does not reach Entity: {chain}")
        fields = []
        for owner in reversed(chain):
            fields += [(owner, f, s) for f, s in defines.get(owner, [])]
        out[name] = fields
    return out


def serializers(release: Release) -> list[str]:
    path = release.decomp / "net/minecraft/network/syncher/EntityDataSerializers.java"
    text = path.read_text()
    names = re.findall(r"registerSerializer\((\w+)\)", text.split("static {", 1)[-1])
    if not names:
        die(f"{release.name}: no serializer registrations in {path}")
    return names


def match_fields(server: list, client: list) -> list[int | None]:
    """Server index -> client index, by (owner, field), then by unique field name."""
    by_owner = {(o, f): i for i, (o, f, _) in enumerate(client)}
    names: dict[str, list[int]] = {}
    for i, (_, f, _) in enumerate(client):
        names.setdefault(f, []).append(i)
    server_names: dict[str, int] = {}
    for _, f, _ in server:
        server_names[f] = server_names.get(f, 0) + 1
    out: list[int | None] = []
    for owner, field, _ in server:
        if (owner, field) in by_owner:
            out.append(by_owner[(owner, field)])
        elif len(names.get(field, [])) == 1 and server_names[field] == 1:
            out.append(names[field][0])
        else:
            out.append(None)
    return out


def gen_entity_data(rel: dict[int, Release], report: list[str]) -> str:
    out = [HEADER, "//! Entity data serializers and per-entity-type index remaps.\n\n"]
    sers = {p: serializers(rel[p]) for p in rel}
    union: list[str] = []
    for p in sorted(sers):
        for n in sers[p]:
            if n not in union:
                union.append(n)
    out.append("/// Entity data serializers of every supported protocol, by Mojang's\n/// constant name in `EntityDataSerializers`.\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\n#[allow(non_camel_case_types)]\npub enum Ser {\n")
    for n in union:
        out.append(f"    {n},\n")
    out.append("}\n\n")
    for p in sorted(sers):
        out.append(f"/// Serializer ids of protocol {p}, by id.\n")
        out.append(f"pub static SERIALIZERS_{p}: &[Ser] = &{arr(('Ser::' + n for n in sers[p]), 0)};\n")
    layouts = {p: entity_layouts(rel[p]) for p in rel}
    server_types = rel[SERVER].registry("minecraft:entity_type")
    out.append("\n/// For one client protocol: `(server entity type id, server index -> client\n")
    out.append("/// index)` for every type whose layout differs. `u8::MAX` drops the entry.\n")
    out.append("pub type IndexRemaps = &'static [(u32, &'static [u8])];\n\n")
    for client in CLIENTS:
        client_types = rel[client].registry("minecraft:entity_type")
        fallback = REGISTRY_FALLBACK["minecraft:entity_type"]
        rows = []
        dropped_total = 0
        for sid, name in enumerate(server_types):
            target = name if name in client_types else fallback.get(name)
            if target is None:
                die(f"entity type {name} has no equivalent in {client}")
            s_fields = layouts[SERVER][name]
            c_fields = layouts[client][target]
            mapping = match_fields(s_fields, c_fields)
            for i, m in enumerate(mapping):
                if m is not None and s_fields[i][2] != c_fields[m][2]:
                    report.append(f"entity {name} 776->{client}: {s_fields[i][1]} serializer {s_fields[i][2]} -> {c_fields[m][2]}")
            if mapping == list(range(len(c_fields))) and len(s_fields) == len(c_fields):
                continue
            dropped = [s_fields[i][1] for i, m in enumerate(mapping) if m is None]
            dropped_total += len(dropped)
            if dropped:
                report.append(f"entity {name} 776->{client}: drops {dropped}")
            if any(m is not None and m > 254 for m in mapping):
                die("entity data index above 254")
            body = arr(("255" if m is None else str(m) for m in mapping), 4)
            rows.append(f"    ({sid}, &{body}),\n")
        out.append(f"pub static INDEX_REMAPS_{client}: IndexRemaps = &[\n{''.join(rows)}];\n")
        report.append(f"entity data 776->{client}: {len(rows)} types remapped, {dropped_total} fields dropped")
    return "".join(out)


# ---------------------------------------------------------------------------
# Particles
# ---------------------------------------------------------------------------

PARTICLE_RE = re.compile(
    r'register\("([a-z_]+)",\s*(?:true|false)(?:,\s*(?:[\w$]+\s*->\s*)?(\w+)(?:::codec|\.CODEC))?'
)


def particle_options(release: Release) -> dict[str, str]:
    text = (release.decomp / "net/minecraft/core/particles/ParticleTypes.java").read_text()
    out = {}
    for m in PARTICLE_RE.finditer(text):
        out["minecraft:" + m.group(1)] = m.group(2) or "Simple"
    return out


def gen_particles(rel: dict[int, Release], report: list[str]) -> str:
    out = [HEADER, "//! The option data each particle type carries, per protocol.\n\n"]
    options = {p: particle_options(rel[p]) for p in rel}
    kinds = sorted({k for o in options.values() for k in o.values()})
    out.append("/// The `ParticleOptions` class a particle's data is encoded with.\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\npub enum Opt {\n")
    for k in kinds:
        out.append(f"    {k},\n")
    out.append("}\n\n")
    for p in sorted(rel):
        names = rel[p].registry("minecraft:particle_type")
        missing = [n for n in names if n not in options[p]]
        if missing:
            die(f"{rel[p].name}: particles without a registration: {missing}")
        out.append(f"pub static OPTIONS_{p}: &[Opt] = &{arr(('Opt::' + options[p][n] for n in names), 0)};\n")
        report.append(f"particles {p}: {len(names)}")
    return "".join(out)


# ---------------------------------------------------------------------------
# Data component layouts
#
# An item stack's component patch has no per-component length on the wire, so
# translating one component id means parsing every component value. The
# layouts come from hyperion's extractor (`nix/extract-protocol.py`), which
# reads each component's `StreamCodec` off the decompiled jar. Codecs it
# cannot model (a dispatch on a runtime type, a recursion, a nested item
# stack) are replaced here by named custom nodes the runtime implements by
# hand; each override below names the Java codec it stands for.
#
# 1.21.11 has no extractor run of its own (its mapped decompile is too lossy
# for the extractor); its layouts are 26.1's except for the components listed
# in COMPONENTS_CHANGED_IN_26_1, which the translator converts by hand.
# ---------------------------------------------------------------------------

CUSTOM_REFS = {
    "net.minecraft.world.item.ItemStackTemplate#STREAM_CODEC": "W::Custom(Custom::ItemTemplate)",
    "net.minecraft.world.item.ItemStack#STREAM_CODEC": "W::Custom(Custom::ItemStack)",
    "net.minecraft.world.item.ItemStack#OPTIONAL_STREAM_CODEC": "W::Custom(Custom::OptionalItemStack)",
    "net.minecraft.core.component.DataComponentPatch#STREAM_CODEC": "W::Custom(Custom::Patch)",
    "net.minecraft.core.component.TypedDataComponent#STREAM_CODEC": "W::Custom(Custom::TypedComponent)",
    "net.minecraft.core.particles.ParticleTypes#STREAM_CODEC": "W::Custom(Custom::Particle)",
    "net.minecraft.world.item.consume_effects.ConsumeEffect#STREAM_CODEC": "W::Custom(Custom::ConsumeEffect)",
    "net.minecraft.world.item.component.ItemAttributeModifiers$Display#STREAM_CODEC": "W::Custom(Custom::AttributeDisplay)",
    # `Type.STREAM_CODEC.dispatch(...)` whose every branch is
    # `ByteBufCodecs.fromCodecWithRegistries`: an Either of the predicate or
    # component type id, then the value as NBT.
    "net.minecraft.core.component.predicates.DataComponentPredicate#STREAM_CODEC":
        "W::List(&W::Struct(&[W::Either(&W::Reg(R::Static(Reg::DataComponentPredicateType)), &W::Reg(R::Static(Reg::DataComponentType))), W::Nbt]))",
    # Text components travel as NBT, but a hover inside one can name items,
    # components and entity types that differ between releases, so they get a
    # node of their own (`text.rs`) instead of opaque NBT.
    "net.minecraft.network.chat.ComponentSerialization#STREAM_CODEC": "W::Text",
    "net.minecraft.network.chat.ComponentSerialization#TRUSTED_STREAM_CODEC": "W::Text",
    "net.minecraft.network.chat.ComponentSerialization#OPTIONAL_STREAM_CODEC": "W::Opt(&W::Text)",
    "net.minecraft.network.chat.ComponentSerialization#TRUSTED_OPTIONAL_STREAM_CODEC": "W::Opt(&W::Text)",
}

# Codecs the extractor cannot model, written out from the release's source.
# 26.3 `SignText.STREAM_CODEC` uses `ByteBufCodecs.fixedSizeList(4)`, which
# writes exactly four elements with no count: four lines, four optional
# filtered lines, `DyeColor` (VarInt id) and the glow flag.
OVERRIDE_REFS = {
    "net.minecraft.world.level.block.entity.SignText#STREAM_CODEC":
        "W::Struct(&[W::Struct(&[W::Text, W::Text, W::Text, W::Text]), "
        "W::Opt(&W::Struct(&[W::Text, W::Text, W::Text, W::Text])), W::VarInt, W::Bool])",
}

# `MobEffectInstance.Details` carries an optional hidden effect of its own
# type (`subCodec.apply(ByteBufCodecs::optional)` inside a recursive codec).
RECURSIVE_REFS = {
    "net.minecraft.world.effect.MobEffectInstance$Details#STREAM_CODEC":
        ["W::VarInt", "W::VarInt", "W::Bool", "W::Bool", "W::Bool", "W::Opt(&W::Ref({self}))"],
}

# Components whose codec changed between 1.21.11 and 26.1 (ViaVersion's
# Protocol1_21_11To26_1 and the two releases' sources agree on this list):
# holder-or-key unions became plain holders, tag keys became holder sets, and
# nested stacks became item templates (id before count).
COMPONENTS_CHANGED_IN_26_1 = [
    "minecraft:jukebox_playable",
    "minecraft:instrument",
    "minecraft:provides_trim_material",
    "minecraft:chicken/variant",
    "minecraft:zombie_nautilus/variant",
    "minecraft:damage_type",
    "minecraft:provides_banner_patterns",
    "minecraft:damage_resistant",
    "minecraft:blocks_attacks",
    "minecraft:container",
    "minecraft:bundle_contents",
    "minecraft:charged_projectiles",
    "minecraft:use_remainder",
]

SIMPLE_KINDS = {
    "varint": "W::VarInt", "varlong": "W::VarLong", "i8": "W::I8", "i16": "W::I16", "i32": "W::I32",
    "i64": "W::I64", "f32": "W::F32", "f64": "W::F64", "bool": "W::Bool", "uuid": "W::Uuid",
    "string": "W::Str", "identifier": "W::Str", "nbt": "W::Nbt", "optional_nbt": "W::OptNbt",
    "byte_array": "W::ByteArray", "long_array": "W::LongArray", "varint_array": "W::VarIntArray",
    "block_pos": "W::I64", "chunk_pos": "W::I64", "unit": "W::Unit", "enum": "W::VarInt",
    "block_hit_result": "W::BlockHitResult",
}


class LayoutEmitter:
    def __init__(self, release: Release):
        self.release = release
        self.types = release.proto["types"]
        self.index: dict[str, int] = {}
        self.order: list[str] = []
        self.unresolved = False

    def registry(self, name: str) -> str:
        if name in WIRE_REGISTRIES:
            return f"R::Static(Reg::{pascal(name)})"
        if self.release.registry(name) is not None:
            # A static registry outside the remapped set: identical in every
            # supported release, so its ids pass through.
            return "R::Same"
        return f"R::Dynamic({rust_str(name)})"

    def ref(self, name: str) -> str:
        if name in CUSTOM_REFS:
            return CUSTOM_REFS[name]
        if name not in self.index:
            self.index[name] = len(self.order)
            self.order.append(name)
        return f"W::Ref({self.index[name]})"

    def wire(self, w: dict) -> str:
        k = w["kind"]
        if k == "named":
            return self.ref(w["ref"])
        if k in SIMPLE_KINDS:
            return SIMPLE_KINDS[k]
        if k == "fixed_bitset":
            return f"W::Fixed({(w['bits'] + 7) // 8})"
        if k == "registry_id":
            return f"W::Reg({self.registry(w['registry'])})"
        if k == "holder":
            return f"W::Holder({self.registry(w['registry'])}, &{self.wire(w['of'])})"
        if k == "holder_set":
            return f"W::HolderSet({self.registry(w['registry'])})"
        if k == "list":
            return f"W::List(&{self.wire(w['of'])})"
        if k == "option":
            return f"W::Opt(&{self.wire(w['of'])})"
        if k == "length_prefixed":
            return f"W::LenPrefixed(&{self.wire(w['of'])})"
        if k == "map":
            return f"W::Map(&{self.wire(w['key'])}, &{self.wire(w['value'])})"
        if k == "either":
            return f"W::Either(&{self.wire(w['left'])}, &{self.wire(w['right'])})"
        if k == "struct":
            return "W::Struct(&[" + ", ".join(self.wire(f["wire"]) for f in w["fields"]) + "])"
        if k == "unresolved":
            self.unresolved = True
            return "W::Unresolved"
        die(f"{self.release.name}: wire kind {k} has no runtime node")
        return ""

    def named_body(self, name: str) -> str:
        if name in OVERRIDE_REFS:
            return OVERRIDE_REFS[name]
        if name in RECURSIVE_REFS:
            me = self.index[name]
            return "W::Struct(&[" + ", ".join(f.replace("{self}", str(me)) for f in RECURSIVE_REFS[name]) + "])"
        target = self.types.get(name)
        if target is None:
            return "W::Unresolved"
        return self.wire(target)


def has_unresolved(w: dict, types: dict, seen: frozenset = frozenset()) -> bool:
    """Whether a layout still contains a part no override models."""
    k = w["kind"]
    if k == "unresolved":
        return True
    if k == "named":
        ref = w["ref"]
        if ref in CUSTOM_REFS or ref in RECURSIVE_REFS or ref in OVERRIDE_REFS or ref in seen:
            return False
        target = types.get(ref)
        return target is None or has_unresolved(target, types, seen | {ref})
    children = [w[key] for key in ("of", "key", "value", "left", "right") if key in w]
    children += [f["wire"] for f in w.get("fields", [])]
    return any(has_unresolved(c, types, seen) for c in children)


def canonical(w: dict, types: dict, seen: tuple = ()) -> str:
    """A layout's shape with registry names but no Java names, for comparison."""
    k = w["kind"]
    if k == "named":
        ref = w["ref"]
        if ref in CUSTOM_REFS or ref in RECURSIVE_REFS or ref in OVERRIDE_REFS:
            return ref.split(".")[-1]
        if ref in seen:
            return "rec"
        target = types.get(ref)
        return "?" if target is None else canonical(target, types, seen + (ref,))
    if k in ("string", "identifier"):
        return "str"
    if k == "enum":
        return "varint"
    parts = [k]
    for key in ("registry", "bits"):
        if key in w:
            parts.append(str(w[key]))
    for key in ("of", "key", "value", "left", "right"):
        if key in w:
            parts.append(canonical(w[key], types, seen))
    for f in w.get("fields", []):
        parts.append(canonical(f["wire"], types, seen))
    return "(" + " ".join(parts) + ")"


def gen_components(rel: dict[int, Release], report: list[str]) -> str:
    out = [HEADER, "//! Data component wire layouts, per protocol, for the item translator.\n\n",
           "use crate::layout::{Custom, R, W};\nuse super::registries::Reg;\n\n"]
    layouts: dict[int, dict[str, str]] = {}
    for p in (775, 776, 777):
        release = rel[p]
        em = LayoutEmitter(release)
        names = release.registry("minecraft:data_component_type")
        comps = {c["name"]: c for c in release.proto["dataComponents"]}
        rows = [em.wire(comps[name]["wire"]) for name in names]
        opaque = [n for n in names if has_unresolved(comps[n]["wire"], release.proto["types"])]
        # Named types, emitted after the components so every reference is known.
        bodies = []
        i = 0
        while i < len(em.order):
            em.unresolved = False
            bodies.append(em.named_body(em.order[i]))
            i += 1
        out.append(f"/// Named codecs reachable from protocol {p}'s components, by `W::Ref` index.\n")
        out.append(f"pub static TYPES_{p}: &[W] = &[\n")
        for name, body in zip(em.order, bodies):
            out.append(f"    // {name}\n    {body},\n")
        out.append("];\n\n")
        out.append(f"/// Protocol {p}'s components in id order: `(name, layout)`.\n")
        out.append(f"pub static COMPONENTS_{p}: &[(&str, W)] = &[\n")
        for name, body in zip(names, rows):
            out.append(f"    ({rust_str(name)}, {body}),\n")
        out.append("];\n\n")
        report.append(f"components {p}: {len(names)} ({len(opaque)} with an unmodelled part: {opaque})")
        layouts[p] = {c["name"]: canonical(c["wire"], release.proto["types"]) for c in release.proto["dataComponents"]}
    # Components whose shape (not just their registry ids) differs from 776's.
    for client in CLIENTS:
        base = layouts[775] if client == 774 else layouts[client]
        differs = sorted(n for n in layouts[SERVER] if n in base and base[n] != layouts[SERVER][n])
        if client == 774:
            differs = sorted(set(differs) | set(COMPONENTS_CHANGED_IN_26_1))
        out.append(f"/// Components whose layout in protocol {client} differs from 776's beyond registry ids.\n")
        out.append(f"pub static RESHAPED_{client}: &[&str] = &{arr((rust_str(n) for n in differs), 0)};\n")
        report.append(f"components reshaped 776->{client}: {differs}")
    return "".join(out)

HEADER = "// @generated by plugins/elyzune-via/tools/generate_tables.py from the vanilla\n// server jars of 1.21.11, 26.1.2, 26.2 and 26.3. Do not edit by hand.\n\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--data", required=True, type=Path, help="prepared data directory (tools/via_data.py)")
    ap.add_argument("--out", required=True, type=Path, help="src/generated directory to write")
    args = ap.parse_args()

    rel = {p: Release(args.data, p) for p in RELEASES}
    report: list[str] = []
    files = {
        "packets.rs": gen_packets(rel),
        "registries.rs": gen_registries(rel, report),
        "blocks.rs": gen_blocks(rel, report),
        "dynamic.rs": gen_dynamic(rel, report),
        "tags.rs": gen_tags(rel, report),
        "entity_data.rs": gen_entity_data(rel, report),
        "particles.rs": gen_particles(rel, report),
        "components.rs": gen_components(rel, report),
    }
    args.out.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        (args.out / name).write_text(text)
    for line in report:
        print(line, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
