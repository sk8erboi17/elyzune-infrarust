//! Protocol translation between a client and a backend that speak different
//! protocol versions.
//!
//! # Where a translator sits
//!
//! A codec filter rewrites packets *inside* one protocol version, and only in
//! Play. A protocol translator sits one level lower, on the backend socket of
//! an intercepted session (`client_only`, `offline`, `full`):
//!
//! ```text
//!  client (version C) ── ClientBridge ── proxy core (version C) ── BackendBridge ── backend (version B)
//!                                                                   └ translator: C ⇄ B
//! ```
//!
//! Everything the core reads from the backend has already been translated to
//! the client's version C, and everything the core writes to the backend is
//! translated to B on its way out. The rest of the proxy (chat and command
//! interception, codec filters, limbo, server switches) therefore keeps
//! working at a single version, exactly as if the backend spoke C.
//!
//! The translator sees every frame of the backend connection in wire order,
//! in both directions and in every state, starting with the handshake (whose
//! protocol number it rewrites to B). It tracks the connection state itself
//! from the transition packets it sees: the proxy's own notion of the backend
//! state is not handed over, because during a reconfiguration the core reads
//! with one state and writes with another, while the wire has exactly one
//! state per direction.
//!
//! # Threading and cost
//!
//! A translator is created per backend connection and lives on the session's
//! task. Its methods are synchronous and run inline on every frame, so they
//! must not block; they run on proxy threads, never on a game server tick.

use crate::types::{ProtocolVersion, RawPacket, ServerId};

/// Global factory registered once by a plugin. Decides, per backend
/// connection, whether that connection is translated.
///
/// `Send + Sync` because the proxy shares it across all sessions.
pub trait ProtocolTranslatorFactory: Send + Sync {
    /// Stable identifier, used to report ownership conflicts.
    fn id(&self) -> &str;

    /// Called once each time an intercepted session connects to a backend,
    /// including every server switch. Returns `None` when the client and the
    /// backend already speak the same version (or the pair is unsupported):
    /// the connection is then relayed untouched, at zero cost.
    fn create(&self, init: &TranslatorInit) -> Option<Box<dyn ProtocolTranslator>>;
}

/// What the proxy knows about a backend connection when it asks a factory for
/// a translator.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TranslatorInit {
    /// The version the client negotiated in its handshake.
    pub client_version: ProtocolVersion,
    /// The configured server the session is connecting to.
    pub server: ServerId,
    /// The session identifier, for log correlation.
    pub connection_id: u64,
}

impl TranslatorInit {
    #[must_use]
    pub const fn new(
        client_version: ProtocolVersion,
        server: ServerId,
        connection_id: u64,
    ) -> Self {
        Self {
            client_version,
            server,
            connection_id,
        }
    }
}

/// One backend connection's translation state.
///
/// Contract shared by both directions: the translator consumes the packet it
/// is given and pushes zero or more packets into `out`. Packets pushed with
/// [`TranslatorOutput::to_client`] are in the client's version and reach the
/// proxy core as if the backend had sent them; packets pushed with
/// [`TranslatorOutput::to_server`] are in the backend's version and are
/// written to the backend socket. Order within each list is preserved, and a
/// direction's outputs are delivered before the next packet of that direction
/// is translated.
///
/// Dropping a packet (pushing nothing) is a legitimate translation, for a
/// packet the other side has no equivalent for. A translator that cannot
/// continue safely calls [`TranslatorOutput::close`]; the proxy then ends the
/// session with that reason instead of desynchronising the stream.
pub trait ProtocolTranslator: Send {
    /// The version the backend speaks. The proxy writes it into the handshake
    /// it sends to the backend (the translator also rewrites it if it sees the
    /// client's own handshake bytes).
    fn backend_version(&self) -> ProtocolVersion;

    /// Translates a packet read from the backend (version B).
    fn clientbound(&mut self, packet: RawPacket, out: &mut TranslatorOutput);

    /// Translates a packet the proxy core wants to send to the backend
    /// (version C).
    fn serverbound(&mut self, packet: RawPacket, out: &mut TranslatorOutput);
}

/// Packets produced by one translation call.
///
/// Reused across calls by the proxy: it drains the lists after every call, so
/// a translator never sees what an earlier call pushed.
#[derive(Debug, Default)]
pub struct TranslatorOutput {
    to_client: Vec<RawPacket>,
    to_server: Vec<RawPacket>,
    close: Option<String>,
}

impl TranslatorOutput {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a packet, already in the client's version, for the proxy core.
    pub fn to_client(&mut self, packet: RawPacket) {
        self.to_client.push(packet);
    }

    /// Queues a packet, already in the backend's version, for the backend.
    pub fn to_server(&mut self, packet: RawPacket) {
        self.to_server.push(packet);
    }

    /// Asks the proxy to end the session. The first reason wins.
    pub fn close(&mut self, reason: impl Into<String>) {
        if self.close.is_none() {
            self.close = Some(reason.into());
        }
    }

    /// Drains the packets queued for the proxy core.
    pub fn take_to_client(&mut self) -> std::vec::Drain<'_, RawPacket> {
        self.to_client.drain(..)
    }

    /// Drains the packets queued for the backend.
    pub fn take_to_server(&mut self) -> std::vec::Drain<'_, RawPacket> {
        self.to_server.drain(..)
    }

    /// Takes the close reason, if the translator asked for one.
    pub fn take_close(&mut self) -> Option<String> {
        self.close.take()
    }

    /// Read-only view of the packets queued for the proxy core (tests).
    #[must_use]
    pub fn client_packets(&self) -> &[RawPacket] {
        &self.to_client
    }

    /// Read-only view of the packets queued for the backend (tests).
    #[must_use]
    pub fn server_packets(&self) -> &[RawPacket] {
        &self.to_server
    }

    /// Whether a close was requested and not yet taken (tests).
    #[must_use]
    pub const fn close_requested(&self) -> Option<&String> {
        self.close.as_ref()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn output_drains_in_order_and_keeps_the_first_close() {
        let mut out = TranslatorOutput::new();
        out.to_client(RawPacket::new(1, bytes::Bytes::from_static(b"a")));
        out.to_client(RawPacket::new(2, bytes::Bytes::from_static(b"b")));
        out.to_server(RawPacket::new(3, bytes::Bytes::new()));
        out.close("first");
        out.close("second");

        let ids: Vec<i32> = out.take_to_client().map(|p| p.packet_id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(out.take_to_server().count(), 1);
        assert_eq!(out.take_close().as_deref(), Some("first"));
        assert!(out.take_close().is_none());
        assert!(out.client_packets().is_empty());
    }
}
