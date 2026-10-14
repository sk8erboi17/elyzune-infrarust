//! The texts the proxy itself shows players: kick reasons, refusals and
//! the MOTD of an unreachable server.
//!
//! They come from the language file `[messages] file` names (TOML, one
//! table per language, the same shape as Elyzune's `content/lang`):
//!
//! ```toml
//! [en]
//! shutdown = "Proxy shutting down."
//! rate-limited = "You are logging in too fast, try again later."
//! [it]
//! shutdown = "Il server è in fase di arresto."
//! ```
//!
//! and `[messages] language` picks the table. One language for every player
//! on purpose: these texts are shown before the client has said which
//! language it reads (a login refusal comes before configuration), which is
//! also why Velocity showed them in the proxy's own locale. A key the file
//! lacks, or no file at all, keeps the built-in English of [`Text::english`],
//! so a proxy without a language file behaves exactly as before.
//!
//! # Why a process-wide table
//!
//! The texts are read at a dozen kick sites deep in the session code, most
//! of which have no path to the services, and they never change after
//! start: [`install`] fills a `OnceLock` once, before the first connection
//! is accepted, and [`get`] only reads it. Nothing mutable is shared.

use std::{collections::HashMap, path::Path, sync::OnceLock};

static TEXTS: OnceLock<HashMap<&'static str, String>> = OnceLock::new();

/// Every text the proxy shows a player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Text {
    /// The proxy is stopping.
    Shutdown,
    /// A newer login of the same profile replaced this session.
    Replaced,
    /// The backend cannot be reached (no route `disconnect_message`).
    Unreachable,
    /// The MOTD of a server that cannot be reached.
    UnreachableMotd,
    /// A managed server is stopping.
    ServerStopping,
    /// A managed server did not start in time.
    ServerStartTimeout,
    /// A managed server is unavailable.
    ServerUnavailable,
    /// A plugin refused the connection without a reason.
    NotAllowed,
    /// The per-address connection limit refused the connection.
    RateLimited,
    /// A ban could not be checked (the check fails closed).
    BanCheckUnavailable,
    LimboUnavailable,
    UnknownServer,
    ProxyLoginServer,
    CodecFilterFailed,
    CodecFilterUnavailable,
    InternalError,
    /// `/kick` without a reason.
    Kicked,
    NoPermission,
    CommandDenied,
    /// Mojang authentication was required but this route has none.
    AuthenticationUnavailable,
}

impl Text {
    pub const ALL: [Self; 20] = [
        Self::Shutdown,
        Self::Replaced,
        Self::Unreachable,
        Self::UnreachableMotd,
        Self::ServerStopping,
        Self::ServerStartTimeout,
        Self::ServerUnavailable,
        Self::NotAllowed,
        Self::RateLimited,
        Self::BanCheckUnavailable,
        Self::LimboUnavailable,
        Self::UnknownServer,
        Self::ProxyLoginServer,
        Self::CodecFilterFailed,
        Self::CodecFilterUnavailable,
        Self::InternalError,
        Self::Kicked,
        Self::NoPermission,
        Self::CommandDenied,
        Self::AuthenticationUnavailable,
    ];

