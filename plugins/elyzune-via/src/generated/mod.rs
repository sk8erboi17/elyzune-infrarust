//! Version tables generated from the vanilla server jars by
//! `tools/generate_tables.py`. Nothing in the child modules is written by
//! hand; this file only declares them and the protocol enum they index by.

#![allow(clippy::all, clippy::pedantic, unused_qualifications)]

// Laid out by the generator, which wraps the long tables itself; rustfmt
// would put every element on a line of its own.
#[rustfmt::skip]
pub mod blocks;
#[rustfmt::skip]
pub mod components;
#[rustfmt::skip]
pub mod dynamic;
#[rustfmt::skip]
pub mod entity_data;
#[rustfmt::skip]
pub mod packets;
#[rustfmt::skip]
pub mod particles;
#[rustfmt::skip]
pub mod registries;
#[rustfmt::skip]
pub mod tags;

/// The four protocols the generated tables describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proto {
    /// 1.21.11
    V774,
    /// 26.1, 26.1.1, 26.1.2
    V775,
    /// 26.2, the backend's protocol
    V776,
    /// 26.3
    V777,
}

impl Proto {
    /// The protocol number written in a handshake.
    pub const fn number(self) -> i32 {
        match self {
            Self::V774 => 774,
            Self::V775 => 775,
            Self::V776 => 776,
            Self::V777 => 777,
        }
    }

    pub const fn from_number(number: i32) -> Option<Self> {
        match number {
            774 => Some(Self::V774),
            775 => Some(Self::V775),
            776 => Some(Self::V776),
            777 => Some(Self::V777),
            _ => None,
        }
    }
}
