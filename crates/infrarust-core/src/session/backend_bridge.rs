//! Backend-side bridge for intercepted proxy modes.
//!
//! Wraps the backend TCP stream with packet codec and optional compression.
//!
//! # Protocol translation
//!
//! When the session's client and the backend speak different protocol
//! versions, the bridge owns a [`ProtocolTranslator`] and becomes the version
//! boundary: every frame it hands to the core has been translated to the
//! client's version, and every frame the core queues is translated to the
//! backend's version before it reaches the encoder. The core keeps using
//! [`BackendBridge::protocol_version`] (the client's version) for everything
//! it encodes or decodes, so nothing above this file needs to know.
//!
//! Every byte written to the backend goes through [`queue_frame`] in that
//! mode, including the raw handshake and login bytes the relay path forwards,
//! because the translator tracks the connection state from the frames it sees
//! and must see all of them, in wire order.
//!
//! [`queue_frame`]: BackendBridge::queue_frame

use std::collections::VecDeque;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use infrarust_api::filter::{ProtocolTranslator, TranslatorOutput};
use infrarust_api::types::RawPacket;
use infrarust_config::ServerConfig;
use infrarust_protocol::Packet;
use infrarust_protocol::io::{PacketDecoder, PacketEncoder, PacketFrame};
use infrarust_protocol::packets::handshake::SHandshake;
use infrarust_protocol::packets::login::{
    CLoginDisconnect, CLoginPluginRequest, CLoginSuccess, CSetCompression, SLoginPluginResponse,
    SLoginStart,
};
use infrarust_protocol::registry::{DecodedPacket, PacketRegistry};
use infrarust_protocol::version::{ConnectionState, Direction, ProtocolVersion};

use crate::auth::game_profile::offline_uuid;
use crate::error::CoreError;
use crate::pipeline::types::HandshakeData;
use crate::player::packets::encode_for_state;
use crate::session::kick::BackendKick;
use crate::util::domain_rewrite::rewrite_handshake;

const READ_CHUNK: usize = 16 * 1024;

/// The backend side of a proxied connection.
///
/// Can be replaced during a server switch (Phase 4+).
pub struct BackendBridge {
    stream: TcpStream,
    decoder: PacketDecoder,
    encoder: PacketEncoder,
    /// Current protocol state.
    pub state: ConnectionState,
    /// Protocol version of this connection, as the core sees it: the client's
    /// version, also when a translator makes the backend speak another one.
    pub protocol_version: ProtocolVersion,
    server_address: Option<infrarust_config::ServerAddress>,
    translation: Option<Translation>,
}

/// The translation state of one backend connection.
struct Translation {
    translator: Box<dyn ProtocolTranslator>,
    /// Frames in the client's version waiting to be handed to the core: the
    /// translated backend frames, and the frames the translator produced for
    /// the client while translating a serverbound one. Drained before the
    /// decoder is asked for another backend frame, so the order the
    /// translator emitted is the order the core sees.
    pending: VecDeque<PacketFrame>,
    /// Reused output buffer; empty between calls.
    out: TranslatorOutput,
}

impl Translation {
    /// Moves one call's outputs to their destinations: backend-bound frames
    /// into the encoder, client-bound frames into `pending`.
    fn settle(&mut self, encoder: &mut PacketEncoder) -> Result<(), CoreError> {
        for packet in self.out.take_to_server() {
            encoder.append_frame(&PacketFrame::new(packet.packet_id, packet.data))?;
        }
        for packet in self.out.take_to_client() {
            self.pending
                .push_back(PacketFrame::new(packet.packet_id, packet.data));
        }
        match self.out.take_close() {
            Some(reason) => Err(CoreError::Translation(reason)),
            None => Ok(()),
        }
    }
}

impl BackendBridge {
    pub fn new(stream: TcpStream, protocol_version: ProtocolVersion) -> Self {
        Self {
            stream,
            decoder: PacketDecoder::new(),
            encoder: PacketEncoder::new(),
            state: ConnectionState::Login,
            protocol_version,
            server_address: None,
            translation: None,
        }
    }

