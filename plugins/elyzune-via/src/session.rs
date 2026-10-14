//! One translated backend connection.
//!
//! # Connection state
//!
//! The translator sees every frame of the backend connection, in wire order,
//! in both directions. Each direction has exactly one protocol state at any
//! point, and it changes only on that direction's own transition packets:
//!
//! ```text
//! serverbound: Handshake --intention--> Login/Status
//!              Login --login_acknowledged--> Config
//!              Config --finish_configuration--> Play
//!              Play --configuration_acknowledged--> Config
//! clientbound: Login --login_finished--> Config
//!              Config --finish_configuration--> Play
//!              Play --start_configuration--> Config
//! ```
//!
//! A clientbound transition takes effect right after the packet that causes
//! it, which is also how the vanilla client switches its decoder, so the
//! translator reads every frame with the state the real client will use.
//!
//! # Per-packet decisions
//!
//! Each packet is identified by its name in the sending side's protocol (from
//! the generated id tables), then either re-identified only (same body on both
//! sides) or rewritten by a handler in `translate`. A packet with no
//! equivalent on the other side is dropped; a body a handler cannot parse is
//! dropped too, because forwarding it half-understood would desynchronise the
//! peer. Both are logged once per packet name per connection, and nothing is
//! dropped silently.

use std::collections::HashSet;
use std::time::Instant;

use bytes::Bytes;
use infrarust_api::filter::{ProtocolTranslator, TranslatorOutput};
use infrarust_api::types::{ProtocolVersion, RawPacket};

use crate::generated::Proto;
use crate::generated::packets::{
    ConfigCb, ConfigSb, HandshakeSb, LoginCb, LoginSb, PlayCb, PlaySb, StatusCb, StatusSb,
};
use crate::state::Conn;
use crate::stats::Timing;
use crate::tables::{self, VersionTables};
use crate::wire::WireError;

/// The protocol state of one direction of the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireState {
    Handshake,
    Status,
    Login,
    Config,
    Play,
}

impl WireState {
    /// Lower-case name, for logs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::Status => "status",
            Self::Login => "login",
            Self::Config => "config",
            Self::Play => "play",
        }
    }
}

/// Packet names already reported on this connection, so each problem is
/// logged once rather than once per packet. Keys are static strings, so the
/// set is bounded by the number of distinct packet names and reasons.
#[derive(Debug, Default)]
pub struct LogOnce {
    seen: HashSet<(&'static str, &'static str)>,
}

impl LogOnce {
    /// Reports `reason` for `packet` the first time it happens on this
    /// connection.
    pub fn once(&mut self, connection: u64, packet: &'static str, reason: &'static str) {
        if self.seen.insert((packet, reason)) {
            tracing::warn!(
                connection,
                packet,
                reason,
                "elyzune-via: packet not translated"
            );
        }
    }

    /// Like [`once`](Self::once) for a body that failed to parse.
    pub fn malformed(&mut self, connection: u64, packet: &'static str, error: WireError) {
        if self.seen.insert((packet, "malformed")) {
            tracing::warn!(
                connection,
                packet,
                %error,
                "elyzune-via: could not translate the packet body; dropped"
            );
        }
    }
}

/// The translator of one backend connection.
pub struct ViaTranslator {
    pub(crate) client: Proto,
    pub(crate) tables: &'static VersionTables,
    pub(crate) conn: Conn,
    pub(crate) log: LogOnce,
    pub(crate) connection_id: u64,
    cb: WireState,
    sb: WireState,
    /// Per-packet timing, only when asked for (`stats.rs`).
    timing: Option<Box<Timing>>,
}

impl ViaTranslator {
    /// A translator for a client speaking `client` (774, 775 or 777) on a
    /// 776 backend; it times its packets when `ELYZUNE_VIA_STATS` is set
    /// (`stats.rs`).
    #[must_use]
    pub fn new(client: Proto, connection_id: u64) -> Self {
        let mut translator = Self::untimed(client, connection_id);
        if crate::stats::enabled() {
            translator.timing = Some(Box::default());
        }
        translator
    }

    /// [`new`](Self::new), timing every packet whatever the environment
    /// says.
    #[must_use]
    pub fn timed(client: Proto, connection_id: u64) -> Self {
        let mut translator = Self::untimed(client, connection_id);
        translator.timing = Some(Box::default());
        translator
    }

    fn untimed(client: Proto, connection_id: u64) -> Self {
        Self {
            client,
            tables: tables::for_client(client),
            conn: Conn::default(),
            log: LogOnce::default(),
            connection_id,
            cb: WireState::Handshake,
            sb: WireState::Handshake,
            timing: None,
        }
    }

    /// The timing so far, when this translator times its packets.
    #[must_use]
    pub fn timing(&self) -> Option<&Timing> {
        self.timing.as_deref()
    }

