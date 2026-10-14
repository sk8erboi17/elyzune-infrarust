//! Elyzune's language file (`elyzune/lang.toml`) is read by the proxy's
//! texts loader in both languages, with only keys the proxy knows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use infrarust_core::texts::{self, Text};

#[test]
fn both_languages_load_and_keep_velocitys_live_texts() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../elyzune/lang.toml");
    let english = texts::read(&file, "en").unwrap_or_else(|e| panic!("{e}"));
    let italian = texts::read(&file, "it").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        english.get(Text::RateLimited.key()).map(String::as_str),
        Some("You are logging in too fast, try again later.")
    );
    assert_eq!(
        italian.get(Text::Shutdown.key()).map(String::as_str),
        Some("Il server è in fase di arresto.")
    );
    assert_eq!(english.len(), italian.len(), "the same keys in both");
}
