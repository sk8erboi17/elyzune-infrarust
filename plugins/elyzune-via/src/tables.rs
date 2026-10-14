//! Runtime form of the generated tables.
//!
//! The generated remaps are stored as runs to keep the committed source small.
//! At first use each client protocol's tables are expanded once into flat
//! arrays (a few hundred KiB for all three clients together) so the per-packet
//! lookup is one bounds-checked index. Expansion happens on the first
//! connection of that protocol, on a proxy thread, never on a game tick.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::generated::registries::Reg;
use crate::generated::{Proto, blocks, entity_data, particles, registries, tags};

/// "No equivalent on the other side."
pub const ABSENT: u32 = u32::MAX;

/// A dense id remap between two numberings.
#[derive(Debug)]
pub struct IdMap {
    forward: Vec<u32>,
}

impl IdMap {
    fn from_runs(runs: &[(u32, u32, u32)], len: u32) -> Self {
        let mut forward = vec![ABSENT; len as usize];
        for &(from, to, n) in runs {
            for i in 0..n {
                if let Some(slot) = forward.get_mut((from + i) as usize) {
                    *slot = to + i;
                }
            }
        }
        Self { forward }
    }

    /// The mapped id, or `None` when the entry has no equivalent.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<u32> {
        match self.forward.get(id as usize) {
            Some(&ABSENT) | None => None,
            Some(&to) => Some(to),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

/// Both directions of one static registry for one client protocol, or the
/// identity when the registry did not change.
#[derive(Debug)]
pub enum RegMap {
    Identity,
    Remap { s2c: IdMap, c2s: IdMap },
}

impl RegMap {
    /// Server id to client id.
    #[must_use]
    pub fn s2c(&self, id: u32) -> Option<u32> {
        match self {
            Self::Identity => Some(id),
            Self::Remap { s2c, .. } => s2c.get(id),
        }
    }

    /// Client id to server id.
    #[must_use]
    pub fn c2s(&self, id: u32) -> Option<u32> {
        match self {
            Self::Identity => Some(id),
            Self::Remap { c2s, .. } => c2s.get(id),
        }
    }
}

/// Everything static the translator needs for one client protocol.
pub struct VersionTables {
    pub client: Proto,
    regs: HashMap<Reg, RegMap>,
    /// 776 block state -> client block state; total (fallbacks included).
    pub block_states: RegMap,
    /// 776 block state ids that are beds (for the 774/775 bed block entity).
    pub bed_states: Vec<(u32, u32)>,
    /// Bits a direct block palette takes on the client.
    pub client_state_bits: u8,
    /// Per server entity type: server metadata index -> client index.
    pub entity_index: HashMap<u32, &'static [u8]>,
    /// Client serializer id for each server serializer id.
    pub serializer_s2c: Vec<Option<u32>>,
    pub client_particles: &'static [particles::Opt],
    pub extra_tags: &'static [(&'static str, &'static str, tags::TagEntries)],
}

impl VersionTables {
    /// The remap of `reg`. Registries without a generated table are identical.
    #[must_use]
    pub fn reg(&self, reg: Reg) -> &RegMap {
        self.regs.get(&reg).unwrap_or(&RegMap::Identity)
    }

    #[must_use]
    pub fn is_bed(&self, server_state: u32) -> bool {
        self.bed_states
            .iter()
            .any(|&(start, len)| server_state >= start && server_state < start + len)
    }

    fn build(client: Proto) -> Self {
        let number = client.number() as u32;
        let mut regs = HashMap::new();
        for &reg in Reg::ALL {
            if let Some((s2c, c2s, server_len, client_len)) = registries::tables(reg, number) {
                regs.insert(
                    reg,
                    RegMap::Remap {
                        s2c: IdMap::from_runs(s2c, server_len),
                        c2s: IdMap::from_runs(c2s, client_len),
                    },
                );
            }
        }
        let (state_runs, state_bits) = match client {
            Proto::V774 => (blocks::STATES_774_S2C, blocks::STATE_BITS_774),
            Proto::V775 => (blocks::STATES_775_S2C, blocks::STATE_BITS_775),
            Proto::V777 => (blocks::STATES_777_S2C, blocks::STATE_BITS_777),
            Proto::V776 => (&[][..], blocks::STATE_BITS_776),
        };
        let block_states = if client == Proto::V776 {
            RegMap::Identity
        } else {
            RegMap::Remap {
                s2c: IdMap::from_runs(state_runs, blocks::STATE_COUNT_776),
                c2s: IdMap::from_runs(&[], 0),
            }
        };
        let bed_states = blocks::BED_STATES_776
            .iter()
            .map(|&(s, _, n)| (s, n))
            .collect();
        let remaps = match client {
            Proto::V774 => entity_data::INDEX_REMAPS_774,
            Proto::V775 => entity_data::INDEX_REMAPS_775,
            Proto::V777 => entity_data::INDEX_REMAPS_777,
            Proto::V776 => &[][..],
        };
        let entity_index = remaps.iter().map(|&(id, map)| (id, map)).collect();
        let client_sers = serializers(client);
        let serializer_s2c = serializers(Proto::V776)
            .iter()
            .map(|s| {
                client_sers
                    .iter()
                    .position(|c| c == s)
                    .and_then(|i| u32::try_from(i).ok())
            })
            .collect();
        let client_particles = particle_options(client);
        let extra_tags = match client {
            Proto::V774 => tags::EXTRA_TAGS_774,
            Proto::V775 => tags::EXTRA_TAGS_775,
            Proto::V777 => tags::EXTRA_TAGS_777,
            Proto::V776 => &[][..],
        };
        Self {
            client,
            regs,
            block_states,
            bed_states,
            client_state_bits: state_bits,
            entity_index,
            serializer_s2c,
            client_particles,
            extra_tags,
        }
    }
}

/// The remapped static registry called `name`, if it is one.
#[must_use]
pub fn reg_by_name(name: &str) -> Option<Reg> {
    Reg::ALL.iter().copied().find(|reg| reg.name() == name)
}

/// Registries whose entries the translator looks up by name: the ones NBT
/// names entries of (items, blocks, entities, components, block entities,
/// attributes, sounds, effects, map decorations) plus the few packets handle
/// by name. Kept in step with `NAMED_REGISTRIES` in the generator; a registry
/// missing here simply has no index and every lookup in it misses.
const NAMED: &[Reg] = &[
    Reg::Attribute,
    Reg::Block,
    Reg::BlockEntityType,
    Reg::CommandArgumentType,
    Reg::ConsumeEffectType,
    Reg::DataComponentType,
    Reg::EntityType,
    Reg::Item,
    Reg::MapDecorationType,
    Reg::Menu,
    Reg::MobEffect,
    Reg::ParticleType,
    Reg::PositionSourceType,
    Reg::RecipeDisplay,
    Reg::SlotDisplay,
    Reg::SoundEvent,
    Reg::StatType,
];

const PROTOS: [Proto; 4] = [Proto::V774, Proto::V775, Proto::V776, Proto::V777];

type NameIndex = HashMap<(Reg, u32), HashMap<&'static str, u32>>;

/// Name -> id of every [`NAMED`] registry in every protocol, built on the
/// first lookup (a few thousand entries) and shared for the process.
fn name_index() -> &'static NameIndex {
    static INDEX: OnceLock<NameIndex> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index = HashMap::new();
        for &reg in NAMED {
            for proto in PROTOS {
                let number = proto.number().unsigned_abs();
                let mut names = HashMap::new();
                for id in 0u32.. {
                    let Some(name) = registries::name_of(reg, number, id) else {
                        break;
                    };
                    names.insert(name, id);
                }
                index.insert((reg, number), names);
            }
        }
        index
    })
}

