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
//! from the server's value. A claim that matches a record (same client item
//! and component ids) is rewritten into the server's stack. A record with a
//! component this module cannot hash is still matched, and the claim keeps
//! the client's numbers for the rest: the server resends that slot, as it
//! would have without the record.
//!
//! The records are bounded ([`MAX_SENT`], oldest dropped) and cleared on a
//! reconfiguration; the most recent matching one wins.
//!
//! # HashOps
//!
//! The primitives below are `net.minecraft.util.HashOps` over Guava's
//! CRC32C, as Hyperion's `hyperion-minecraft-proto::item::hash` implements
//! them for the server side (and pins with vectors from the 26.2 jar):
//!
//! ```text
//! int     8, value (LE)          string  12, UTF-16 length (LE i32), units (LE)
//! list    4, element hashes, 5   map     2, (key, value) hashes sorted unsigned, 3
//! ```

use std::collections::VecDeque;

/// Upper bound on recorded stacks per connection. A player rarely holds more
/// than a handful of stacks the translation changed; 64 covers a full
/// inventory of them with room for a container.
pub const MAX_SENT: usize = 64;

/// How the server's hash of one component is obtained for a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Same codec form on both sides: the client's hash of its component
    /// with this (client) type id.
    FromClient(u32),
    /// The server's hash, computed from its value when the stack was sent.
    Computed(i32),
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

/// The recent [`Sent`] stacks of one connection.
#[derive(Debug, Default)]
pub struct SentStacks {
    recent: VecDeque<Sent>,
}

impl SentStacks {
    /// Records `sent`, replacing an identical record and evicting the oldest
    /// past [`MAX_SENT`].
    pub fn record(&mut self, sent: Sent) {
        if let Some(index) = self.recent.iter().position(|s| *s == sent) {
            self.recent.remove(index);
        }
        if self.recent.len() >= MAX_SENT {
            self.recent.pop_front();
        }
        self.recent.push_back(sent);
    }

    /// The most recent record whose client side has `item` and these sorted
    /// component ids.
    #[must_use]
    pub fn find(&self, item: u32, added: &[u32], removed: &[u32]) -> Option<&Sent> {
        self.recent.iter().rev().find(|s| {
            s.client_item == item && s.client_added == added && s.client_removed == removed
        })
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
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
}
