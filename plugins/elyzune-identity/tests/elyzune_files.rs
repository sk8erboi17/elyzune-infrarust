//! Elyzune's deployed files for this plugin (`elyzune/` at the repository
//! root) load with the plugin's own loaders, in both languages.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use elyzune_identity::config::{IdentityConfig, MOJANG_PROFILES, Texts};

fn elyzune() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../elyzune")
}

#[test]
fn the_deployed_config_and_texts_load() {
    let path = elyzune().join("plugins/elyzune_identity/config.toml");
    let config: IdentityConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(config.profile_url, MOJANG_PROFILES);
    // Mojang's ~600 lookups per 10 minutes: burst + refills never exceed it.
    let refills = 600_000 / config.rate.refill_ms;
    assert!(u64::from(config.rate.burst) + refills <= 600);
    let file = elyzune().join(config.language_file.as_ref().expect("a language file"));
    for language in ["en", "it"] {
        let texts = Texts::load(&file, language).unwrap_or_else(|e| panic!("{language}: {e}"));
        assert_ne!(texts, Texts::default(), "{language} has its own texts");
    }
    let italian = Texts::load(&file, "it").unwrap();
    assert_eq!(
        italian.already_connected,
        "Sei già connesso a questo proxy!"
    );
}