/// The id of entry `name` in `proto`'s `reg`. `name` may omit the
/// `minecraft:` namespace, as identifiers in NBT may.
#[must_use]
pub fn id_by_name(reg: Reg, proto: Proto, name: &str) -> Option<u32> {
    let names = name_index().get(&(reg, proto.number().unsigned_abs()))?;
    if name.contains(':') {
        names.get(name).copied()
    } else {
        names.get(format!("minecraft:{name}").as_str()).copied()
    }
}

/// The serializer list of a protocol, by id.
#[must_use]
pub fn serializers(proto: Proto) -> &'static [entity_data::Ser] {
    match proto {
        Proto::V774 => entity_data::SERIALIZERS_774,
        Proto::V775 => entity_data::SERIALIZERS_775,
        Proto::V776 => entity_data::SERIALIZERS_776,
        Proto::V777 => entity_data::SERIALIZERS_777,
    }
}

/// The particle option kinds of a protocol, by particle id.
#[must_use]
pub fn particle_options(proto: Proto) -> &'static [particles::Opt] {
    match proto {
        Proto::V774 => particles::OPTIONS_774,
        Proto::V775 => particles::OPTIONS_775,
        Proto::V776 => particles::OPTIONS_776,
        Proto::V777 => particles::OPTIONS_777,
    }
}

