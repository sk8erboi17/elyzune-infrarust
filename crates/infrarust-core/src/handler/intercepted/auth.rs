//! Authentication strategy for intercepted proxy modes.

use std::sync::Arc;

use infrarust_api::events::lifecycle::{OnlineAuthFailedEvent, PreLoginResult};
use infrarust_api::types::{Component, GameProfile, ProfileProperty};
use infrarust_protocol::version::ProtocolVersion;

use crate::auth::game_profile::offline_profile_uuid;
use crate::auth::mojang::MojangAuth;
use crate::error::CoreError;
use crate::pipeline::types::LoginData;
use crate::services::ProxyServices;
use crate::session::admission::{PreLogin, pre_login};
use crate::session::client_bridge::ClientBridge;

pub(crate) enum Authenticated {
    Denied(Component),
    Player {
        profile: GameProfile,
        online_mode: bool,
    },
}

pub(crate) enum AuthStrategy {
    Mojang(Arc<MojangAuth>),
    Offline { mojang: Option<Arc<MojangAuth>> },
}

impl AuthStrategy {
    pub(crate) const fn mode_label(&self) -> &'static str {
        match self {
            Self::Mojang(_) => "client_only",
            Self::Offline { .. } => "offline",
        }
    }

    pub(crate) async fn authenticate(
        &self,
        client: &mut ClientBridge,
        login_data: Option<&LoginData>,
        services: &ProxyServices,
        version: ProtocolVersion,
        remote_addr: std::net::SocketAddr,
        domain: &str,
    ) -> Result<Authenticated, CoreError> {
        let api_version = infrarust_api::types::ProtocolVersion::new(version.0);
        match self {
            Self::Mojang(auth) => {
                let login_data = login_data.ok_or(CoreError::MissingExtension("LoginData"))?;

                let pre_login_profile = GameProfile {
                    uuid: uuid::Uuid::nil(),
                    username: login_data.username.clone(),
                    properties: vec![],
                };
                let pre_login_result = match pre_login(
                    services,
                    pre_login_profile,
                    remote_addr,
                    api_version,
                    domain,
                )
                .await
                {
                    PreLogin::Denied(reason) => return Ok(Authenticated::Denied(reason)),
                    PreLogin::Proceed(result) => result,
                };

                if matches!(pre_login_result, PreLoginResult::ForceOffline) {
                    tracing::info!(
                        username = %login_data.username,
                        "ForceOffline: skipping Mojang auth for client_only player"
                    );
                    return Ok(Authenticated::Player {
                        profile: offline_profile(
                            &login_data.username,
                            login_data.player_uuid,
                            services,
                        ),
                        online_mode: false,
                    });
                }

                online_auth(auth, client, login_data, services).await
            }
            Self::Offline { mojang } => {
                let username = login_data.map(|d| d.username.clone()).unwrap_or_default();
                let profile =
                    offline_profile(&username, login_data.and_then(|d| d.player_uuid), services);

                let pre_login_result =
                    match pre_login(services, profile.clone(), remote_addr, api_version, domain)
                        .await
                    {
                        PreLogin::Denied(reason) => return Ok(Authenticated::Denied(reason)),
                        PreLogin::Proceed(result) => result,
                    };

                if matches!(pre_login_result, PreLoginResult::ForceOnline) {
                    if let Some(auth) = mojang {
                        let login_data =
                            login_data.ok_or(CoreError::MissingExtension("LoginData"))?;
                        tracing::info!(
                            username = %login_data.username,
                            "ForceOnline: upgrading offline player to Mojang auth"
                        );
                        return online_auth(auth, client, login_data, services).await;
                    }
                    // A plugin decided this player must prove who they are
                    // with Mojang, and this route cannot ask Mojang: letting
                    // them in offline would hand a paid name to anyone who
                    // types it. Refused, never downgraded.
                    tracing::warn!(
                        username = %profile.username,
                        "ForceOnline requested but this route has no Mojang authentication; refused"
                    );
                    return Ok(Authenticated::Denied(Component::text(crate::texts::get(
                        crate::texts::Text::AuthenticationUnavailable,
                    ))));
                }

                Ok(Authenticated::Player {
                    profile,
                    online_mode: false,
                })
            }
        }
    }
}

