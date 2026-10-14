//! The handshake (`minecraft:intention`).
//!
//! Layout, identical from 774 to 777: VarInt protocol, String host (≤255),
//! u16 port, VarInt intent (1 status, 2 login, 3 transfer). The translator
//! rewrites only the protocol number: the backend must see 776 or it refuses
//! the login as outdated.

use bytes::Bytes;

use crate::generated::Proto;
use crate::session::WireState;
use crate::wire::{Put, Reader, WireError, WireResult};

/// Rewrites a client handshake for the 776 backend and returns the state both
/// directions enter next.
pub fn intention(body: &[u8]) -> WireResult<(Bytes, WireState)> {
    let mut r = Reader::new(body);
    let _client_protocol = r.varint()?;
    let host = r.str_bytes()?;
    let port = r.bytes(2)?;
    let intent = r.varint()?;
    let next = match intent {
        1 => WireState::Status,
        2 | 3 => WireState::Login,
        _ => return Err(WireError::Malformed("unknown handshake intent")),
    };
    let mut out = Vec::with_capacity(body.len() + 1);
    out.put_varint(Proto::V776.number());
    out.put_str_bytes(host);
    out.put_bytes(port);
    out.put_varint(intent);
    Ok((out.into(), next))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn handshake(protocol: i32, intent: i32) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_varint(protocol);
        b.put_str("play.elyzune.test");
        b.put_bytes(&25565u16.to_be_bytes());
        b.put_varint(intent);
        b
    }

    #[test]
    fn rewrites_only_the_protocol_number() {
        for (client, intent, state) in [
            (774, 2, WireState::Login),
            (777, 1, WireState::Status),
            (775, 3, WireState::Login),
        ] {
            let (body, next) = intention(&handshake(client, intent)).unwrap();
            assert_eq!(body.as_ref(), handshake(776, intent).as_slice());
            assert_eq!(next, state);
        }
    }

    #[test]
    fn refuses_an_unknown_intent() {
        assert!(intention(&handshake(774, 9)).is_err());
    }
}
