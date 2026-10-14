//! Elyzune's deployed configuration (`elyzune/` at the repository root) must
//! parse and validate with the production loaders, and must keep the three
//! properties the Elyzune login depends on: the proxy authenticates with Mojang
//! (`client_only`), forwards the identity with Velocity modern forwarding from
//! a host-owned secret file, and no route trusts a client-claimed name
//! (`offline`), because Hyperion admits whatever a correctly signed forward
//! says.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use infrarust_config::{
    ForwardingMode, ProxyConfig, ProxyMode, ServerConfig, validate_proxy_config,
    validate_server_config, validate_server_configs, validate_server_forwarding,
};

fn elyzune() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../elyzune")
}

fn proxy() -> ProxyConfig {
    let path = elyzune().join("infrarust.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn servers() -> Vec<ServerConfig> {
    let dir = elyzune().join("servers");
    let mut servers = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "toml") {
            let text = std::fs::read_to_string(&path).unwrap();
            let mut server: ServerConfig =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            // What the file provider does: the file name is the id unless
            // `name` overrides it.
            server.id = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            servers.push(server);
        }
    }
    servers
}

#[test]
fn the_elyzune_proxy_config_validates() {
    let mut config = proxy();
    // Relative to the directory Infrarust runs from, which is `elyzune/`.
    config.servers_dir = elyzune().join(&config.servers_dir);
    validate_proxy_config(&config).unwrap();
}

#[test]
fn the_proxy_forwards_with_velocity_from_a_host_owned_secret() {
    let forwarding = proxy().forwarding.expect("[forwarding] must be configured");
    assert_eq!(forwarding.mode, ForwardingMode::Velocity);
    assert!(
        forwarding.secret_file.is_absolute(),
        "the secret lives on the host, outside the deployed tree: {}",
        forwarding.secret_file.display()
    );
}

#[test]
fn the_proxy_authenticates_with_mojang() {
    assert_eq!(
        proxy().auth.session_url,
        "https://sessionserver.mojang.com/session/minecraft/hasJoined"
    );
}

#[test]
fn the_client_link_is_compressed_like_velocity_did() {
    assert_eq!(proxy().compression_threshold, 256);
}

#[test]
fn logins_are_rate_limited_like_velocity_did() {
    let limit = proxy().rate_limit;
    assert!(limit.enabled);
    assert_eq!(limit.max_connections, 1);
    assert_eq!(limit.window, std::time::Duration::from_secs(3));
}

#[test]
fn every_route_authenticates_and_forwards() {
    let servers = servers();
    assert!(!servers.is_empty(), "elyzune/servers holds no route");
    let names: Vec<String> = servers.iter().map(ServerConfig::effective_id).collect();
    validate_server_configs(&servers).unwrap();

    for server in &servers {
        let id = server.effective_id();
        let warnings = validate_server_config(server).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(warnings.is_empty(), "{id}: {warnings:?}");
        validate_server_forwarding(server, ForwardingMode::Velocity)
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(
            server.proxy_mode,
            ProxyMode::ClientOnly,
            "{id}: an Elyzune route must authenticate with Mojang; an offline route would \
             forward any claimed name with a valid signature"
        );
        assert_eq!(
            server.forwarding_mode,
            Some(ForwardingMode::Velocity),
            "{id}: Hyperion only admits Velocity-forwarded logins"
        );
        assert_eq!(
            server.network.as_deref(),
            Some("elyzune"),
            "{id}: every game-server process shares one network so players can be moved \
             between them (routes: {names:?})"
        );
    }
}

#[test]
fn the_game_route_catches_every_hostname() {
    let servers = servers();
    let game = servers
        .iter()
        .find(|server| server.effective_id() == "elyzune")
        .expect("the main route is named elyzune");
    assert_eq!(game.domains, ["*"]);
    for address in game.address_list() {
        assert!(
            address.host == "127.0.0.1" || address.host == "localhost",
            "hyperion-proxy is never public: {address:?}"
        );
    }
}

#[test]
fn the_hybrid_login_is_wired_and_the_texts_are_in_a_language_file() {
    let config = proxy();
    // Infrarust's own password plugin would shadow the game's /register and
    // /login: Elyzune's passwords live in the game server.
    assert!(
        config.plugins.get("auth").is_some_and(|auth| !auth.enabled),
        "[plugins.auth] enabled = false"
    );
    let messages = config.messages.expect("[messages] names the language file");
    assert_eq!(messages.language, "en", "Velocity showed these in English");
    assert!(
        elyzune().join(&messages.file).is_file(),
        "{} exists next to infrarust.toml",
        messages.file.display()
    );
    let identity = elyzune().join("plugins/elyzune_identity/config.toml");
    assert!(
        identity.is_file(),
        "elyzune-identity is inactive without {}",
        identity.display()
    );
}