    /// Makes this connection translate between the core's version and the
    /// backend's. Must be called before anything is written to the backend.
    #[must_use]
    pub fn with_translator(mut self, translator: Option<Box<dyn ProtocolTranslator>>) -> Self {
        self.translation = translator.map(|translator| Translation {
            translator,
            pending: VecDeque::new(),
            out: TranslatorOutput::new(),
        });
        self
    }

    /// The version the backend speaks on the wire, when it differs from the
    /// core's.
    pub fn translated_version(&self) -> Option<ProtocolVersion> {
        self.translation
            .as_ref()
            .map(|t| ProtocolVersion(t.translator.backend_version().raw()))
    }

    #[must_use]
    pub fn with_server_address(mut self, address: infrarust_config::ServerAddress) -> Self {
        self.server_address = Some(address);
        self
    }

    pub const fn server_address(&self) -> Option<&infrarust_config::ServerAddress> {
        self.server_address.as_ref()
    }

    /// Reads the next packet frame from the backend.
    ///
    /// Returns `Ok(None)` on clean disconnect (EOF).
    ///
    /// # Errors
    /// Returns `CoreError` on I/O or protocol decode errors.
    pub async fn read_frame(&mut self) -> Result<Option<PacketFrame>, CoreError> {
        loop {
            if let Some(frame) = self.try_next_frame()? {
                return Ok(Some(frame));
            }

            let buf = self.decoder.read_buf_mut();
            buf.reserve(READ_CHUNK);
            let n = self.stream.read_buf(buf).await?;
            if n == 0 {
                return Ok(None);
            }
        }
    }

    /// Extracts the next frame already buffered in the decoder, without
    /// reading from the socket. Used to drain a read that carried several
    /// frames before flushing them as one write.
    ///
    /// # Errors
    /// Returns `CoreError` on protocol decode errors.
    pub fn try_next_frame(&mut self) -> Result<Option<PacketFrame>, CoreError> {
        let Some(translation) = self.translation.as_mut() else {
            return Ok(self.decoder.try_next_frame()?);
        };
        // A backend frame can translate to nothing (a packet the client has
        // no equivalent for), so keep decoding until something is ready or
        // the buffered bytes run out.
        loop {
            if let Some(frame) = translation.pending.pop_front() {
                return Ok(Some(frame));
            }
            let Some(frame) = self.decoder.try_next_frame()? else {
                return Ok(None);
            };
            translation.translator.clientbound(
                RawPacket::new(frame.id, frame.payload),
                &mut translation.out,
            );
            translation.settle(&mut self.encoder)?;
        }
    }

    /// Queues a frame in the core's version for the backend, translating it
    /// first when the backend speaks another version.
    pub fn queue_frame(&mut self, frame: &PacketFrame) -> Result<(), CoreError> {
        let Some(translation) = self.translation.as_mut() else {
            return Ok(self.encoder.append_frame(frame)?);
        };
        translation.translator.serverbound(
            RawPacket::new(frame.id, frame.payload.clone()),
            &mut translation.out,
        );
        translation.settle(&mut self.encoder)
    }

    /// Writes bytes that are already framed (uncompressed, as a client sends
    /// them before compression is negotiated) to the backend.
    ///
    /// Without a translator the bytes go out untouched. With one, they are
    /// split back into frames and queued one by one, so the translator sees
    /// the handshake and the login start and can rewrite them.
    async fn write_framed_bytes(&mut self, bytes: &[u8]) -> Result<(), CoreError> {
        if self.translation.is_none() {
            self.stream.write_all(bytes).await?;
            return Ok(());
        }
        let mut frames = PacketDecoder::new();
        frames.queue_bytes(bytes);
        while let Some(frame) = frames.try_next_frame()? {
            self.queue_frame(&frame)?;
        }
        if !frames.read_buf_mut().is_empty() {
            return Err(CoreError::InvalidState(
                "a forwarded login packet was truncated",
            ));
        }
        self.flush().await
    }

    pub async fn flush(&mut self) -> Result<(), CoreError> {
        let data = self.encoder.take();
        if data.is_empty() {
            return Ok(());
        }
        self.stream.write_all(&data).await?;
        Ok(())
    }

    pub async fn write_frame(&mut self, frame: &PacketFrame) -> Result<(), CoreError> {
        self.queue_frame(frame)?;
        self.flush().await
    }

