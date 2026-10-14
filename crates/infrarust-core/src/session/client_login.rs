use std::sync::OnceLock;
use std::time::Duration;

use infrarust_api::types::GameProfile;
use infrarust_config::ServerConfig;
use infrarust_protocol::VarInt;
use infrarust_protocol::packets::cookie::SLoginCookieResponse;
use infrarust_protocol::packets::login::{
    CLoginSuccess, CSetCompression, Property, SLoginAcknowledged, SLoginPluginResponse,
};
use infrarust_protocol::registry::{DecodedPacket, PacketRegistry};
use infrarust_protocol::version::{ConnectionState, Direction, ProtocolVersion};
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::session::client_bridge::ClientBridge;

/// How long a login the proxy completes itself waits for the client's
/// `LoginAcknowledged`: the `[timeouts] read` of the route the client arrived
/// on, 30 s when the route sets none. A vanilla client acknowledges the moment
/// it reads `LoginSuccess`, so the wait is one round trip; the bound exists for
/// the clients that never answer.
pub(crate) fn login_ack_timeout(route: &ServerConfig) -> Duration {
    route
        .timeouts
        .as_ref()
        .map_or_else(infrarust_config::defaults::read_timeout, |t| t.read)
}

/// Finishes a login the proxy runs itself: optional compression, then
/// `LoginSuccess` with `profile`, then (1.20.2+) the client's acknowledgement.
///
/// `compression_threshold` is `[infrarust.toml] compression_threshold`. When it
/// is non-negative the client is sent `SetCompression` first and the bridge
/// switches both directions to the compressed framing before `LoginSuccess`,
/// which is the order a vanilla server uses and the only one a client accepts:
/// it applies the threshold to the very next frame it reads. Only call this
/// while the client link is still in its login framing, which every caller
/// guarantees by checking the login has not been completed yet.
///
/// The wait for the acknowledgement is bounded, because every caller runs
/// this after the player was admitted: the session is registered under its
/// UUID, `PostLoginEvent` has fired, and on an offline route nothing has been
/// authenticated yet. An unbounded wait let any client that reads
/// `LoginSuccess` and goes silent hold that registration, its socket and its
/// task until it chose to leave, and kept a displacing login of the same
/// profile (and the proxy's own drain at shutdown) waiting on it. So:
///
/// - `ack_timeout` (see [`login_ack_timeout`]) bounds the whole wait, late
///   frames included: past it the result is `CoreError::Timeout`.
/// - `token` is the session's token, cancelled by a kick, by a newer login of
///   the same profile and by proxy shutdown; it ends the wait at once with
///   `CoreError::Rejected`. Callers tell the two apart with
///   `token.is_cancelled()` and end the player's lifecycle with the cause.
pub(crate) async fn complete_login(
    client: &mut ClientBridge,
    profile: &GameProfile,
    version: ProtocolVersion,
    registry: &PacketRegistry,
    compression_threshold: i32,
    ack_timeout: Duration,
    token: &CancellationToken,
) -> Result<(), CoreError> {
    // SetCompression exists since 1.8; older clients would read it as garbage.
    if compression_threshold >= 0 && version.no_less_than(ProtocolVersion::V1_8) {
        client
            .send_packet(
                &CSetCompression {
                    threshold: VarInt(compression_threshold),
                },
                registry,
            )
            .await?;
        client.set_compression(compression_threshold);
        tracing::debug!(
            threshold = compression_threshold,
            "client compression enabled"
        );
    }

    let properties: Vec<Property> = profile
        .properties
        .iter()
        .map(|p| Property {
            name: p.name.clone(),
            value: p.value.clone(),
            signature: p.signature.clone(),
        })
        .collect();
    send_login_success(
        client,
        profile.uuid,
        &profile.username,
        &properties,
        version,
        registry,
    )
    .await?;

    if version.no_less_than(ProtocolVersion::V1_20_2) {
        tokio::select! {
            biased;
            () = token.cancelled() => Err(CoreError::Rejected(
                "the session ended while waiting for the client's LoginAcknowledged".to_string(),
            )),
            acknowledged = tokio::time::timeout(
                ack_timeout,
                consume_login_acknowledged(client, version, registry),
            ) => acknowledged.unwrap_or_else(|_| {
                Err(CoreError::Timeout(format!(
                    "the client did not acknowledge its login within {ack_timeout:?}"
                )))
            }),
        }
    } else {
        client.set_state(ConnectionState::Play);
        Ok(())
    }
}