    /// The key in the language file.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Replaced => "replaced",
            Self::Unreachable => "unreachable",
            Self::UnreachableMotd => "unreachable-motd",
            Self::ServerStopping => "server-stopping",
            Self::ServerStartTimeout => "server-start-timeout",
            Self::ServerUnavailable => "server-unavailable",
            Self::NotAllowed => "not-allowed",
            Self::RateLimited => "rate-limited",
            Self::BanCheckUnavailable => "ban-check-unavailable",
            Self::LimboUnavailable => "limbo-unavailable",
            Self::UnknownServer => "unknown-server",
            Self::ProxyLoginServer => "proxy-login-server",
            Self::CodecFilterFailed => "codec-filter-failed",
            Self::CodecFilterUnavailable => "codec-filter-unavailable",
            Self::InternalError => "internal-error",
            Self::Kicked => "kicked",
            Self::NoPermission => "no-permission",
            Self::CommandDenied => "command-denied",
            Self::AuthenticationUnavailable => "authentication-unavailable",
        }
    }

    /// The built-in text: Infrarust's own English, unchanged.
    #[must_use]
    pub const fn english(self) -> &'static str {
        match self {
            Self::Shutdown => "Proxy is shutting down",
            Self::Replaced => "You logged in from another location",
            Self::Unreachable => "Server is currently unreachable. Please try again later.",
            Self::UnreachableMotd => "\u{00a7}cServer unreachable",
            Self::ServerStopping => "Server is shutting down, please try again later.",
            Self::ServerStartTimeout => "Server failed to start in time. Please try again.",
            Self::ServerUnavailable => "Server is unavailable. Please try again later.",
            Self::NotAllowed => "You are not allowed to connect to this server",
            Self::RateLimited => "Rate limit exceeded",
            Self::BanCheckUnavailable => {
                "Your ban status cannot be checked right now. Please try again later."
            }
            Self::LimboUnavailable => "Limbo is not available on this server",
            Self::UnknownServer => "Unknown server",
            Self::ProxyLoginServer => "This server cannot be joined from here",
            Self::CodecFilterFailed => "Disconnected: a required packet filter failed.",
            Self::CodecFilterUnavailable => {
                "Connection refused: a required packet filter is not available."
            }
            Self::InternalError => "Internal error",
            Self::Kicked => "Kicked by an administrator",
            Self::NoPermission => "You don't have permission.",
            Self::CommandDenied => "You don't have permission to use this command.",
            Self::AuthenticationUnavailable => {
                "This server requires authentication, which is unavailable right now."
            }
        }
    }
}

/// The text to show for `text`: the language file's, else the built-in.
#[must_use]
pub fn get(text: Text) -> &'static str {
    TEXTS
        .get()
        .and_then(|texts| texts.get(text.key()))
        .map_or_else(|| text.english(), String::as_str)
}

/// Reads `[language]` of `file`.
///
/// # Errors
/// When the file cannot be read or parsed, the language has no table, or
/// the table holds a key that is not a [`Text`] or a value that is not text
/// (subtables, which plugins read for themselves, are skipped).
pub fn read(file: &Path, language: &str) -> Result<HashMap<&'static str, String>, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| format!("reading {}: {error}", file.display()))?;
    let root: toml::Table =
        toml::from_str(&text).map_err(|error| format!("parsing {}: {error}", file.display()))?;
    let table = root
        .get(language)
        .and_then(toml::Value::as_table)
        .ok_or_else(|| format!("{} has no [{language}] table", file.display()))?;
    let mut texts = HashMap::new();
    for (key, value) in table {
        if value.is_table() {
            continue;
        }
        let known = Text::ALL
            .iter()
            .find(|text| text.key() == key)
            .ok_or_else(|| format!("{}: [{language}] {key} is not a proxy text", file.display()))?;
        let value = value
            .as_str()
            .ok_or_else(|| format!("{}: [{language}] {key} is not text", file.display()))?;
        texts.insert(known.key(), value.to_owned());
    }
    Ok(texts)
}

/// Installs the texts once, before the first connection. A second call is
/// refused: the texts never change while the proxy runs.
///
/// # Errors
/// As [`read`], or when the texts were already installed.
pub fn install(file: &Path, language: &str) -> Result<(), String> {
    let texts = read(file, language)?;
    TEXTS
        .set(texts)
        .map_err(|_| "the proxy texts are already installed".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn keys_are_unique_and_every_text_has_english() {
        let keys: std::collections::HashSet<_> = Text::ALL.iter().map(|t| t.key()).collect();
        assert_eq!(keys.len(), Text::ALL.len());
        assert!(Text::ALL.iter().all(|t| !t.english().is_empty()));
        assert_eq!(get(Text::Replaced), "You logged in from another location");
    }

    #[test]
    fn a_language_table_overrides_and_refuses_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("infrarust-texts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lang.toml");
        std::fs::write(
            &file,
            "[it]\nshutdown = \"Il server è in fase di arresto.\"\n[it.identity]\nx = \"y\"\n",
        )
        .unwrap();
        let it = read(&file, "it").unwrap();
        assert_eq!(
            it.get("shutdown").map(String::as_str),
            Some("Il server è in fase di arresto.")
        );
        assert!(read(&file, "en").is_err(), "a missing language is an error");
        std::fs::write(&file, "[en]\nshutdwn = \"typo\"\n").unwrap();
        assert!(read(&file, "en").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
