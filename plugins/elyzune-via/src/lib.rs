//! elyzune-via: one protocol translator for Elyzune, replacing ViaVersion,
//! ViaBackwards and ViaRewind for the supported range only.
//!
//! The Elyzune backends (Hyperion) speak Minecraft 26.2, protocol 776. This
//! plugin lets clients on 1.21.11 (774), 26.1 (775) and 26.3 (777) play on
//! them through Infrarust; 26.2 clients are relayed untouched. Translation
//! runs per connection on the proxy's threads, in both directions and every
//! protocol state, through the core's protocol translator hook (see
//! `infrarust_api::filter::translate`).
//!
//! # Structure
//!
//! - `generated/`: every fact about a release (packet ids, registry ids, block
//!   states, entity data layouts, component layouts, tags, synchronised
//!   registries), generated from the vanilla jars by
//!   `tools/generate_tables.py`.
//! - `session`: the per-connection translator and its state machine.
//! - `translate/`: per-state packet handlers; `chunk`, `entity`, `item`,
//!   `particle`, `recipe`, `commands` hold the rewriting of each family.
//! - `layout`: the interpreter that walks generated component layouts.
//!
//! # Requirements
//!
//! Translation needs the proxy to parse packets, so the server must use an
//! intercepted proxy mode (`client_only`, `offline` or `full`), and the plugin
//! needs the codec filter capability (granted to built-in plugins).

pub mod chunk;
pub mod codec;
pub mod commands;
pub mod debug;
pub mod entity;
pub mod generated;
pub mod hashed;
pub mod item;
pub mod layout;
pub mod nbt;
pub mod particle;
pub mod recipe;
pub mod registry_data;
pub mod session;
pub mod state;
pub mod stats;
pub mod tables;
pub mod text;
pub mod translate;
pub mod wire;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use infrarust_api::command::{CommandContext, CommandHandler, CommandSpec};
use infrarust_api::error::PluginError;
use infrarust_api::event::bus::EventBusExt;
use infrarust_api::event::{BoxFuture, EventPriority};
use infrarust_api::events::proxy::ProxyPingEvent;
use infrarust_api::filter::{ProtocolTranslator, ProtocolTranslatorFactory, TranslatorInit};
use infrarust_api::plugin::{Plugin, PluginContext, PluginMetadata};
use infrarust_api::types::{ClickEvent, Component, ProtocolVersion};
use serde::Deserialize;

use crate::generated::Proto;
use crate::session::ViaTranslator;

/// Identifier of the plugin and of its translator factory.
pub const PLUGIN_ID: &str = "elyzune_via";

/// `config.toml` in the plugin's data directory.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViaConfig {
    /// Configured server ids whose backend speaks 26.2 and is translated.
    /// Empty means every server: the Elyzune network has no other backend.
    #[serde(default)]
    pub servers: Vec<String>,
    /// Public source of the proxy distributed to these players. When set,
    /// `/source` offers the URL without a permission or a backend command.
    #[serde(default)]
    pub source_url: Option<String>,
}

const DEFAULT_CONFIG: &str = "\
# elyzune-via: translates 1.21.11, 26.1 and 26.3 clients to 26.2 backends.
#
# Server ids (from the Infrarust server configuration) whose backend speaks
# 26.2. Leave empty to translate every server.
servers = []
";

/// Decides, per backend connection, whether to translate.
pub struct ViaFactory {
    servers: Option<HashSet<String>>,
}

impl ViaFactory {
    #[must_use]
    pub fn new(config: &ViaConfig) -> Self {
        let servers =
            (!config.servers.is_empty()).then(|| config.servers.iter().cloned().collect());
        Self { servers }
    }

    /// The client protocols this plugin translates.
    #[must_use]
    pub fn translates(client: i32) -> Option<Proto> {
        match Proto::from_number(client)? {
            Proto::V776 => None,
            other => Some(other),
        }
    }
}

impl ProtocolTranslatorFactory for ViaFactory {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn create(&self, init: &TranslatorInit) -> Option<Box<dyn ProtocolTranslator>> {
        if let Some(servers) = &self.servers
            && !servers.contains(init.server.as_str())
        {
            return None;
        }
        let client = Self::translates(init.client_version.raw())?;
        tracing::debug!(
            connection = init.connection_id,
            server = %init.server,
            client = client.number(),
            "elyzune-via: translating connection"
        );
        Some(Box::new(ViaTranslator::new(client, init.connection_id)))
    }
}

/// The plugin.
#[derive(Default)]
pub struct ElyzuneViaPlugin;

struct SourceCommand(String);

impl CommandHandler for SourceCommand {
    fn execute<'a>(&'a self, ctx: CommandContext) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            ctx.source.send_message(
                Component::text(&self.0)
                    .color("#88C0E8")
                    .underlined()
                    .click(ClickEvent::OpenUrl(self.0.clone())),
            );
        })
    }
}

