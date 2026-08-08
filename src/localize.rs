// Fluent i18n plumbing (cosmic-greeter's / libcosmic's pattern).
//
// The catalogues under `i18n/<locale>/cosmic_bing_wallpaper.ftl` are embedded
// into the binary by `rust-embed`, so the applet needs no data files at
// runtime. Every user-visible string goes through the [`fl!`] macro, which
// resolves ids against `i18n/en/…` **at compile time** — a typo or a missing
// id is a build error, not a runtime `???`.

use i18n_embed::fluent::{FluentLanguageLoader, fluent_language_loader};
use i18n_embed::{DefaultLocalizer, LanguageLoader, Localizer};
use rust_embed::RustEmbed;
use std::sync::{LazyLock, OnceLock};

#[derive(RustEmbed)]
#[folder = "i18n/"]
pub struct Localizations;

/// The one loader every `fl!` goes through.
///
/// Creating it loads **only the fallback language (`en`)**; the user's own
/// languages are layered on top by [`localize`], which `main` calls once at
/// startup and tests never call. That is what pins `cargo test` to English on
/// a contributor's machine whatever their `LANG` says — see
/// [`tests::loader_is_pinned_to_english`].
pub static LANGUAGE_LOADER: LazyLock<FluentLanguageLoader> = LazyLock::new(|| {
    let loader: FluentLanguageLoader = fluent_language_loader!();
    loader
        .load_fallback_language(&Localizations)
        .expect("i18n/en/cosmic_bing_wallpaper.ftl must be embedded and parse");
    // Fluent wraps every placeable in bidi isolate marks (U+2068/U+2069) by
    // default. They are invisible in a terminal but they *are* in the string,
    // which breaks both plain-text assertions and iced's text measurement for
    // the LTR strings we actually ship. cosmic-greeter and cosmic-settings
    // turn them off for the same reason.
    loader.set_use_isolating(false);
    loader
});

/// Look up a message from the Fluent catalogue.
///
/// `fl!("id")` or `fl!("id", name = value, …)`. Thin wrapper over
/// `i18n_embed_fl::fl!` that fills in the loader; unlike libcosmic's copy it
/// does **not** call [`localize`], so the language selection stays an explicit
/// startup step (and tests stay on `en`).
#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::localize::LANGUAGE_LOADER, $message_id)
    }};
    ($message_id:literal, $($args:tt)*) => {{
        i18n_embed_fl::fl!($crate::localize::LANGUAGE_LOADER, $message_id, $($args)*)
    }};
}

/// Layer the desktop's requested languages onto the `en` fallback.
///
/// Call once from `main`. Idempotent, and deliberately *not* called from
/// library code or tests: `DesktopLanguageRequester` reads the ambient
/// `LANG`/`LC_MESSAGES`, which would make the English string assertions in
/// this crate's tests fail for anyone with a non-English desktop.
pub fn localize() {
    static INITIALIZED: OnceLock<()> = OnceLock::new();
    INITIALIZED.get_or_init(|| {
        let localizer = DefaultLocalizer::new(&*LANGUAGE_LOADER, &Localizations);
        let requested = i18n_embed::DesktopLanguageRequester::requested_languages();
        if let Err(error) = localizer.select(&requested) {
            tracing::warn!(%error, "falling back to English: could not load desktop languages");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_catalogue_is_embedded_and_resolves() {
        assert!(
            Localizations::get("en/cosmic_bing_wallpaper.ftl").is_some(),
            "the fluent domain must match the crate name"
        );
        assert_eq!(fl!("status-up-to-date"), "Up to date");
    }

    #[test]
    fn loader_is_pinned_to_english() {
        // Nothing in the test binary may call `localize()`; if something did,
        // a translated machine would fail every literal-English assertion in
        // `view.rs` instead of failing here with a readable message.
        assert_eq!(
            LANGUAGE_LOADER.current_languages(),
            vec![LANGUAGE_LOADER.fallback_language().clone()],
            "tests must run against the `en` fallback only"
        );
    }

    #[test]
    fn placeables_are_substituted_without_bidi_isolates() {
        let updated = fl!("status-updated-on", date = "Aug 5", time = "07:05");
        assert_eq!(updated, "Updated Aug 5 at 07:05");
        assert!(
            !updated.contains(['\u{2068}', '\u{2069}']),
            "bidi isolate marks leaked into the rendered string"
        );
    }
}
