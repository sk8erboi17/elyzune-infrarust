use infrarust_api::filter::TranslatorInit;
use infrarust_api::types::{ProtocolVersion as ApiVersion, ServerId};
use infrarust_config::{ServerAddress, ServerConfig};
use infrarust_protocol::packets::login::SLoginAcknowledged;
use infrarust_protocol::version::{ConnectionState, ProtocolVersion};

use crate::error::CoreError;
use crate::forwarding::build_handshake_for_backend;
use crate::session::backend_bridge::BackendBridge;
use crate::session::context::SessionContext;
use crate::util::domain_rewrite::rewrite_handshake;

pub(crate) enum Login {
    Relay,
    Proxied,
}

pub(crate) async fn connect_backend(
    ctx: &SessionContext<'_>,
    server: &str,
    config: &ServerConfig,
    addresses: &[ServerAddress],
    login: Login,
) -> Result<BackendBridge, CoreError> {
    let version = ctx.version();
    let connection = ctx
        .backend_connector
        .connect(
            server,
            addresses,
            config.timeouts.as_ref().map(|t| t.connect),
            config.send_proxy_protocol,
            &ctx.connection_info,
        )
        .await?;
    let address = connection.server_address().clone();
    // Asked once per backend connection, so a server switch to a backend of
    // another version gets its own translator (or none).
    let translator = ctx
        .services
        .codec_filter_registry
        .create_translator(&TranslatorInit::new(
            ApiVersion::new(version.0),
            ServerId::new(server),
            ctx.player_id().as_u64(),
        ));
    let mut backend = BackendBridge::new(connection.into_stream(), version)
        .with_server_address(address)
        .with_translator(translator);
    if let Some(backend_version) = backend.translated_version() {
        tracing::debug!(
            server,
            client = version.0,
            backend = backend_version.0,
            "translating the backend connection"
        );
    }

    match login {
        Login::Relay => {
            backend.send_initial_packets(&ctx.handshake, config).await?;
        }
        Login::Proxied => {
            let handler = ctx.services.resolve_forwarding_handler(config);
            let data = ctx.forwarding_data();
            if handler.modifies_handshake() {
                let mut handshake = build_handshake_for_backend(&ctx.handshake, config);
                handler.apply_handshake(&mut handshake, &data);
                backend.send_handshake(&handshake).await?;
            } else {
                backend
                    .send_raw(&rewrite_handshake(&ctx.handshake, config)?)
                    .await?;
            }
            backend
                .send_login_start(ctx.username(), ctx.registry())
                .await?;
            let velocity = handler
                .is_velocity()
                .then(|| ctx.services.forwarding_secret())
                .flatten()
                .map(|secret| (&data, secret));
            backend
                .consume_backend_login(ctx.registry(), version, velocity)
                .await?;
            if version.no_less_than(ProtocolVersion::V1_20_2) {
                backend
                    .send_packet(&SLoginAcknowledged, ctx.registry())
                    .await?;
                backend.set_state(ConnectionState::Config);
                tracing::debug!("backend LoginAcknowledged -> Config");
            }
        }
    }

    Ok(backend)
}
