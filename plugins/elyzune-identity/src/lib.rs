//! Elyzune's hybrid login, proxy half (user decision of 13 October 2026).
//!
//! ```text
//! PreLoginEvent (before any encryption)
//!   name a vanilla client cannot send ............... refused
//!   a paid name whose Mojang login just failed ....... refused (paid-name)
//!   who owns the name? (memory, else Mojang, rate-limited, timed out)
//!     a paid account .. ForceOnline: Mojang authenticates, or no entry
//!     nobody .......... a password name; refused if that name (any
//!                       spelling) is connected now, else ForceOffline
//!     unknown ......... refused (unavailable): the failure is closed
//! GameProfileRequestEvent (after authentication)
//!   uuid = OfflinePlayer:<name>, the id every live player keeps
//!   property elyzune:login = mojang | password (signed by the forwarding
//!   HMAC with the rest of the profile; any incoming one is dropped first)
//! OnlineAuthFailedEvent
//!   a paid name whose client could not authenticate: remembered, so the
//!   next attempts are refused at once with the paid-name text
//! ```
//!
//! The game server (`elyzune_core::login`) admits `mojang` at once and holds
//! everyone else until `/register` or `/login`. Why refuse a second password
//! connection: until it types the password, nothing proves it is the same
//! person, and admitting it would make Infrarust disconnect the player who
//! is already in (live Velocity, `kick-existing-players = false`, refused it
//! the same way). A Mojang-authenticated login has proven who it is, so it
//! displaces an older session of its own name, as Infrarust does.
//!
//! Every route behind this plugin must be `client_only` with Mojang
//! authentication available: `ForceOnline` on an offline route that has no
//! Mojang client is refused by the proxy (fail closed), never let through.

pub mod config;
pub mod lookup;

use std::{sync::Arc, time::Duration};

use infrarust_api::{
    error::PluginError,
    event::{BoxFuture, EventPriority, ResultedEvent, bus::EventBusExt},
    events::lifecycle::{
        GameProfileRequestEvent, OnlineAuthFailedEvent, PreLoginEvent, PreLoginResult,
    },
    plugin::{Plugin, PluginContext, PluginMetadata},
    services::player_registry::PlayerRegistry,
    types::{Component, GameProfile, ProfileProperty},
};
use md5::{Digest, Md5};

use crate::{
    config::{IdentityConfig, Texts},
    lookup::{LookupError, Memory, MojangLookup, Ownership},
};

pub const PLUGIN_ID: &str = "elyzune_identity";
/// The forwarded property the game server reads.
pub const LOGIN_PROPERTY: &str = "elyzune:login";
pub const MOJANG: &str = "mojang";
pub const PASSWORD: &str = "password";

/// Java's `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))`:
/// MD5, version 3, IETF variant. The id the live (offline-mode) server gave
/// every player, and the key of every PostgreSQL row.
#[must_use]
pub fn offline_uuid(name: &str) -> uuid::Uuid {
    let mut digest: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(digest)
}

/// What a vanilla client may send as its name: 1 to 16 characters, none of
/// them blank or a control (`StringUtil.isValidPlayerName`).
#[must_use]
pub fn valid_name(name: &str) -> bool {
    let length = name.chars().count();
    (1..=16).contains(&length) && name.chars().all(|c| c > ' ' && c != '\u{7f}')
}

