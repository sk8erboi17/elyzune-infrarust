//! Login state.
//!
//! Only `login_finished` differs: 26.2 appended the session id (a UUID the
//! client echoes nowhere on an offline backend) after the game profile.
//! 1.21.11 and 26.1 clients do not read it, and a trailing UUID would make
//! the vanilla decoder fail with "packet was larger than expected".

use bytes::Bytes;

use crate::generated::Proto;
use crate::session::ViaTranslator;
use infrarust_api::types::RawPacket;

use crate::layout::{Dir, Xc};
use crate::wire::{Put, Reader, WireError, WireResult};

/// `login_finished`: `UUID id, String name(16), properties[], (776+) UUID session`.
pub fn login_finished(t: &ViaTranslator, body: &Bytes) -> WireResult<Bytes> {
    if t.client == Proto::V777 {
        return Ok(body.clone());
    }
    let mut r = Reader::new(body);
    r.uuid()?;
    r.str_bytes()?;
    let properties = r.len()?;
    for _ in 0..properties {
        r.str_bytes()?;
        r.str_bytes()?;
        if r.bool()? {
            r.str_bytes()?;
        }
    }
    let profile_end = r.pos();
    r.uuid()?;
    if !r.is_empty() {
        return Err(WireError::Malformed("bytes after the session id"));
    }
    Ok(body.slice(..profile_end))
}

/// `login_disconnect`: the reason as a JSON text component (String). A hover
/// naming release-dependent data is rewritten (`text::json`).
pub fn json_text(t: &ViaTranslator, body: &Bytes) -> WireResult<Bytes> {
    let mut r = Reader::new(body);
    let text = r.string()?;
    let mut x = Xc::new(t, Dir::S2c);
    let rewritten = crate::text::json(&mut x, text);
    declare(t, "minecraft:login_disconnect", &x.notes);
    let Some(rewritten) = rewritten else {
        return Ok(body.clone());
    };
    let mut out = Vec::with_capacity(rewritten.len() + 3);
    out.put_str(&rewritten);
    Ok(out.into())
}

/// `status_response` (id 0): a JSON object whose `description` is a text
/// component. `None` when it needs no change.
pub fn status_response(t: &ViaTranslator, packet: &RawPacket) -> Option<Bytes> {
    if packet.packet_id != 0 || !crate::text::mentions_versioned_data(&packet.data) {
        return None;
    }
    let mut r = Reader::new(&packet.data);
    let text = r.string().ok()?;
    let mut status: serde_json::Value = serde_json::from_str(text).ok()?;
    let description = status.get("description")?.to_string();
    let mut x = Xc::new(t, Dir::S2c);
    let rewritten = crate::text::json(&mut x, &description);
    declare(t, "minecraft:status_response", &x.notes);
    let rewritten = rewritten?;
    status["description"] = serde_json::from_str(&rewritten).ok()?;
    let mut out = Vec::new();
    out.put_str(&serde_json::to_string(&status).ok()?);
    Some(out.into())
}

/// Logs what a text rewrite left out. These packets come once per connection
/// (and the translator is only borrowed here), so the warning is written
/// directly rather than through the once-per-connection log.
fn declare(t: &ViaTranslator, packet: &'static str, notes: &[&'static str]) {
    for reason in notes {
        tracing::warn!(
            connection = t.connection_id,
            packet,
            reason,
            "elyzune-via: packet not translated"
        );
    }
}