    pub async fn send_packet<P: Packet>(
        &mut self,
        packet: &P,
        registry: &PacketRegistry,
    ) -> Result<(), CoreError> {
        let frame = encode_for_state(packet, self.state, self.protocol_version, registry)?;
        self.write_frame(&frame).await
    }

    /// Activates packet compression with the given threshold, or leaves it
    /// disabled if `threshold` is negative.
    pub const fn set_compression(&mut self, threshold: i32) {
        self.decoder.set_compression(threshold);
        self.encoder.set_compression(threshold);
    }

    /// Changes the protocol state.
    pub const fn set_state(&mut self, state: ConnectionState) {
        self.state = state;
    }

    /// Sends handshake + login start packets to the backend.
    ///
    /// Applies domain rewrite according to the server config.
    /// Used by `OfflineHandler` where the client's original login is forwarded.
    ///
    /// # Errors
    /// Returns `CoreError` on handshake rewrite or I/O errors.
    pub async fn send_initial_packets(
        &mut self,
        handshake_data: &HandshakeData,
        server_config: &ServerConfig,
    ) -> Result<(), CoreError> {
        // Write (possibly rewritten) handshake
        let handshake_bytes = rewrite_handshake(handshake_data, server_config)?;
        self.write_framed_bytes(&handshake_bytes).await?;

        // Forward remaining raw packets (login start, etc.) as-is
        for raw in handshake_data.raw_packets.iter().skip(1) {
            self.write_framed_bytes(raw).await?;
        }

        self.stream.flush().await?;
        Ok(())
    }

    pub async fn send_handshake(&mut self, handshake: &SHandshake) -> Result<(), CoreError> {
        let mut payload = Vec::new();
        handshake.encode(&mut payload, self.protocol_version)?;
        self.queue_frame(&PacketFrame::new(0x00, payload.into()))?;
        self.flush().await
    }

    /// Writes already-framed bytes (a handshake) to the backend.
    pub async fn send_raw(&mut self, bytes: &[u8]) -> Result<(), CoreError> {
        self.write_framed_bytes(bytes).await
    }

    pub async fn send_login_start(
        &mut self,
        username: &str,
        registry: &PacketRegistry,
    ) -> Result<(), CoreError> {
        let login_start = SLoginStart {
            name: username.to_string(),
            uuid: Some(offline_uuid(username)),
            profile_key: None,
        };
        self.send_packet(&login_start, registry).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Consumes the backend's login response without forwarding to the client.
    ///
    /// Reads `SetCompression` (activates compression on backend) and `LoginSuccess`
    /// (consumed without forwarding). Returns error on `LoginDisconnect`.
    ///
    /// After this call, the backend is ready for the next phase
    /// (Config for 1.20.2+, or Play for older versions).
    ///
    /// # Errors
    /// Returns `CoreError` on I/O errors, protocol errors, or backend rejection.
    pub async fn consume_backend_login(
        &mut self,
        registry: &PacketRegistry,
        version: ProtocolVersion,
        velocity_ctx: Option<(&crate::forwarding::ForwardingData, &[u8])>,
    ) -> Result<(), CoreError> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let frame = self
                    .read_frame()
                    .await?
                    .ok_or(CoreError::ConnectionClosed)?;

                let decoded = registry.decode_frame(
                    &frame,
                    ConnectionState::Login,
                    Direction::Clientbound,
                    version,
                )?;

                match decoded {
                    DecodedPacket::Typed { packet, .. } => {
                        if let Some(set_comp) = packet.as_any().downcast_ref::<CSetCompression>() {
                            self.set_compression(set_comp.threshold.0);
                            tracing::debug!(
                                threshold = set_comp.threshold.0,
                                "backend compression activated"
                            );
                            continue;
                        }

                        if packet.as_any().downcast_ref::<CLoginSuccess>().is_some() {
                            if version.less_than(ProtocolVersion::V1_20_2) {
                                self.set_state(ConnectionState::Play);
                            }
                            tracing::debug!("consumed backend LoginSuccess");
                            break;
                        }

                        if packet.as_any().downcast_ref::<CLoginDisconnect>().is_some() {
                            return Err(CoreError::BackendKick(Box::new(BackendKick::new(
                                frame,
                                ConnectionState::Login,
                                version,
                            ))));
                        }

                        if let Some(request) = packet.as_any().downcast_ref::<CLoginPluginRequest>()
                        {
                            if crate::forwarding::velocity::is_velocity_request(request) {
                                if let Some((fwd_data, secret)) = velocity_ctx {
                                    let response =
                                        crate::forwarding::velocity::build_velocity_response(
                                            request, fwd_data, secret,
                                        )?;
                                    self.send_packet(&response, registry).await?;
                                    tracing::info!("velocity forwarding applied (auto-detected)");
                                    continue;
                                }
                                tracing::debug!(
                                    "backend requested velocity forwarding but no secret is \
                                     configured — responding 'not understood'"
                                );
                            } else {
                                tracing::debug!(
                                    channel = %request.channel,
                                    "responding 'not understood' to LoginPluginRequest"
                                );
                            }

                            let response = SLoginPluginResponse {
                                message_id: request.message_id,
                                successful: false,
                                data: Vec::new(),
                            };
                            self.send_packet(&response, registry).await?;
                            continue;
                        }
                    }
                    DecodedPacket::Opaque { id, .. } => {
                        tracing::debug!(id, "ignoring opaque packet during backend login");
                    }
                }
            }

