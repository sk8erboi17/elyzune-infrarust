//! `[messages]`: the language file of the texts the proxy shows players
//! (kick reasons, refusals, the MOTD of an unreachable server). See
//! `infrarust_core::texts`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagesConfig {
    /// The language file (TOML, one table per language), relative to the
    /// working directory.
    pub file: PathBuf,
    /// The table to read. One language for every player: these texts are
    /// shown before a client says which language it reads.
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_language() -> String {
    "en".to_owned()
}