async fn send_login_success(
    client: &mut ClientBridge,
    uuid: uuid::Uuid,
    username: &str,
    properties: &[Property],
    version: ProtocolVersion,
    registry: &PacketRegistry,
) -> Result<(), CoreError> {
    static SESSION_ID: OnceLock<uuid::Uuid> = OnceLock::new();

    let login_success = CLoginSuccess {
        uuid,
        username: username.to_string(),
        properties: properties.to_vec(),
        strict_error_handling: version.no_less_than(ProtocolVersion::V1_20_5)
            && version.no_greater_than(ProtocolVersion::V1_21),
        session_id: version
            .no_less_than(ProtocolVersion::V26_2)
            .then(|| *SESSION_ID.get_or_init(uuid::Uuid::new_v4)),
    };

    client.send_packet(&login_success, registry).await?;
    tracing::debug!("sent LoginSuccess to client");
    Ok(())
}

/// Reads frames until the client's `LoginAcknowledged`.
///
/// Answers to login requests the client was sent before this login was
/// completed here are skipped, not taken for the acknowledgement: in relay
/// mode a backend may send the client a `LoginPluginRequest` or a login
/// `CookieRequest` and then kick it, and when a plugin redirects that kick the
/// session completes the login itself (`leave_login`) while the client's
/// answer is still on the wire. Any other frame is a protocol error. The
/// caller's deadline covers the whole loop, so a client streaming such
/// answers cannot extend its wait (upstream Shadowner/Infrarust#102,
/// 1f9baf0674).
async fn consume_login_acknowledged(
    client: &mut ClientBridge,
    version: ProtocolVersion,
    registry: &PacketRegistry,
) -> Result<(), CoreError> {
    let late = [
        registry.get_packet_id::<SLoginPluginResponse>(version),
        registry.get_packet_id::<SLoginCookieResponse>(version),
    ];
    let frame = loop {
        let frame = client
            .read_frame()
            .await?
            .ok_or(CoreError::ConnectionClosed)?;
        if !late.contains(&Some(frame.id)) {
            break frame;
        }
        tracing::debug!(
            id = frame.id,
            "dropping a late login answer sent before the client's LoginAcknowledged"
        );
    };

    let decoded = registry.decode_frame(
        &frame,
        ConnectionState::Login,
        Direction::Serverbound,
        version,
    )?;

    match decoded {
        DecodedPacket::Typed { packet, .. }
            if packet
                .as_any()
                .downcast_ref::<SLoginAcknowledged>()
                .is_some() =>
        {
            client.set_state(ConnectionState::Config);
            tracing::debug!("client LoginAcknowledged -> Config");
            Ok(())
        }
        _ => Err(CoreError::Protocol(
            infrarust_protocol::ProtocolError::invalid("expected LoginAcknowledged from client"),
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use bytes::BytesMut;
    use infrarust_protocol::io::{PacketDecoder, PacketEncoder};
    use infrarust_protocol::packets::Packet;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::Instant;

    use super::*;
    use crate::limbo::test_helpers::test_registry;

    async fn pair() -> (ClientBridge, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        (
            ClientBridge::new(stream, BytesMut::new(), ProtocolVersion::V26_2),
            peer,
        )
    }

    async fn next_frame(
        peer: &mut TcpStream,
        decoder: &mut PacketDecoder,
    ) -> infrarust_protocol::io::PacketFrame {
        loop {
            if let Some(frame) = decoder.try_next_frame().unwrap() {
                return frame;
            }
            let buf = decoder.read_buf_mut();
            buf.reserve(4096);
            assert!(peer.read_buf(buf).await.unwrap() > 0, "proxy hung up");
        }
    }

    /// `[timeouts] read` when a route sets none.
    const ACK_TIMEOUT: Duration = Duration::from_secs(30);

    /// The deadline of the tests that wait it out. Real time, not tokio's
    /// paused clock: with the clock paused the runtime counts a socket wait as
    /// idle and may jump to the next timer before the peer's bytes are read,
    /// which would let a deadline restarted by every frame pass for a bound.
    const SHORT: Duration = Duration::from_millis(400);

    /// How long a test waits before calling a login stuck.
    const STUCK: Duration = Duration::from_secs(10);

    /// An uncompressed 26.2 login of [`steve`].
    async fn login_steve(
        client: &mut ClientBridge,
        registry: &PacketRegistry,
        ack_timeout: Duration,
        token: &CancellationToken,
    ) -> Result<(), CoreError> {
        complete_login(
            client,
            &steve(),
            ProtocolVersion::V26_2,
            registry,
            -1,
            ack_timeout,
            token,
        )
        .await
    }

    fn encode<P: Packet>(encoder: &mut PacketEncoder, registry: &PacketRegistry, packet: &P) {
        let id = registry.get_packet_id::<P>(ProtocolVersion::V26_2).unwrap();
        let mut payload = Vec::new();
        packet.encode(&mut payload, ProtocolVersion::V26_2).unwrap();
        encoder.append_raw(id, &payload).unwrap();
    }

    fn late_plugin_answer(message_id: i32) -> SLoginPluginResponse {
        SLoginPluginResponse {
            message_id: VarInt(message_id),
            successful: false,
            data: Vec::new(),
        }
    }

    fn steve() -> GameProfile {
        GameProfile {
            uuid: crate::auth::game_profile::offline_uuid("Steve"),
            username: "Steve".to_string(),
            properties: vec![],
        }
    }

    /// The client must read `SetCompression` uncompressed and everything after
    /// it -- `LoginSuccess` first -- in the compressed framing, and the
    /// acknowledgement it sends back is compressed too. A threshold applied one
    /// frame late or early is a client that disconnects on a bad packet id.
    #[tokio::test]
    async fn compression_is_negotiated_before_login_success() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let login = tokio::spawn(async move {
            complete_login(
                &mut client,
                &steve(),
                ProtocolVersion::V26_2,
                &registry,
                256,
                ACK_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            client
        });

        let mut decoder = PacketDecoder::new();
        let set_compression = next_frame(&mut peer, &mut decoder).await;
        assert_eq!(set_compression.id, 0x03);
        assert_eq!(set_compression.payload.as_ref(), [0x80, 0x02]); // VarInt(256)

        decoder.set_compression(256);
        let login_success = next_frame(&mut peer, &mut decoder).await;
        assert_eq!(login_success.id, 0x02);

        let mut encoder = PacketEncoder::new();
        encoder.set_compression(256);
        encoder.append_raw(0x03, &[]).unwrap(); // LoginAcknowledged
        peer.write_all(&encoder.take()).await.unwrap();

        let client = login.await.unwrap();
        assert_eq!(client.compression_threshold(), Some(256));
        assert_eq!(client.state(), ConnectionState::Config);
    }

    #[tokio::test]
    async fn a_negative_threshold_leaves_the_link_uncompressed() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let login = tokio::spawn(async move {
            login_steve(
                &mut client,
                &registry,
                ACK_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            client
        });

        let mut decoder = PacketDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await.id, 0x02);

        let mut encoder = PacketEncoder::new();
        encoder.append_raw(0x03, &[]).unwrap();
        peer.write_all(&encoder.take()).await.unwrap();

        let client = login.await.unwrap();
        assert_eq!(client.compression_threshold(), None);
    }

    /// Upstream Shadowner/Infrarust#102 (51b1c01286). A client that reads its
    /// `LoginSuccess` and never acknowledges it held the login -- and the
    /// player the proxy had already registered under its UUID -- forever. It
    /// must be let go at the acknowledgement deadline, and no earlier.
    #[tokio::test]
    async fn a_silent_client_does_not_hold_the_login_forever() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let token = CancellationToken::new();
        let started = Instant::now();
        let login =
            tokio::spawn(async move { login_steve(&mut client, &registry, SHORT, &token).await });

        let mut decoder = PacketDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await.id, 0x02);

        // The socket stays open and silent.
        let result = tokio::time::timeout(STUCK, login)
            .await
            .expect("complete_login was still waiting for LoginAcknowledged")
            .unwrap();
        let waited = started.elapsed();
        assert!(matches!(result, Err(CoreError::Timeout(_))), "{result:?}");
        assert!(waited >= SHORT, "gave up after {waited:?}");
        drop(peer);
    }

    /// A kick, a newer login of the same profile and proxy shutdown cancel the
    /// session's token; the wait must end then, not at the deadline, or the
    /// displacing login and the proxy's drain both wait on a silent client.
    #[tokio::test]
    async fn a_cancelled_session_stops_waiting_for_the_acknowledgement() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let token = CancellationToken::new();
        let session = token.clone();
        let login =
            tokio::spawn(async move { login_steve(&mut client, &registry, STUCK, &session).await });

        let mut decoder = PacketDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await.id, 0x02);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cancelled_at = Instant::now();
        token.cancel();

        let result = tokio::time::timeout(2 * STUCK, login)
            .await
            .expect("complete_login outlived its deadline")
            .unwrap();
        assert!(matches!(result, Err(CoreError::Rejected(_))), "{result:?}");
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "the wait outlived its session by {:?}",
            cancelled_at.elapsed()
        );
        drop(peer);
    }

    /// Upstream Shadowner/Infrarust#102 (1f9baf0674). Answers to login
    /// requests sent before the proxy completed the login reach it ahead of
    /// the acknowledgement and must not be read as a malformed one.
    #[tokio::test]
    async fn late_login_answers_before_the_acknowledgement_are_skipped() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let peer_registry = test_registry();
        let login = tokio::spawn(async move {
            login_steve(
                &mut client,
                &registry,
                ACK_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .map(|()| client)
        });

        let mut decoder = PacketDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await.id, 0x02);
        let mut encoder = PacketEncoder::new();
        encode(&mut encoder, &peer_registry, &late_plugin_answer(7));
        encode(
            &mut encoder,
            &peer_registry,
            &SLoginCookieResponse {
                key: "elyzune:session".to_string(),
                payload: None,
            },
        );
        encode(&mut encoder, &peer_registry, &SLoginAcknowledged);
        peer.write_all(&encoder.take()).await.unwrap();

        let client = login.await.unwrap().unwrap();
        assert_eq!(client.state(), ConnectionState::Config);
    }

    /// The deadline bounds the whole wait, skipped frames included: a client
    /// trickling late answers faster than the deadline must still be let go
    /// at it.
    #[tokio::test]
    async fn late_answers_do_not_extend_the_wait() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let peer_registry = test_registry();
        let started = Instant::now();
        let login = tokio::spawn(async move {
            login_steve(&mut client, &registry, SHORT, &CancellationToken::new()).await
        });

        let mut decoder = PacketDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await.id, 0x02);
        let trickle = tokio::spawn(async move {
            for message_id in 0..1000 {
                tokio::time::sleep(SHORT / 4).await;
                let mut encoder = PacketEncoder::new();
                encode(
                    &mut encoder,
                    &peer_registry,
                    &late_plugin_answer(message_id),
                );
                if peer.write_all(&encoder.take()).await.is_err() {
                    break;
                }
            }
        });

        let result = tokio::time::timeout(STUCK, login)
            .await
            .expect("late answers kept the login waiting")
            .unwrap();
        let waited = started.elapsed();
        assert!(matches!(result, Err(CoreError::Timeout(_))), "{result:?}");
        assert!(
            waited >= SHORT && waited < SHORT + Duration::from_secs(2),
            "gave up after {waited:?}"
        );
        trickle.abort();
    }

    /// Anything else in place of the acknowledgement is still refused.
    #[tokio::test]
    async fn another_frame_in_place_of_the_acknowledgement_is_refused() {
        let (mut client, mut peer) = pair().await;
        let registry = test_registry();
        let login = tokio::spawn(async move {
            login_steve(
                &mut client,
                &registry,
                ACK_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
        });

        let mut decoder = PacketDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await.id, 0x02);
        let mut encoder = PacketEncoder::new();
        encoder
            .append_raw(0x00, &[0x05, b'S', b't', b'e', b'v', b'e'])
            .unwrap(); // LoginStart again
        peer.write_all(&encoder.take()).await.unwrap();

        let result = login.await.unwrap();
        assert!(matches!(result, Err(CoreError::Protocol(_))), "{result:?}");
    }
}