    /// Current wire states `(clientbound, serverbound)`, for tests.
    #[must_use]
    pub const fn states(&self) -> (WireState, WireState) {
        (self.cb, self.sb)
    }

    /// Whether `reason` was reported for `packet` on this connection (the
    /// once-per-connection log), for tests.
    #[must_use]
    pub fn reported(&self, packet: &str, reason: &str) -> bool {
        self.log
            .seen
            .iter()
            .any(|(p, r)| *p == packet && *r == reason)
    }

    /// What the translator remembers about the connection (registries,
    /// entities), read-only, for tests.
    #[must_use]
    pub const fn connection_state(&self) -> &Conn {
        &self.conn
    }

    pub(crate) fn drop_unsupported(&mut self, packet: &'static str, reason: &'static str) {
        self.log.once(self.connection_id, packet, reason);
    }

    pub(crate) fn drop_malformed(&mut self, packet: &'static str, error: WireError) {
        self.log.malformed(self.connection_id, packet, error);
    }

    // ---- emitting -------------------------------------------------------

    pub(crate) fn emit_play_cb(&mut self, kind: PlayCb, body: Bytes, out: &mut TranslatorOutput) {
        match kind.id(self.client) {
            Some(id) => out.to_client(RawPacket::new(id, body)),
            None => self.drop_unsupported(kind.name(), "the client protocol has no such packet"),
        }
    }

    pub(crate) fn emit_play_sb(&mut self, kind: PlaySb, body: Bytes, out: &mut TranslatorOutput) {
        match kind.id(Proto::V776) {
            Some(id) => out.to_server(RawPacket::new(id, body)),
            None => self.drop_unsupported(kind.name(), "the backend protocol has no such packet"),
        }
    }

    pub(crate) fn emit_config_cb(
        &mut self,
        kind: ConfigCb,
        body: Bytes,
        out: &mut TranslatorOutput,
    ) {
        match kind.id(self.client) {
            Some(id) => out.to_client(RawPacket::new(id, body)),
            None => self.drop_unsupported(kind.name(), "the client protocol has no such packet"),
        }
    }

    pub(crate) fn emit_config_sb(
        &mut self,
        kind: ConfigSb,
        body: Bytes,
        out: &mut TranslatorOutput,
    ) {
        match kind.id(Proto::V776) {
            Some(id) => out.to_server(RawPacket::new(id, body)),
            None => self.drop_unsupported(kind.name(), "the backend protocol has no such packet"),
        }
    }

    // ---- per-state dispatch ---------------------------------------------

