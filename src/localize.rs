// Fluent i18n plumbing (cosmic-greeter's / libcosmic's pattern).
//
// The catalogues under `i18n/<locale>/cosmic_bing_wallpaper.ftl` are embedded
// into the binary by `rust-embed` (`debug-embed` is on, so debug builds embed
// them too instead of reading the source tree at runtime), so the applet needs
// no data files at runtime. Every user-visible string goes through the [`fl!`]
// macro, which resolves ids against `i18n/en/…` **at compile time** — a typo or
// a missing id is a build error, not a runtime `???`.

use i18n_embed::fluent::{FluentLanguageLoader, fluent_language_loader};
use i18n_embed::unic_langid::LanguageIdentifier;
use i18n_embed::{DefaultLocalizer, LanguageLoader, Localizer};
use rust_embed::RustEmbed;
use std::sync::LazyLock;

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

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
    disable_bidi_isolation(&loader);
    loader
});

/// Turn Fluent's bidi isolate marks (U+2068/U+2069 around every placeable)
/// off for `loader`.
///
/// They are invisible in a terminal but they *are* in the string, which breaks
/// both plain-text assertions and iced's text measurement for the strings we
/// ship. cosmic-greeter and cosmic-settings turn them off for the same reason.
///
/// Tradeoff, accepted knowingly: the catalogues include RTL locales (`ar`,
/// `fa`, `he`) whose `status-updated-on` embeds LTR content ("Aug 5",
/// "09:12") in an RTL sentence, and isolates are exactly what keeps that
/// ordering unambiguous. Since only that one message mixes directions, and
/// since the alternative is stray control characters in every string the
/// applet renders and asserts on, the flag stays off crate-wide.
///
/// **This must be re-applied after every language load.** The setting mutates
/// the bundles that exist *right now* (i18n-embed: "no effect if
/// `load_languages` has not been called first"), and `load_languages` swaps in
/// brand-new bundles built by `FluentBundle::new_concurrent`, which hardcodes
/// `use_isolating: true`.
fn disable_bidi_isolation(loader: &FluentLanguageLoader) {
    loader.set_use_isolating(false);
}

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

/// Layer `requested` onto `loader`'s `en` fallback, keeping the bidi-isolation
/// setting intact (see [`disable_bidi_isolation`] — `select` rebuilds the
/// bundles, resetting it).
///
/// Split out of [`localize`] so the exact production sequence can be exercised
/// against a *local* loader in tests without disturbing the global `en` pin.
fn select_languages(loader: &FluentLanguageLoader, requested: &[LanguageIdentifier]) {
    let localizer = DefaultLocalizer::new(loader, &Localizations);
    if let Err(error) = localizer.select(requested) {
        tracing::warn!("falling back to English: could not load desktop languages ({error})");
    }
    disable_bidi_isolation(loader);
}