/// The tables for `client`, built on first use and shared by every connection
/// of that protocol for the life of the process.
#[must_use]
pub fn for_client(client: Proto) -> &'static VersionTables {
    static V774: OnceLock<VersionTables> = OnceLock::new();
    static V775: OnceLock<VersionTables> = OnceLock::new();
    static V777: OnceLock<VersionTables> = OnceLock::new();
    static V776: OnceLock<VersionTables> = OnceLock::new();
    let cell = match client {
        Proto::V774 => &V774,
        Proto::V775 => &V775,
        Proto::V776 => &V776,
        Proto::V777 => &V777,
    };
    cell.get_or_init(|| VersionTables::build(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::registries::{id_of, name_of};

    #[test]
    fn runs_expand_to_the_generated_mapping() {
        let t = for_client(Proto::V777);
        // 26.3 inserted three entity types; 776's player (156) is 777's 159.
        let player_776 = id_of(Reg::EntityType, 776, "minecraft:player");
        let player_777 = id_of(Reg::EntityType, 777, "minecraft:player");
        assert_eq!(player_776, Some(156));
        assert_eq!(player_777, Some(159));
        assert_eq!(t.reg(Reg::EntityType).s2c(156), Some(159));
        assert_eq!(t.reg(Reg::EntityType).c2s(159), Some(156));
        // The new 26.3 types have no 776 equivalent.
        let cushion = id_of(Reg::EntityType, 777, "minecraft:cushion")
            .map(|id| t.reg(Reg::EntityType).c2s(id));
        assert_eq!(cushion, Some(None));
    }

    #[test]
    fn missing_entries_fall_back_by_name() {
        let t = for_client(Proto::V774);
        let sulfur_cube = id_of(Reg::EntityType, 776, "minecraft:sulfur_cube")
            .and_then(|id| t.reg(Reg::EntityType).s2c(id));
        assert_eq!(
            sulfur_cube.and_then(|id| name_of(Reg::EntityType, 774, id)),
            Some("minecraft:slime")
        );
    }

    #[test]
    fn block_states_are_total_and_beds_are_known() {
        for client in [Proto::V774, Proto::V775, Proto::V777] {
            let t = for_client(client);
            for state in 0..blocks::STATE_COUNT_776 {
                assert!(
                    t.block_states.s2c(state).is_some(),
                    "{client:?} state {state}"
                );
            }
        }
        let t = for_client(Proto::V775);
        // 26.2 white_bed starts at 1931 (blocks.json); 16 states per colour.
        assert!(t.is_bed(1931));
        assert!(!t.is_bed(1930));
        assert_eq!(t.client_state_bits, 15);
        assert_eq!(for_client(Proto::V777).client_state_bits, 16);
    }
}