            Ok(())
        })
        .await
        .map_err(|_| CoreError::Rejected("backend login timed out".to_string()))?
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    use infrarust_api::types::ProtocolVersion as ApiVersion;
    use tokio::net::TcpListener;

    /// Stands in for a real translator: rewrites the handshake's protocol
    /// number, shifts play ids by one in each direction, answers serverbound
    /// id 0x40 with a frame for the client, and drops clientbound id 0x7F.
    struct ShiftTranslator {
        handshaken: bool,
    }

    impl ProtocolTranslator for ShiftTranslator {
        fn backend_version(&self) -> ApiVersion {
            ApiVersion::new(776)
        }

        fn clientbound(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
            match packet.packet_id {
                0x7F => {}
                0x10 => {
                    out.to_client(RawPacket::new(0x0F, packet.data));
                    out.to_server(RawPacket::new(0x33, bytes::Bytes::from_static(b"ack")));
                }
                0x66 => out.close("unknown packet"),
                id => out.to_client(RawPacket::new(id - 1, packet.data)),
            }
        }

        fn serverbound(&mut self, packet: RawPacket, out: &mut TranslatorOutput) {
            if !self.handshaken {
                self.handshaken = true;
                // Handshake: VarInt protocol first; 774 and 776 are both two
                // bytes, so the rewrite is a splice.
                let mut data = packet.data.to_vec();
                let mut version = Vec::new();
                infrarust_protocol::VarInt(776)
                    .encode(&mut version)
                    .expect("vec write");
                data.splice(0..2, version);
                out.to_server(RawPacket::new(packet.packet_id, data.into()));
                return;
            }
            if packet.packet_id == 0x40 {
                out.to_client(RawPacket::new(0x41, bytes::Bytes::from_static(b"reply")));
                return;
            }
            out.to_server(RawPacket::new(packet.packet_id + 1, packet.data));
        }
    }

    async fn pair() -> (BackendBridge, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let bridge = BackendBridge::new(client.unwrap(), ProtocolVersion::V1_21_11)
            .with_translator(Some(Box::new(ShiftTranslator { handshaken: false })));
        (bridge, accepted.unwrap().0)
    }

    async fn read_frames(stream: &mut TcpStream, count: usize) -> Vec<PacketFrame> {
        let mut decoder = PacketDecoder::new();
        let mut frames = Vec::new();
        while frames.len() < count {
            if let Some(frame) = decoder.try_next_frame().unwrap() {
                frames.push(frame);
                continue;
            }
            let n = stream.read_buf(decoder.read_buf_mut()).await.unwrap();
            assert_ne!(n, 0, "backend side closed early");
        }
        frames
    }

    async fn write_frames(stream: &mut TcpStream, frames: &[(i32, &'static [u8])]) {
        let mut encoder = PacketEncoder::new();
        for (id, payload) in frames {
            encoder.append_raw(*id, payload).unwrap();
        }
        stream.write_all(&encoder.take()).await.unwrap();
    }

    #[tokio::test]
    async fn raw_handshake_bytes_are_translated_before_they_reach_the_backend() {
        let (mut bridge, mut backend) = pair().await;
        assert_eq!(bridge.translated_version(), Some(ProtocolVersion(776)));

        let mut payload = Vec::new();
        infrarust_protocol::VarInt(774)
            .encode(&mut payload)
            .unwrap();
        payload.extend_from_slice(b"rest");
        let mut encoder = PacketEncoder::new();
        encoder.append_raw(0x00, &payload).unwrap();
        bridge.send_raw(&encoder.take()).await.unwrap();

        let frames = read_frames(&mut backend, 1).await;
        let mut expected = Vec::new();
        infrarust_protocol::VarInt(776)
            .encode(&mut expected)
            .unwrap();
        expected.extend_from_slice(b"rest");
        assert_eq!(frames[0].id, 0x00);
        assert_eq!(frames[0].payload.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn frames_are_translated_both_ways_in_order() {
        let (mut bridge, mut backend) = pair().await;
        bridge.send_raw(&handshake_frame()).await.unwrap();
        read_frames(&mut backend, 1).await;

        // Backend → core: dropped, mapped, and one that also answers the
        // backend through the encoder.
        write_frames(
            &mut backend,
            &[(0x7F, b"gone"), (0x21, b"a"), (0x10, b"b"), (0x30, b"c")],
        )
        .await;
        let mut ids = Vec::new();
        while ids.len() < 3 {
            let frame = bridge.read_frame().await.unwrap().unwrap();
            ids.push((frame.id, frame.payload.to_vec()));
        }
        assert_eq!(
            ids,
            vec![
                (0x20, b"a".to_vec()),
                (0x0F, b"b".to_vec()),
                (0x2F, b"c".to_vec())
            ]
        );

        // Core → backend: one mapped frame, one answered towards the core.
        bridge
            .queue_frame(&PacketFrame::new(0x05, bytes::Bytes::from_static(b"x")))
            .unwrap();
        bridge
            .queue_frame(&PacketFrame::new(0x40, bytes::Bytes::from_static(b"ping")))
            .unwrap();
        bridge.flush().await.unwrap();
        let frames = read_frames(&mut backend, 2).await;
        assert_eq!(
            (frames[0].id, frames[0].payload.as_ref()),
            (0x33, &b"ack"[..])
        );
        assert_eq!(
            (frames[1].id, frames[1].payload.as_ref()),
            (0x06, &b"x"[..])
        );

        let reply = bridge.try_next_frame().unwrap().unwrap();
        assert_eq!((reply.id, reply.payload.as_ref()), (0x41, &b"reply"[..]));
        assert!(bridge.try_next_frame().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_translator_close_ends_the_read_with_its_reason() {
        let (mut bridge, mut backend) = pair().await;
        write_frames(&mut backend, &[(0x66, b"")]).await;
        let err = bridge.read_frame().await.unwrap_err();
        assert!(matches!(err, CoreError::Translation(ref r) if r == "unknown packet"));
    }

    #[tokio::test]
    async fn without_a_translator_bytes_are_relayed_untouched() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut bridge = BackendBridge::new(client.unwrap(), ProtocolVersion::V1_21_11);
        let mut backend = accepted.unwrap().0;
        assert!(bridge.translated_version().is_none());

        bridge.send_raw(&handshake_frame()).await.unwrap();
        let frames = read_frames(&mut backend, 1).await;
        let mut expected = Vec::new();
        infrarust_protocol::VarInt(774)
            .encode(&mut expected)
            .unwrap();
        assert_eq!(frames[0].payload.as_ref(), expected.as_slice());

        write_frames(&mut backend, &[(0x21, b"a")]).await;
        let frame = bridge.read_frame().await.unwrap().unwrap();
        assert_eq!((frame.id, frame.payload.as_ref()), (0x21, &b"a"[..]));
    }

    fn handshake_frame() -> Vec<u8> {
        let mut payload = Vec::new();
        infrarust_protocol::VarInt(774)
            .encode(&mut payload)
            .unwrap();
        let mut encoder = PacketEncoder::new();
        encoder.append_raw(0x00, &payload).unwrap();
        encoder.take().to_vec()
    }
}