fn offline_profile(
    username: &str,
    claimed: Option<uuid::Uuid>,
    services: &ProxyServices,
) -> GameProfile {
    GameProfile {
        uuid: offline_profile_uuid(services.config.auth.offline_uuid, username, claimed),
        username: username.to_string(),
        properties: vec![],
    }
}

async fn online_auth(
    auth: &MojangAuth,
    client: &mut ClientBridge,
    login_data: &LoginData,
    services: &ProxyServices,
) -> Result<Authenticated, CoreError> {
    let game_profile = match auth
        .authenticate(
            client,
            &login_data.username,
            login_data.profile_key.as_ref(),
            &services.packet_registry,
        )
        .await
    {
        Ok(profile) => profile,
        Err(e) => {
            tracing::warn!(
                username = %login_data.username,
                error = %e,
                "online authentication failed, the client will be disconnected"
            );
            let _ = services
                .event_bus
                .fire(OnlineAuthFailedEvent::new(login_data.username.clone()))
                .await;
            return Err(e);
        }
    };

    // The session server's id is the player's identity everywhere behind this
    // proxy: it is what a backend persists progress under. An id that does
    // not parse is a failed authentication, not a reason to invent a random
    // one -- that would admit the player as a stranger to their own data.
    let uuid = match game_profile.uuid() {
        Ok(uuid) => uuid,
        Err(e) => {
            tracing::warn!(
                username = %game_profile.name,
                id = %game_profile.id,
                error = %e,
                "the session server returned an unusable profile id, the client will be disconnected"
            );
            let _ = services
                .event_bus
                .fire(OnlineAuthFailedEvent::new(login_data.username.clone()))
                .await;
            return Err(e);
        }
    };

    tracing::info!(
        username = %game_profile.name,
        uuid = %game_profile.id,
        "client authenticated"
    );

    Ok(Authenticated::Player {
        profile: GameProfile {
            uuid,
            username: game_profile.name.clone(),
            properties: game_profile
                .properties
                .iter()
                .map(|p| ProfileProperty {
                    name: p.name.clone(),
                    value: p.value.clone(),
                    signature: p.signature.clone(),
                })
                .collect(),
        },
        online_mode: true,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use infrarust_api::event::EventPriority;
    use infrarust_api::event::ResultedEvent;
    use infrarust_api::event::bus::{EventBus, EventBusExt};
    use infrarust_api::events::lifecycle::PreLoginEvent;

    use super::*;
    use crate::limbo::test_helpers::{test_client_bridge, test_proxy_services};

    /// A plugin that demands Mojang authentication on an offline route that
    /// cannot ask Mojang gets a refusal, never an offline login: the paid
    /// name would otherwise be anyone's who types it (fail closed).
    #[tokio::test]
    async fn force_online_without_mojang_is_refused_not_downgraded() {
        let services = test_proxy_services();
        let bus: &dyn EventBus = services.event_bus.as_ref();
        bus.subscribe::<PreLoginEvent, _>(EventPriority::NORMAL, |event: &mut PreLoginEvent| {
            event.set_result(PreLoginResult::ForceOnline);
        });
        let (mut client, _peer) = test_client_bridge(ProtocolVersion(776)).await;
        let login = LoginData {
            username: "Notch".to_owned(),
            player_uuid: None,
            profile_key: None,
        };
        let outcome = AuthStrategy::Offline { mojang: None }
            .authenticate(
                &mut client,
                Some(&login),
                &services,
                ProtocolVersion(776),
                "203.0.113.9:40000".parse().unwrap(),
                "play.test",
            )
            .await
            .unwrap();
        match outcome {
            Authenticated::Denied(reason) => assert_eq!(
                reason.to_plain(),
                crate::texts::Text::AuthenticationUnavailable.english()
            ),
            Authenticated::Player { online_mode, .. } => {
                panic!("admitted (online_mode = {online_mode}) without Mojang")
            }
        }
    }
}
