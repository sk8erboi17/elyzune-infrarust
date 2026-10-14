//! What a translator remembers about its connection between packets.
//!
//! Everything here is rebuilt from the stream itself: the synchronised
//! registries from `registry_data`, entity types from `add_entity`, the game
//! mode from `login`/`respawn`/`game_event`. A reconfiguration (the backend's
//! `start_configuration`) clears what the backend is about to resend.
//!
//! Bounds: the entity map holds at most [`MAX_TRACKED_ENTITIES`] entries. The
//! client itself only tracks entities within view distance, so a well-behaved
//! backend never reaches the cap; past it, new entities are not tracked and
//! their metadata is forwarded with the server's indices (logged once).

use std::collections::HashMap;

use crate::generated::Proto;
use crate::generated::dynamic;

/// Upper bound on tracked entity ids per connection.
pub const MAX_TRACKED_ENTITIES: usize = 1 << 17;

/// A synchronised (data-driven) registry as the backend sent it, and how its
/// ids translate after the entries the client cannot resolve were removed.
#[derive(Debug, Default, Clone)]
pub struct DynRegistry {
    /// Entry names in the backend's order: the backend's ids.
    pub names: Vec<String>,
    /// Backend id -> client id; `None` for entries removed for the client.
    pub s2c: Vec<Option<u32>>,
    /// Whether the client has this registry at all.
    pub client_has_registry: bool,
}

impl DynRegistry {
    #[must_use]
    pub fn s2c(&self, id: u32) -> Option<u32> {
        self.s2c.get(id as usize).copied().flatten()
    }

    /// The client id of the entry called `name`.
    #[must_use]
    pub fn client_id_of(&self, name: &str) -> Option<u32> {
        let index = self.names.iter().position(|n| n == name)?;
        self.s2c.get(index).copied().flatten()
    }

    /// The backend id of the client's entry `client_id` (reverse lookup; the
    /// lists are tens of entries long).
    #[must_use]
    pub fn c2s(&self, client_id: u32) -> Option<u32> {
        self.s2c
            .iter()
            .position(|m| *m == Some(client_id))
            .and_then(|i| u32::try_from(i).ok())
    }
}

/// Clock state a 1.21.11 client needs: 26.1 replaced `set_time`'s day time
/// with per-clock state, and an old client still expects the overworld's day
/// time in every `set_time`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DayTime {
    pub game_time: i64,
    pub day_time: i64,
    pub advancing: bool,
}

/// Per-connection translation state.
#[derive(Debug)]
pub struct Conn {
    /// Synchronised registries, keyed by registry name.
    pub dynamic: HashMap<String, DynRegistry>,
    /// Entity id -> backend entity type id, for entity data remaps.
    pub entities: HashMap<i32, u32>,
    /// Whether the entity cap was reached (logged once).
    pub entity_cap_hit: bool,
    /// The player's current game mode (0 survival .. 3 spectator).
    pub game_mode: u8,
    pub day_time: DayTime,
    /// The player's own entity id, from `login`.
    pub player_entity: i32,
    /// Lowest block y of the current dimension, from `login`/`respawn`
    /// (needed to place the bed block entities 774/775 clients expect).
    pub min_y: i16,
    /// Slot stacks sent changed in a way their hash notices (`hashed.rs`).
    pub sent_stacks: crate::hashed::SentStacks,
}

impl Default for Conn {
    fn default() -> Self {
        Self {
            dynamic: HashMap::new(),
            entities: HashMap::new(),
            entity_cap_hit: false,
            game_mode: 0,
            day_time: DayTime::default(),
            player_entity: 0,
            min_y: -64,
            sent_stacks: crate::hashed::SentStacks::default(),
        }
    }
}

impl Conn {
    /// The backend is about to resend its configuration: registries and
    /// everything indexed by them become stale.
    pub fn reconfigure(&mut self) {
        self.dynamic.clear();
        self.entities.clear();
        self.sent_stacks.clear();
    }

    #[must_use]
    pub fn dynamic(&self, registry: &str) -> Option<&DynRegistry> {
        self.dynamic.get(registry)
    }

    /// Maps a backend id of the synchronised registry `registry` to the
    /// client's id. Registries the backend never sent are identity: a
    /// backend that relies on the client's own built-in data has the same
    /// numbering on both sides.
    #[must_use]
    pub fn dyn_s2c(&self, registry: &str, id: u32) -> Option<u32> {
        match self.dynamic.get(registry) {
            Some(r) => r.s2c(id),
            None => Some(id),
        }
    }

    #[must_use]
    pub fn dyn_c2s(&self, registry: &str, id: u32) -> Option<u32> {
        match self.dynamic.get(registry) {
            Some(r) => r.c2s(id),
            None => Some(id),
        }
    }

    pub fn track_entity(&mut self, id: i32, server_type: u32) -> bool {
        if self.entities.len() >= MAX_TRACKED_ENTITIES && !self.entities.contains_key(&id) {
            let first = !self.entity_cap_hit;
            self.entity_cap_hit = true;
            return first;
        }
        self.entities.insert(id, server_type);
        false
    }
}

/// The synchronised registries of `proto` with their vanilla entries.
#[must_use]
pub fn synced(proto: Proto) -> dynamic::Synced {
    match proto {
        Proto::V774 => dynamic::SYNCED_774,
        Proto::V775 => dynamic::SYNCED_775,
        Proto::V776 => dynamic::SYNCED_776,
        Proto::V777 => dynamic::SYNCED_777,
    }
}

/// The ids the `minecraft:core` known pack carries in `proto`.
#[must_use]
pub fn core_pack_versions(proto: Proto) -> &'static [&'static str] {
    match proto {
        Proto::V774 => dynamic::CORE_PACK_774,
        Proto::V775 => dynamic::CORE_PACK_775,
        Proto::V776 => dynamic::CORE_PACK_776,
        Proto::V777 => dynamic::CORE_PACK_777,
    }
}

/// The vanilla entries `proto` ships for a synchronised registry, if it
/// synchronises that registry at all.
#[must_use]
pub fn vanilla_entries(proto: Proto, registry: &str) -> Option<&'static [&'static str]> {
    synced(proto)
        .iter()
        .find(|(name, _)| *name == registry)
        .map(|(_, entries)| *entries)
}
