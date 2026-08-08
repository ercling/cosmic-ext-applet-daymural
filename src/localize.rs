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
    use i18n_embed::unic_langid::LanguageIdentifier;
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    /// Every locale COSMIC itself ships (cosmic-greeter's `i18n/` set), `en`
    /// included. Bumping this is a deliberate act: a locale silently dropped
    /// from the embed folder is a shipped regression, not a refactor.
    const COSMIC_LOCALES: usize = 73;

    /// `locale -> raw FTL source`, straight out of the embedded assets.
    fn catalogues() -> BTreeMap<String, String> {
        let file_name = LANGUAGE_LOADER.language_file_name();
        Localizations::iter()
            .map(|path| {
                let (locale, name) = path
                    .split_once('/')
                    .unwrap_or_else(|| panic!("`i18n/{path}` is not inside a locale directory"));
                assert_eq!(
                    name, file_name,
                    "`i18n/{path}` does not match the fluent domain"
                );
                let file = Localizations::get(&path).expect("embedded path must resolve");
                let source = String::from_utf8(file.data.to_vec())
                    .unwrap_or_else(|error| panic!("`i18n/{path}` is not UTF-8: {error}"));
                (locale.to_owned(), source)
            })
            .collect()
    }

    /// Message id -> the set of `$variable` names its value references.
    ///
    /// A hand-rolled scan rather than a fluent-syntax dependency: the
    /// catalogues are flat `id = value` lines, and the only thing this needs
    /// to be exact about is which placeables a translator kept.
    fn message_variables(ftl: &str) -> BTreeMap<String, BTreeSet<String>> {
        let mut messages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in ftl.lines() {
            // Indented lines continue the value of the message above them.
            if line.starts_with([' ', '\t']) {
                if let Some(id) = &current {
                    let variables = messages.get_mut(id).expect("current id was inserted");
                    collect_variables(line, variables);
                }
                continue;
            }
            current = None;
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((id, value)) = line.split_once('=') else {
                continue;
            };
            let id = id.trim().to_owned();
            let mut variables = BTreeSet::new();
            collect_variables(value, &mut variables);
            messages.insert(id.clone(), variables);
            current = Some(id);
        }
        messages
    }

    fn collect_variables(text: &str, out: &mut BTreeSet<String>) {
        let mut rest = text;
        while let Some(start) = rest.find('$') {
            let after = &rest[start + 1..];
            let end = after
                .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                .unwrap_or(after.len());
            if end > 0 {
                out.insert(after[..end].to_owned());
            }
            rest = &after[end..];
        }
    }

    /// The ids fluent actually parsed out of `locale`'s own catalogue —
    /// deliberately not `has()`, which would happily answer from the `en`
    /// fallback and hide both missing keys and unparsable entries.
    fn parsed_ids(loader: &FluentLanguageLoader, locale: &LanguageIdentifier) -> BTreeSet<String> {
        loader.with_message_iter(locale, |messages| {
            messages.map(|message| message.id.name.to_owned()).collect()
        })
    }

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

    #[test]
    fn every_cosmic_locale_ships_a_catalogue() {
        let catalogues = catalogues();
        assert_eq!(
            catalogues.len(),
            COSMIC_LOCALES,
            "embedded locales: {:?}",
            catalogues.keys().collect::<Vec<_>>()
        );
        assert!(
            catalogues.contains_key("en"),
            "the fallback must be shipped"
        );
        for locale in catalogues.keys() {
            locale
                .parse::<LanguageIdentifier>()
                .unwrap_or_else(|error| panic!("`i18n/{locale}` is not a language tag: {error}"));
        }
    }

    #[test]
    fn every_locale_defines_and_renders_every_english_message() {
        let catalogues = catalogues();
        let english: BTreeSet<String> = message_variables(&catalogues["en"]).into_keys().collect();

        for locale in catalogues.keys() {
            let language: LanguageIdentifier = locale.parse().expect("checked above");
            let loader: FluentLanguageLoader = fluent_language_loader!();
            loader
                .load_languages(&Localizations, std::slice::from_ref(&language))
                .unwrap_or_else(|error| panic!("`{locale}` failed to load: {error}"));
            loader.set_use_isolating(false);

            // Unparsable entries are dropped by fluent with only a log line,
            // so a missing id here means either "not translated" or "broken
            // fluent syntax" — both are release blockers.
            assert_eq!(
                parsed_ids(&loader, &language),
                english,
                "`{locale}` does not define exactly the English message ids"
            );

            for id in ["status-updated-today", "status-updated-on"] {
                let args = HashMap::from([("date", "Aug 5"), ("time", "07:05")]);
                let rendered = loader.get_args(id, args);
                assert!(
                    rendered.contains("07:05"),
                    "`{locale}` renders `{id}` without its time: {rendered}"
                );
            }
        }
    }

    #[test]
    fn every_locale_preserves_the_english_placeables() {
        let catalogues = catalogues();
        let english = message_variables(&catalogues["en"]);

        for (locale, source) in &catalogues {
            for (id, variables) in message_variables(source) {
                let expected = english
                    .get(&id)
                    .unwrap_or_else(|| panic!("`{locale}` defines unknown message `{id}`"));
                // A renamed or dropped placeable still "resolves" — it just
                // renders a sentence with a hole in it, which is the most
                // common way a machine translation goes wrong.
                assert_eq!(
                    &variables, expected,
                    "`{locale}` changed the placeables of `{id}`"
                );
            }
        }
    }
}