    fn handshake_sb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        if HandshakeSb::from_id(self.client, packet.packet_id) != Some(HandshakeSb::Intention) {
            self.drop_unsupported("minecraft:intention", "unexpected handshake packet");
            return;
        }
        match crate::translate::handshake::intention(&packet.data) {
            Ok((body, next)) => {
                self.sb = next;
                self.cb = next;
                out.to_server(RawPacket::new(packet.packet_id, body));
            }
            Err(e) => {
                // Without a handshake the backend cannot be reached at all,
                // so this ends the connection instead of guessing.
                self.drop_malformed("minecraft:intention", e);
                out.close("elyzune-via: unreadable handshake");
            }
        }
    }

    fn status(&mut self, packet: RawPacket, out: &mut TranslatorOutput, to_client: bool) {
        // Status bodies and ids are identical from 774 to 777; the response's
        // version number is rewritten by the ProxyPing listener instead.
        let known = if to_client {
            StatusCb::from_id(Proto::V776, packet.packet_id).is_some()
        } else {
            StatusSb::from_id(self.client, packet.packet_id).is_some()
        };
        if !known {
            self.drop_unsupported("status", "unknown status packet");
        } else if to_client {
            // The status JSON's `description` is a text component; a hover in
            // it is rewritten like any other (the response is id 0).
            let packet = match crate::translate::login::status_response(self, &packet) {
                Some(body) => RawPacket::new(packet.packet_id, body),
                None => packet,
            };
            out.to_client(packet);
        } else {
            out.to_server(packet);
        }
    }

    fn login_cb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        let Some(kind) = LoginCb::from_id(Proto::V776, packet.packet_id) else {
            self.drop_unsupported("login", "unknown clientbound login packet");
            return;
        };
        let result = match kind {
            LoginCb::LoginFinished => {
                // The client switches to configuration after this packet.
                self.cb = WireState::Config;
                crate::translate::login::login_finished(self, &packet.data)
            }
            LoginCb::LoginDisconnect => crate::translate::login::json_text(self, &packet.data),
            _ => Ok(packet.data),
        };
        match result {
            Ok(body) => match kind.id(self.client) {
                Some(id) => out.to_client(RawPacket::new(id, body)),
                None => {
                    self.drop_unsupported(kind.name(), "the client protocol has no such packet")
                }
            },
            Err(e) => self.drop_malformed(kind.name(), e),
        }
    }

    fn login_sb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        let Some(kind) = LoginSb::from_id(self.client, packet.packet_id) else {
            self.drop_unsupported("login", "unknown serverbound login packet");
            return;
        };
        if kind == LoginSb::LoginAcknowledged {
            self.sb = WireState::Config;
        }
        match kind.id(Proto::V776) {
            Some(id) => out.to_server(RawPacket::new(id, packet.data)),
            None => self.drop_unsupported(kind.name(), "the backend protocol has no such packet"),
        }
    }

    fn config_cb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        let Some(kind) = ConfigCb::from_id(Proto::V776, packet.packet_id) else {
            self.drop_unsupported("configuration", "unknown clientbound configuration packet");
            return;
        };
        if kind == ConfigCb::FinishConfiguration {
            crate::translate::config::before_finish(self, out);
            self.cb = WireState::Play;
            self.emit_config_cb(kind, packet.data, out);
            return;
        }
        if let Err(e) = crate::translate::config::clientbound(self, kind, packet.data, out) {
            self.drop_malformed(kind.name(), e);
        }
    }

    fn config_sb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        let Some(kind) = ConfigSb::from_id(self.client, packet.packet_id) else {
            self.drop_unsupported("configuration", "unknown serverbound configuration packet");
            return;
        };
        if kind == ConfigSb::FinishConfiguration {
            self.sb = WireState::Play;
        }
        if let Err(e) = crate::translate::config::serverbound(self, kind, packet.data, out) {
            self.drop_malformed(kind.name(), e);
        }
    }

    fn play_cb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        let Some(kind) = PlayCb::from_id(Proto::V776, packet.packet_id) else {
            self.drop_unsupported("play", "unknown clientbound play packet");
            return;
        };
        if kind == PlayCb::StartConfiguration {
            self.cb = WireState::Config;
            self.conn.reconfigure();
        }
        if let Err(e) = crate::translate::play::clientbound(self, kind, packet.data, out) {
            self.drop_malformed(kind.name(), e);
        }
    }

    fn play_sb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        let Some(kind) = PlaySb::from_id(self.client, packet.packet_id) else {
            self.drop_unsupported("play", "unknown serverbound play packet");
            return;
        };
        if kind == PlaySb::ConfigurationAcknowledged {
            self.sb = WireState::Config;
        }
        if let Err(e) = crate::translate::play::serverbound(self, kind, packet.data, out) {
            self.drop_malformed(kind.name(), e);
        }
    }
}

impl ProtocolTranslator for ViaTranslator {
    fn backend_version(&self) -> ProtocolVersion {
        ProtocolVersion::new(Proto::V776.number())
    }

    fn clientbound(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        if self.timing.is_none() {
            self.dispatch_cb(packet, out);
            return;
        }
        let what = (self.cb.name(), packet.packet_id);
        let (bytes, started) = (packet.data.len(), Instant::now());
        self.dispatch_cb(packet, out);
        let elapsed = started.elapsed();
        if let Some(timing) = &mut self.timing {
            timing.clientbound.add(bytes, elapsed, what);
        }
    }

    fn serverbound(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        if self.timing.is_none() {
            self.dispatch_sb(packet, out);
            return;
        }
        let what = (self.sb.name(), packet.packet_id);
        let (bytes, started) = (packet.data.len(), Instant::now());
        self.dispatch_sb(packet, out);
        let elapsed = started.elapsed();
        if let Some(timing) = &mut self.timing {
            timing.serverbound.add(bytes, elapsed, what);
        }
    }
}

impl ViaTranslator {
    fn dispatch_cb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        match self.cb {
            // Nothing is sent clientbound before the handshake; a backend
            // that does is not speaking Minecraft.
            WireState::Handshake => {
                self.drop_unsupported("handshake", "clientbound packet before the handshake")
            }
            WireState::Status => self.status(packet, out, true),
            WireState::Login => self.login_cb(packet, out),
            WireState::Config => self.config_cb(packet, out),
            WireState::Play => self.play_cb(packet, out),
        }
    }

    fn dispatch_sb(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
        match self.sb {
            WireState::Handshake => self.handshake_sb(packet, out),
            WireState::Status => self.status(packet, out, false),
            WireState::Login => self.login_sb(packet, out),
            WireState::Config => self.config_sb(packet, out),
            WireState::Play => self.play_sb(packet, out),
        }
    }
}

/// A timed translator reports when its connection ends (the proxy drops the
/// translator with the connection).
impl Drop for ViaTranslator {
    fn drop(&mut self) {
        if let Some(timing) = &self.timing {
            tracing::info!("{}", timing.line(self.connection_id, self.client.number()));
        }
    }
}
