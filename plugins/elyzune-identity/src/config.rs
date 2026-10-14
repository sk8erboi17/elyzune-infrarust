//! `plugins/elyzune_identity/config.toml` and the texts the plugin sends.
//!
//! No file, no plugin: without its configuration the plugin stays inactive
//! and never asks Mojang anything, so an Infrarust built with it behaves as
//! before on every other deployment. Elyzune ships the file in
//! `elyzune/plugins/elyzune_identity/config.toml`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Mojang's profile-by-name lookup: `GET <url>/<name>` answers 200 with the
/// profile of the account that owns the name, 204 or 404 when none does.
pub const MOJANG_PROFILES: &str = "https://api.mojang.com/users/profiles/minecraft";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    /// Where names are looked up (a loopback stand-in in tests).
    #[serde(default = "default_profile_url")]
    pub profile_url: String,
    /// One lookup's whole budget, connection included.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub rate: RateConfig,
    /// How long a failed Mojang authentication of a paid name is remembered:
    /// the next attempts under that name are refused at once with the
    /// "paid name" text instead of another encryption round.
    #[serde(default = "default_failed_auth_seconds")]
    pub failed_auth_seconds: u64,
    /// The proxy's language file (`[<language>.identity]` holds this
    /// plugin's texts), relative to Infrarust's working directory. Absent:
    /// the built-in English texts.
    #[serde(default)]
    pub language_file: Option<PathBuf>,
    #[serde(default = "default_language")]
    pub language: String,
}

/// What is remembered about a name, and for how long.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    /// A paid name stays paid: hours are fine.
    pub paid_seconds: u64,
    /// A free name can be bought at any time: minutes.
    pub free_seconds: u64,
    /// Names remembered at most.
    pub capacity: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            paid_seconds: 6 * 3600,
            free_seconds: 600,
            capacity: 100_000,
        }
    }
}

/// The lookups sent to Mojang: a burst, then one every `refill_ms`.
/// Mojang allows about 600 lookups per 10 minutes per address; 300 + one
/// every 2 s stays under it in any 10 minutes (300 + 300).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct RateConfig {
    pub burst: u32,
    pub refill_ms: u64,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            burst: 300,
            refill_ms: 2000,
        }
    }
}

fn default_profile_url() -> String {
    MOJANG_PROFILES.to_owned()
}

const fn default_timeout_ms() -> u64 {
    3000
}

const fn default_failed_auth_seconds() -> u64 {
    600
}

fn default_language() -> String {
    "en".to_owned()
}

/// The texts this plugin sends, from the language file or built in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texts {
    /// A name a paid account owns, after its Mojang authentication failed.
    pub paid_name: String,
    /// The name could not be checked (Mojang slow, down or rate-limited):
    /// the login fails closed.
    pub unavailable: String,
    /// A password name already connected (Velocity's
    /// `already-connected-proxy`, as live).
    pub already_connected: String,
    /// A name no vanilla client sends.
    pub invalid_name: String,
}

impl Default for Texts {
    fn default() -> Self {
        Self {
            paid_name: "This name belongs to a Minecraft account: join with that account."
                .to_owned(),
            unavailable: "Your name cannot be checked right now. Try again in a moment.".to_owned(),
            already_connected: "You are already connected to this proxy!".to_owned(),
            invalid_name: "This name is not valid.".to_owned(),
        }
    }
}

impl Texts {
    /// The `[<language>.identity]` table of `file`; keys it lacks keep the
    /// built-in text.
    ///
    /// # Errors
    /// When the file is unreadable or not TOML, or the table holds a value
    /// that is not text.
    pub fn load(file: &Path, language: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(file)
            .map_err(|error| format!("reading {}: {error}", file.display()))?;
        let root: toml::Table = toml::from_str(&text)
            .map_err(|error| format!("parsing {}: {error}", file.display()))?;
        let mut texts = Self::default();
        let Some(table) = root
            .get(language)
            .and_then(toml::Value::as_table)
            .and_then(|language| language.get("identity"))
            .and_then(toml::Value::as_table)
        else {
            return Ok(texts);
        };
        for (key, value) in table {
            let value = value
                .as_str()
                .ok_or_else(|| format!("{language}.identity.{key} is not text"))?
                .to_owned();
            match key.as_str() {
                "paid-name" => texts.paid_name = value,
                "unavailable" => texts.unavailable = value,
                "already-connected" => texts.already_connected = value,
                "invalid-name" => texts.invalid_name = value,
                other => {
                    return Err(format!(
                        "{language}.identity.{other} is not a text this plugin sends"
                    ));
                }
            }
        }
        Ok(texts)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn defaults_and_unknown_keys() {
        let config: IdentityConfig = toml::from_str("").unwrap();
        assert_eq!(config.profile_url, MOJANG_PROFILES);
        assert_eq!(config.rate.burst, 300);
        assert_eq!(config.language, "en");
        assert!(toml::from_str::<IdentityConfig>("profile = \"x\"").is_err());
    }

    #[test]
    fn texts_come_from_the_language_table() {
        let dir = std::env::temp_dir().join(format!("elyzune-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lang.toml");
        std::fs::write(
            &file,
            "[it.identity]\npaid-name = \"Nome di un account\"\n[en]\nshutdown = \"x\"\n",
        )
        .unwrap();
        let it = Texts::load(&file, "it").unwrap();
        assert_eq!(it.paid_name, "Nome di un account");
        assert_eq!(it.unavailable, Texts::default().unavailable);
        assert_eq!(Texts::load(&file, "en").unwrap(), Texts::default());
        std::fs::write(&file, "[en.identity]\nwhat = \"x\"\n").unwrap();
        assert!(Texts::load(&file, "en").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