impl Plugin for ElyzuneViaPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(PLUGIN_ID, "Elyzune Via", env!("CARGO_PKG_VERSION"))
            .author("Elyzune")
            .description("Translates 1.21.11, 26.1 and 26.3 clients to 26.2 backends")
    }

    fn on_enable<'a>(
        &'a self,
        ctx: &'a dyn PluginContext,
    ) -> BoxFuture<'a, Result<(), PluginError>> {
        Box::pin(async move {
            let config = load_config(&ctx.data_dir().join("config.toml")).await?;
            if let Some(url) = &config.source_url {
                if !url.starts_with("https://")
                    || url.len() <= 8
                    || url.chars().any(char::is_whitespace)
                {
                    return Err(PluginError::InitFailed(
                        "elyzune-via: source_url must be an HTTPS URL".to_owned(),
                    ));
                }
                ctx.command_manager()
                    .register(
                        CommandSpec::new("source"),
                        Box::new(SourceCommand(url.clone())),
                    )
                    .map_err(|error| {
                        PluginError::InitFailed(format!("elyzune-via: /source: {error}"))
                    })?;
            }
            let registry = ctx.codec_filters().ok_or_else(|| {
                PluginError::InitFailed("elyzune-via needs the codec filter capability".to_owned())
            })?;
            registry
                .register_translator(Box::new(ViaFactory::new(&config)))
                .map_err(|e| PluginError::InitFailed(format!("elyzune-via: {e}")))?;

            // The status answer carries the backend's protocol (776); a
            // translated client would show the server as incompatible.
            let servers: Arc<Option<HashSet<String>>> = Arc::new(ViaFactory::new(&config).servers);
            ctx.event_bus()
                .subscribe::<ProxyPingEvent, _>(EventPriority::NORMAL, move |event| {
                    let client = event.protocol_version.raw();
                    let applies = servers.as_ref().as_ref().is_none_or(|set| {
                        event
                            .server
                            .as_ref()
                            .is_some_and(|server| set.contains(server.as_str()))
                    });
                    if applies
                        && ViaFactory::translates(client).is_some()
                        && event.response.protocol_version.raw() == Proto::V776.number()
                    {
                        event.response.protocol_version = ProtocolVersion::new(client);
                    }
                });
            tracing::info!(
                servers = ?config.servers,
                "elyzune-via enabled: 774, 775 and 777 clients are translated to 776"
            );
            Ok(())
        })
    }
}

async fn load_config(path: &Path) -> Result<ViaConfig, PluginError> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => toml::from_str(&text).map_err(|e| {
            PluginError::InitFailed(format!("elyzune-via config {}: {e}", path.display()))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| PluginError::InitFailed(format!("elyzune-via: {e}")))?;
            }
            tokio::fs::write(path, DEFAULT_CONFIG)
                .await
                .map_err(|e| PluginError::InitFailed(format!("elyzune-via: {e}")))?;
            Ok(ViaConfig::default())
        }
        Err(e) => Err(PluginError::InitFailed(format!(
            "elyzune-via config {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use infrarust_api::types::ServerId;

    fn init(version: i32, server: &str) -> TranslatorInit {
        TranslatorInit::new(ProtocolVersion::new(version), ServerId::new(server), 1)
    }

    #[test]
    fn only_the_supported_clients_are_translated() {
        let factory = ViaFactory::new(&ViaConfig::default());
        for (version, translated) in [
            (774, true),
            (775, true),
            (776, false),
            (777, true),
            (773, false),
            (778, false),
        ] {
            assert_eq!(
                factory.create(&init(version, "lobby")).is_some(),
                translated,
                "{version}"
            );
        }
        let translator = factory.create(&init(777, "lobby")).unwrap();
        assert_eq!(translator.backend_version().raw(), 776);
    }

    #[test]
    fn a_server_list_limits_translation() {
        let factory = ViaFactory::new(&ViaConfig {
            servers: vec!["elyzune".to_owned()],
            ..ViaConfig::default()
        });
        assert!(factory.create(&init(774, "elyzune")).is_some());
        assert!(factory.create(&init(774, "other")).is_none());
    }

    #[test]
    fn the_default_config_parses() {
        let config: ViaConfig = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(config.servers.is_empty());
        assert!(config.source_url.is_none());
    }

    #[tokio::test]
    async fn the_source_offer_is_clickable_without_staff_permission() {
        use infrarust_api::test_util::{MockPlayer, command_context, player_source};

        let url = "https://github.com/sk8erboi17/elyzune-infrarust";
        let player = MockPlayer::new(1, "Visitor").into_arc();
        SourceCommand(url.to_owned())
            .execute(command_context(player_source(&player), "source", ""))
            .await;
        let messages = player.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to_plain(), url);
        assert_eq!(
            messages[0].style.click.as_deref(),
            Some(&ClickEvent::OpenUrl(url.to_owned()))
        );
    }
}
