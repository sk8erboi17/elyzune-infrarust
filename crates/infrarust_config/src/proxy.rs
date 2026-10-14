//! Global proxy configuration (`infrarust.toml`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::defaults;
use crate::types::{
    ActiveHealthConfig, AuthConfig, BanConfig, DockerProviderConfig, EventsConfig,
    ForwardingConfig, IpFilterConfig, KeepaliveConfig, MessagesConfig, MotdConfig,
    PermissionsConfig, PluginMessagingConfig, PluginWasmConfig, RateLimitConfig, StatusCacheConfig,
    TelemetryConfig, WasmConfig, WebConfig,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownDomainBehavior {
    #[default]
    DefaultMotd,
    Drop,
}

/// Corresponds to the `infrarust.toml` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Listen address, e.g., "0.0.0.0:25565"
    #[serde(default = "defaults::bind")]
    pub bind: SocketAddr,

    /// Maximum number of simultaneous connections (0 = unlimited)
    #[serde(default)]
    pub max_connections: u32,

    /// Backend connection timeout
    #[serde(default = "defaults::connect_timeout")]
    #[serde(with = "humantime_serde")]
    pub connect_timeout: Duration,

    /// Maximum backend addresses tried before giving up on a connection.
    /// Bounds worst-case login latency to `attempts × connect_timeout`.
    #[serde(default = "defaults::connect_max_attempts")]
    pub connect_max_attempts: usize,

    /// Compression the proxy negotiates with clients whose login it completes
    /// itself: `client_only`, and `offline` when it forwards with Velocity or
    /// rewrote the profile. Frames of at least this many bytes are
    /// zlib-compressed on the client link; -1 leaves that link uncompressed.
    ///
    /// The backend link negotiates its own compression independently, so a
    /// frame is decompressed from the backend and recompressed for the client
    /// on proxy threads, as Velocity does with its `compression-threshold`.
    /// Forwarding modes never use this: there the backend talks to the client.
    #[serde(default = "defaults::compression_threshold")]
    pub compression_threshold: i32,

    /// Enables receiving proxy protocol (`HAProxy` v1/v2)
    #[serde(default)]
    pub receive_proxy_protocol: bool,

    /// Path to the server configuration directory
    #[serde(default = "defaults::servers_dir")]
    pub servers_dir: PathBuf,

    #[serde(default = "defaults::plugins_dir")]
    pub plugins_dir: PathBuf,

    /// Number of tokio worker threads (0 = auto)
    #[serde(default)]
    pub worker_threads: usize,

    /// Global rate limiting configuration
    #[serde(default)]
    pub rate_limit: RateLimitConfig,

    /// Status ping cache configuration
    #[serde(default)]
    pub status_cache: StatusCacheConfig,

    /// Default MOTD when no server matches
    #[serde(default)]
    pub default_motd: Option<MotdConfig>,

    /// Telemetry configuration (absent = disabled)
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,

    /// TCP keepalive configuration
    #[serde(default)]
    pub keepalive: KeepaliveConfig,

    /// Enables `SO_REUSEPORT` (Linux only)
    #[serde(default)]
    pub so_reuseport: bool,

    /// Ban system configuration
    #[serde(default)]
    pub ban: BanConfig,

    /// Docker provider configuration (optional).
    /// Present in the TOML even without the `docker` feature compiled.
    #[serde(default)]
    pub docker: Option<DockerProviderConfig>,

    #[serde(default)]
    pub ip_filter: Option<IpFilterConfig>,

    #[serde(default)]
    pub unknown_domain_behavior: UnknownDomainBehavior,

    #[serde(default = "defaults::true_val")]
    pub announce_proxy_commands: bool,

    #[serde(default)]
    pub forwarding: Option<ForwardingConfig>,

    #[serde(default)]
    pub auth: AuthConfig,

    /// The language file of the texts shown to players (absent: the
    /// built-in English).
    #[serde(default)]
    pub messages: Option<MessagesConfig>,

    /// Web admin API / UI configuration (absent = web plugin not loaded).
    #[serde(default)]
    pub web: Option<WebConfig>,

    #[serde(default)]
    pub permissions: PermissionsConfig,

    /// Background probing that brings ejected backend addresses back.
    #[serde(default)]
    pub active_health: ActiveHealthConfig,

    #[serde(default)]
    pub events: EventsConfig,

    #[serde(default)]
    pub plugin_messaging: PluginMessagingConfig,

    #[serde(default)]
    pub wasm: WasmConfig,

    /// Plugin configurations keyed by plugin ID.
    #[serde(default)]
    pub plugins: HashMap<String, PluginConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    /// Path to the plugin binary/library.
    #[serde(default)]
    pub path: Option<String>,

    /// Permissions granted to this plugin.
    #[serde(default)]
    pub permissions: Vec<String>,

    #[serde(default)]
    pub deny: Vec<String>,

    #[serde(default)]
    pub strict_capabilities: bool,

    #[serde(default)]
    pub wasm: Option<PluginWasmConfig>,

    /// Whether the plugin is enabled (default: true).
    #[serde(default = "defaults::true_val")]
    pub enabled: bool,
}