/// Whether Mojang could have issued `name` (`[A-Za-z0-9_]{1,16}`); any other
/// name is a password name without asking.
#[must_use]
pub fn mojang_spelling(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The decision for one login, from what is known about its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Online,
    Offline,
    Refuse(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    InvalidName,
    PaidName,
    AlreadyConnected,
    Unavailable,
}

/// The rule, without I/O: `owner` is who owns the name (or why that is
/// unknown), `connected` whether a session under that name (any spelling)
/// is connected now, `failed` whether a Mojang login of it failed recently.
#[must_use]
pub fn decide(
    name: &str,
    owner: Option<&Result<Ownership, LookupError>>,
    connected: bool,
    failed: bool,
) -> Decision {
    if !valid_name(name) {
        return Decision::Refuse(Refusal::InvalidName);
    }
    match owner {
        Some(Ok(Ownership::Paid { .. })) if failed => Decision::Refuse(Refusal::PaidName),
        Some(Ok(Ownership::Paid { .. })) => Decision::Online,
        Some(Ok(Ownership::Free)) | None if connected => {
            Decision::Refuse(Refusal::AlreadyConnected)
        }
        Some(Ok(Ownership::Free)) | None => Decision::Offline,
        Some(Err(_)) => Decision::Refuse(Refusal::Unavailable),
    }
}

/// The profile the game server is sent: the offline id of the name, and the
/// login property saying how the player was authenticated.
pub fn rewrite(profile: &mut GameProfile, online: bool) {
    profile.uuid = offline_uuid(&profile.username);
    profile
        .properties
        .retain(|property| property.name != LOGIN_PROPERTY);
    profile.properties.push(ProfileProperty {
        name: LOGIN_PROPERTY.to_owned(),
        value: if online { MOJANG } else { PASSWORD }.to_owned(),
        signature: None,
    });
}

struct Identity {
    lookup: MojangLookup,
    memory: Memory,
    texts: Texts,
    players: Arc<dyn PlayerRegistry>,
}

impl Identity {
    async fn pre_login(&self, event: &mut PreLoginEvent) {
        let name = event.profile.username.clone();
        let owner = if !valid_name(&name) || !mojang_spelling(&name) {
            None
        } else if let Some(known) = self.memory.owner(&name) {
            Some(Ok(known))
        } else {
            let answer = self.lookup.owner(&name).await;
            match &answer {
                Ok(ownership) => self.memory.remember(&name, ownership.clone()),
                Err(error) => {
                    tracing::warn!(%name, %error, "the owner of a name could not be checked; refused")
                }
            }
            Some(answer)
        };
        let connected = self.players.get_player(&name).is_some();
        let failed = self.memory.recently_failed(&name);
        let decision = decide(&name, owner.as_ref(), connected, failed);
        tracing::info!(%name, ?decision, "identity");
        match decision {
            Decision::Online => event.set_result(PreLoginResult::ForceOnline),
            Decision::Offline => event.set_result(PreLoginResult::ForceOffline),
            Decision::Refuse(refusal) => {
                let text = match refusal {
                    Refusal::InvalidName => &self.texts.invalid_name,
                    Refusal::PaidName => &self.texts.paid_name,
                    Refusal::AlreadyConnected => &self.texts.already_connected,
                    Refusal::Unavailable => &self.texts.unavailable,
                };
                event.deny(Component::from_legacy(text));
            }
        }
    }
}

/// The plugin.
#[derive(Default)]
pub struct ElyzuneIdentityPlugin;

impl Plugin for ElyzuneIdentityPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(PLUGIN_ID, "Elyzune Identity", env!("CARGO_PKG_VERSION"))
            .author("Elyzune")
            .description("Paid Mojang names log in with Mojang, every other name with a password")
    }

    fn on_enable<'a>(
        &'a self,
        ctx: &'a dyn PluginContext,
    ) -> BoxFuture<'a, Result<(), PluginError>> {
        Box::pin(async move {
            let path = ctx.data_dir().join("config.toml");
            let text = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tracing::info!(
                        "elyzune-identity: no {}; inactive (no name is looked up)",
                        path.display()
                    );
                    return Ok(());
                }
                Err(error) => {
                    return Err(PluginError::InitFailed(format!(
                        "elyzune-identity: reading {}: {error}",
                        path.display()
                    )));
                }
            };
            let config: IdentityConfig = toml::from_str(&text).map_err(|error| {
                PluginError::InitFailed(format!("elyzune-identity: {}: {error}", path.display()))
            })?;
            let texts = match &config.language_file {
                Some(file) => Texts::load(file, &config.language).map_err(|error| {
                    PluginError::InitFailed(format!("elyzune-identity: {error}"))
                })?,
                None => Texts::default(),
            };
            let lookup = MojangLookup::new(
                &config.profile_url,
                Duration::from_millis(config.timeout_ms.max(1)),
                config.rate,
            )
            .map_err(|error| PluginError::InitFailed(format!("elyzune-identity: {error}")))?;
            let identity = Arc::new(Identity {
                lookup,
                memory: Memory::new(
                    config.cache,
                    Duration::from_secs(config.failed_auth_seconds),
                ),
                texts,
                players: ctx.player_registry(),
            });

            let pre_login = Arc::clone(&identity);
            ctx.event_bus().subscribe_async::<PreLoginEvent, _>(
                EventPriority::EARLY,
                move |event: &mut PreLoginEvent| {
                    let identity = Arc::clone(&pre_login);
                    Box::pin(async move { identity.pre_login(event).await })
                },
            );
            ctx.event_bus().subscribe::<GameProfileRequestEvent, _>(
                EventPriority::EARLY,
                |event: &mut GameProfileRequestEvent| {
                    let online = event.online_mode;
                    rewrite(&mut event.profile, online);
                },
            );
            let failures = Arc::clone(&identity);
            ctx.event_bus().subscribe::<OnlineAuthFailedEvent, _>(
                EventPriority::NORMAL,
                move |event: &mut OnlineAuthFailedEvent| {
                    if matches!(
                        failures.memory.owner(&event.username),
                        Some(Ownership::Paid { .. })
                    ) {
                        failures.memory.failed(&event.username);
                    }
                },
            );
            let sweeper = Arc::clone(&identity);
            let stop = ctx.proxy_shutdown();
            tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_secs(60));
                loop {
                    tokio::select! {
                        _ = every.tick() => sweeper.memory.sweep(),
                        () = stop.cancelled() => return,
                    }
                }
            });
            tracing::info!(
                url = %config.profile_url,
                "elyzune-identity enabled: paid names log in with Mojang, others with a password"
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_offline_id_is_javas() {
        // UUID.nameUUIDFromBytes("OfflinePlayer:Load64"): the live id the
        // core's session tests use.
        assert_eq!(
            offline_uuid("Load64").to_string(),
            "7107c273-81b1-3488-93e8-6ccfa91b5ff8"
        );
        assert_ne!(offline_uuid("Steve"), offline_uuid("steve"), "case matters");
    }

    #[test]
    fn names_vanilla_and_mojang() {
        assert!(valid_name("Steve") && mojang_spelling("Steve"));
        assert!(valid_name("Stévé") && !mojang_spelling("Stévé"));
        assert!(!valid_name("") && !valid_name("a b") && !valid_name(&"x".repeat(17)));
    }

    #[test]
    fn the_decision_table() {
        let paid = Ok(Ownership::Paid {
            name: "Notch".to_owned(),
        });
        let free = Ok(Ownership::Free);
        let down = Err(LookupError::RateLimited);
        assert_eq!(decide("Notch", Some(&paid), false, false), Decision::Online);
        assert_eq!(
            decide("Notch", Some(&paid), true, false),
            Decision::Online,
            "Mojang proves the owner: it may displace its own older session"
        );
        assert_eq!(
            decide("Notch", Some(&paid), false, true),
            Decision::Refuse(Refusal::PaidName)
        );
        assert_eq!(
            decide("Steve", Some(&free), false, false),
            Decision::Offline
        );
        assert_eq!(
            decide("Steve", Some(&free), true, false),
            Decision::Refuse(Refusal::AlreadyConnected)
        );
        assert_eq!(
            decide("Steve", Some(&down), false, false),
            Decision::Refuse(Refusal::Unavailable)
        );
        assert_eq!(decide("Stévé", None, false, false), Decision::Offline);
        assert_eq!(
            decide("a b", None, false, false),
            Decision::Refuse(Refusal::InvalidName)
        );
    }

    #[test]
    fn the_profile_carries_the_offline_id_and_one_login_property() {
        let mut profile = GameProfile {
            uuid: uuid::Uuid::from_u128(42),
            username: "Notch".to_owned(),
            properties: vec![
                ProfileProperty {
                    name: "textures".to_owned(),
                    value: "skin".to_owned(),
                    signature: Some("sig".to_owned()),
                },
                ProfileProperty {
                    name: LOGIN_PROPERTY.to_owned(),
                    value: MOJANG.to_owned(),
                    signature: None,
                },
            ],
        };
        rewrite(&mut profile, false);
        assert_eq!(profile.uuid, offline_uuid("Notch"));
        let logins: Vec<_> = profile
            .properties
            .iter()
            .filter(|property| property.name == LOGIN_PROPERTY)
            .map(|property| property.value.as_str())
            .collect();
        assert_eq!(logins, [PASSWORD], "a smuggled property never survives");
        assert!(profile.properties.iter().any(|p| p.name == "textures"));
        rewrite(&mut profile, true);
        assert!(
            profile
                .properties
                .iter()
                .any(|p| p.name == LOGIN_PROPERTY && p.value == MOJANG)
        );
    }
}