/// Layer the desktop's requested languages onto the `en` fallback.
///
/// Call once from `main`, and deliberately *not* from library code or tests:
/// `DesktopLanguageRequester` reads the ambient `LANG`/`LC_MESSAGES`, which
/// would make the English string assertions in this crate's tests fail for
/// anyone with a non-English desktop.
pub fn localize() {
    let requested = i18n_embed::DesktopLanguageRequester::requested_languages();
    select_languages(&LANGUAGE_LOADER, &requested);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// Every locale COSMIC itself ships (cosmic-greeter's `i18n/` set), `en`
    /// included. Editing this is a deliberate act: a locale silently dropped
    /// from the embed folder is a shipped regression, not a refactor — and a
    /// bare count would not notice one locale being *swapped* for another.
    const COSMIC_LOCALES: [&str; 73] = [
        "af", "ar", "be", "bg", "bn", "ca", "cs", "da", "de", "el", "en", "en-GB", "eo", "es",
        "es-419", "es-MX", "et", "eu", "fa", "fi", "fr", "fy", "ga", "gd", "gu", "he", "hi", "hr",
        "hu", "id", "ie", "is", "it", "ja", "jv", "ka", "kab", "kk", "kmr", "kn", "ko", "li", "lo",
        "lt", "ml", "ms", "nb-NO", "nl", "nn", "oc", "pa", "pl", "pt", "pt-BR", "ro", "ru", "sat",
        "sk", "sl", "sr", "sr-Cyrl", "sr-Latn", "sv", "ta", "th", "ti", "tr", "uk", "uz", "vi",
        "yue-Hant", "zh-CN", "zh-TW",
    ];

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

    /// Message id -> the set of placeables (`{ … }` contents) its value uses.
    ///
    /// A source scan rather than a fluent-syntax dependency: fluent's own
    /// loader exposes message *ids* but not their placeables, and the
    /// catalogues are flat `id = value` lines. Panics on a duplicated id —
    /// fluent silently keeps the first definition and only logs the override,
    /// so a machine translation that emitted a message twice would otherwise
    /// pass every guard here.
    fn message_placeables(locale: &str, ftl: &str) -> BTreeMap<String, BTreeSet<String>> {
        let mut messages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in ftl.lines() {
            // Indented lines continue the value of the message above them.
            if line.starts_with([' ', '\t']) {
                if let Some(id) = &current {
                    let placeables = messages.get_mut(id).expect("current id was inserted");
                    collect_placeables(line, placeables);
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
            let mut placeables = BTreeSet::new();
            collect_placeables(value, &mut placeables);
            assert!(
                messages.insert(id.clone(), placeables).is_none(),
                "`{locale}` defines `{id}` twice (fluent keeps the first and only logs)"
            );
            current = Some(id);
        }
        messages
    }

    /// Every `{ … }` placeable in `text`, normalized by trimming. Covers both
    /// variable references (`{ $time }`) and message/term references
    /// (`{ shuffle }`), which a machine translation can invent out of nowhere
    /// and which render as `???` at runtime.
    fn collect_placeables(text: &str, out: &mut BTreeSet<String>) {
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else {
                // An unbalanced brace is a fluent syntax error; the id-set
                // guard catches it (the message fails to parse at all).
                return;
            };
            out.insert(after[..end].trim().to_owned());
            rest = &after[end + 1..];
        }
    }

    /// A loader holding exactly `locale`'s catalogue (plus the `en` fallback
    /// fluent always appends).
    fn loader_for(locale: &LanguageIdentifier) -> FluentLanguageLoader {
        let loader: FluentLanguageLoader = fluent_language_loader!();
        loader
            .load_languages(&Localizations, std::slice::from_ref(locale))
            .unwrap_or_else(|error| panic!("`{locale}` failed to load: {error}"));
        disable_bidi_isolation(&loader);
        loader
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
    fn selecting_a_language_loads_it_and_keeps_isolation_off() {
        // The production path (`localize()`), run against a *local* loader so
        // the global `en` pin above survives. `select` rebuilds every bundle
        // with fluent's `use_isolating: true` default, so this is the one
        // place the re-application in `select_languages` can be caught.
        let loader: FluentLanguageLoader = fluent_language_loader!();
        loader
            .load_fallback_language(&Localizations)
            .expect("fallback must load");
        disable_bidi_isolation(&loader);

        select_languages(&loader, &["uk".parse().unwrap()]);

        assert_eq!(
            loader.get("status-up-to-date"),
            "Актуально",
            "`DefaultLocalizer::select` did not load the requested catalogue"
        );
        let updated = loader.get_args("status-updated-today", [("time", "09:12")].into());
        assert_eq!(updated, "Оновлено сьогодні о 09:12");
        assert!(
            !updated.contains(['\u{2068}', '\u{2069}']),
            "bidi isolates came back after `select` rebuilt the bundles"
        );
    }

    #[test]
    fn every_cosmic_locale_ships_a_catalogue() {
        let shipped: BTreeSet<String> = catalogues().into_keys().collect();
        let expected: BTreeSet<String> = COSMIC_LOCALES.iter().map(|&l| l.to_owned()).collect();
        assert_eq!(
            shipped, expected,
            "embedded locales differ from the COSMIC set"
        );
        for locale in &shipped {
            locale
                .parse::<LanguageIdentifier>()
                .unwrap_or_else(|error| panic!("`i18n/{locale}` is not a language tag: {error}"));
        }
    }

    #[test]
    fn every_locale_defines_every_english_message() {
        let english_locale: LanguageIdentifier = "en".parse().unwrap();
        let english = parsed_ids(&loader_for(&english_locale), &english_locale);
        assert!(!english.is_empty(), "the English catalogue must define ids");

        for locale in COSMIC_LOCALES {
            let language: LanguageIdentifier = locale.parse().expect("checked above");
            // Unparsable entries are dropped by fluent with only a log line,
            // so a missing id here means either "not translated" or "broken
            // fluent syntax" — both are release blockers.
            assert_eq!(
                parsed_ids(&loader_for(&language), &language),
                english,
                "`{locale}` does not define exactly the English message ids"
            );
        }
    }

    #[test]
    fn every_locale_preserves_the_english_placeables() {
        let catalogues = catalogues();
        let english = message_placeables("en", &catalogues["en"]);

        for (locale, source) in &catalogues {
            for (id, placeables) in message_placeables(locale, source) {
                let expected = english
                    .get(&id)
                    .unwrap_or_else(|| panic!("`{locale}` defines unknown message `{id}`"));
                // A renamed or dropped placeable still "resolves" — it just
                // renders a sentence with a hole in it, which is the most
                // common way a machine translation goes wrong. An *invented*
                // one (a message reference) renders as `???`.
                assert_eq!(
                    &placeables, expected,
                    "`{locale}` changed the placeables of `{id}`"
                );
            }
        }
    }

    #[test]
    fn every_message_id_is_referenced_by_the_ui() {
        // Guards against orphaned strings — notably the `tooltip-*` ids,
        // whose only trace in the binary is one `fl!` argument at the
        // `popup_tooltip`/`applet_tooltip` call site. Dropping a tooltip
        // would otherwise leave the id shipped in all 73 catalogues and
        // nothing would notice. (The reverse direction — an id used but not
        // defined — is a compile error, `fl!` resolves at build time.)
        //
        // Each source is cut at its own `#[cfg(test)]` module: a `fl!` that
        // only appears in an assertion (the dropdown-label and
        // `display_title` tests in `view.rs` are full of them) must not count
        // as "rendered by the UI".
        const SOURCES: [&str; 2] = [include_str!("app.rs"), include_str!("view.rs")];
        let production: Vec<&str> = SOURCES
            .iter()
            .map(|source| {
                source
                    .split_once("\n#[cfg(test)]\n")
                    .map_or(*source, |(before, _)| before)
            })
            .collect();
        let english_locale: LanguageIdentifier = "en".parse().unwrap();
        for id in parsed_ids(&loader_for(&english_locale), &english_locale) {
            let call = format!("fl!(\"{id}\"");
            assert!(
                production.iter().any(|source| source.contains(&call)),
                "`{id}` is defined in every catalogue but never rendered — \
                 delete it from `i18n/*/` or wire it up"
            );
        }
    }
}
