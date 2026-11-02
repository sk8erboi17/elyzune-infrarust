//! `servux:litematics`: the version in the server's Servux metadata.
//!
//! The game server answers Litematica's Servux handshake (packet type 1 on
//! the channel, a network NBT compound) with
//! `servux = "servux-fabric-26.2-<provider>"`. Litematica speaking Servux
//! protocol 2 (its 1.21.11, 26.2 and 26.3 releases) accepts the server only
//! when that string starts with `servux-fabric-<the client's own Minecraft
//! version>`, and otherwise turns its entity data sync off for the session.
//! The string names a release, so it is per client and this translator is
//! where the client's release is known: the backend's release is replaced
//! with the newest release of the client's protocol (a later patch on the
//! same protocol starts with it: `26.1.2` covers `26.1`). Protocol 1
//! clients (26.1) do not read the string; their metadata passes unchanged,
//! as does every other payload.

use bytes::Bytes;

use crate::{
    generated::Proto,
    nbt::Tag,
    state::core_pack_versions,
    wire::{Put, Reader, WireResult},
};

/// The channel (`ServuxLitematicaHandler.CHANNEL_ID`).
pub const CHANNEL: &str = "servux:litematics";
/// `PACKET_S2C_METADATA`.
const METADATA: i32 = 1;
const PREFIX: &str = "servux-fabric-";

/// The newest release `proto` covers.
fn release(proto: Proto) -> Option<&'static str> {
    core_pack_versions(proto).last().copied()
}

/// A clientbound `custom_payload` body (channel, then data) for `client`:
/// the Servux metadata with the client's release, anything else as is.
///
/// # Errors
/// A metadata body whose NBT does not read.
pub fn custom_payload(client: Proto, body: Bytes) -> WireResult<Bytes> {
    if matches!(client, Proto::V775 | Proto::V776) {
        return Ok(body);
    }
    let mut r = Reader::new(&body);
    let channel = r.string()?;
    if channel != CHANNEL {
        return Ok(body);
    }
    if r.varint()? != METADATA {
        return Ok(body);
    }
    let Some(mut tag) = Tag::read(&mut r)? else {
        return Ok(body);
    };
    let (Some(server), Some(client_release)) = (release(Proto::V776), release(client)) else {
        return Ok(body);
    };
    let Some(rest) = tag
        .get("servux")
        .and_then(Tag::as_str)
        .and_then(|servux| servux.strip_prefix(PREFIX))
        .and_then(|servux| servux.strip_prefix(server))
        .map(str::to_owned)
    else {
        return Ok(body);
    };
    tag.insert(
        "servux",
        Tag::string(&format!("{PREFIX}{client_release}{rest}")),
    );
    let mut out = Vec::with_capacity(body.len() + 8);
    out.put_str(channel);
    out.put_varint(METADATA);
    tag.write(&mut out);
    out.put_bytes(r.rest());
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn metadata(servux: &str) -> Bytes {
        let mut out = Vec::new();
        out.put_str(CHANNEL);
        out.put_varint(METADATA);
        Tag::Compound(vec![
            (b"name".to_vec(), Tag::string("litematic_data")),
            (b"id".to_vec(), Tag::string(CHANNEL)),
            (b"version".to_vec(), Tag::Int(2)),
            (b"servux".to_vec(), Tag::string(servux)),
        ])
        .write(&mut out);
        out.into()
    }

    fn servux_of(body: &[u8]) -> String {
        let mut r = Reader::new(body);
        assert_eq!(r.string().unwrap(), CHANNEL);
        assert_eq!(r.varint().unwrap(), METADATA);
        let tag = Tag::read(&mut r).unwrap().unwrap();
        assert_eq!(tag.get("version"), Some(&Tag::Int(2)));
        assert!(r.is_empty());
        tag.get("servux").and_then(Tag::as_str).unwrap().to_owned()
    }

    #[test]
    fn the_metadata_names_the_clients_release() {
        let body = metadata("servux-fabric-26.2-elyzune");
        assert_eq!(
            servux_of(&custom_payload(Proto::V777, body.clone()).unwrap()),
            "servux-fabric-26.3-elyzune"
        );
        assert_eq!(
            servux_of(&custom_payload(Proto::V774, body.clone()).unwrap()),
            "servux-fabric-1.21.11-elyzune"
        );
        // 26.2 is the server's own release; 26.1 speaks protocol 1, which
        // does not read the string.
        assert_eq!(custom_payload(Proto::V776, body.clone()).unwrap(), body);
        assert_eq!(custom_payload(Proto::V775, body.clone()).unwrap(), body);
    }

    #[test]
    fn other_payloads_pass_unchanged() {
        let mut brand = Vec::new();
        brand.put_str("minecraft:brand");
        brand.put_str("elyzune");
        let brand = Bytes::from(brand);
        assert_eq!(custom_payload(Proto::V777, brand.clone()).unwrap(), brand);

        // A data slice (type 11) on the channel.
        let mut slice = Vec::new();
        slice.put_str(CHANNEL);
        slice.put_varint(11);
        slice.put_bytes(&[1, 2, 3]);
        let slice = Bytes::from(slice);
        assert_eq!(custom_payload(Proto::V777, slice.clone()).unwrap(), slice);

        // Another release's string is not this server's: left alone.
        let foreign = metadata("servux-fabric-1.21.4-0.5.0");
        assert_eq!(
            custom_payload(Proto::V777, foreign.clone()).unwrap(),
            foreign
        );
    }
}
