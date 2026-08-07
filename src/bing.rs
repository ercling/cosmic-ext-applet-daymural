// Bing HPImageArchive API: response types, parsing, and the pure
// URL/filename builders shared with the catalogue rebuild path.
//
// Reference behavior: examples/bing-wallpaper-gnome-extension
// (`utils.js` getImageTitle/toFilename, `extension.js:908-910` for the
// full-width-paren copyright handling that `utils.js:207` misses).

use serde::Deserialize;

/// Base URL images are fetched from (`https://www.bing.com<urlbase>_<res>.jpg`).
pub const BING_BASE_URL: &str = "https://www.bing.com";

/// Prefix every Bing `urlbase` carries; stripped for filenames and
/// re-added when rebuilding a catalogue from a folder scan.
const URLBASE_PREFIX: &str = "/th?id=OHR.";

/// Hardcoded resolution for downloads (v1 scope decision).
pub const RESOLUTION: &str = "UHD";

/// Top-level HPImageArchive response. Only `images` is used.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageArchive {
    pub images: Vec<BingImage>,
}

/// One image entry as Bing returns it.
///
/// Deliberately omitted: `title` (the literal string `"Info"` — useless;
/// the display title is derived from `copyright`) and `wp`/`url`/`hsh`/…
/// (unused; resolution is hardcoded to UHD).
#[derive(Debug, Clone, Deserialize)]
pub struct BingImage {
    pub urlbase: String,
    pub startdate: String,
    pub fullstartdate: String,
    pub copyright: String,
    pub copyrightlink: String,
}

/// Parse an HPImageArchive JSON response. Malformed/empty input is an
/// error, never a panic.
pub fn parse_image_list(json: &str) -> Result<ImageArchive, serde_json::Error> {
    serde_json::from_str(json)
}

/// Split Bing's `copyright` string into (display title, copyright notice).
///
/// `"Nyhavn Canal, Copenhagen (© emicristea/Getty Images)"` →
/// `("Nyhavn Canal, Copenhagen", "© emicristea/Getty Images")`.
///
/// Matches the reference's semantics (`extension.js:908-910`): every
/// parenthesised group — ASCII `()` or Japanese full-width `（）` — plus its
/// surrounding whitespace is stripped from the title; the copyright is the
/// content of the first group (parens removed, literal `**` dropped).
/// Unlike the reference, a copyright string with *no* parens is handled
/// gracefully: the whole string becomes the title and the notice is empty
/// (the reference crashes on `.match(...)[1]` there).
pub fn split_copyright(raw: &str) -> (String, String) {
    let is_open = |c: char| c == '(' || c == '（';
    let is_close = |c: char| c == ')' || c == '）';

    let mut title = String::with_capacity(raw.len());
    let mut notice: Option<String> = None;

    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if is_open(c) {
            // Capture until the matching close; an unclosed group is kept
            // verbatim in the title (the reference's lazy regex also
            // refuses to match it).
            let mut group = String::new();
            let mut closed = false;
            for g in chars.by_ref() {
                if is_close(g) {
                    closed = true;
                    break;
                }
                group.push(g);
            }
            if closed {
                if notice.is_none() {
                    notice = Some(group.replace("**", ""));
                }
                // Strip whitespace adjacent to the removed group
                // (regex `\s*[（(].*?[)）]\s*`).
                while title.ends_with(char::is_whitespace) {
                    title.pop();
                }
                while chars.peek().is_some_and(|n| n.is_whitespace()) {
                    chars.next();
                }
            } else {
                title.push(c);
                title.push_str(&group);
            }
        } else {
            title.push(c);
        }
    }

    (title.trim().to_owned(), notice.unwrap_or_default())
}

/// Full download URL for an image at the hardcoded UHD resolution:
/// `https://www.bing.com<urlbase>_UHD.jpg&qlt=100`.
pub fn image_url(urlbase: &str) -> String {
    format!("{BING_BASE_URL}{urlbase}_{RESOLUTION}.jpg&qlt=100")
}

/// The `<name>` part of a `urlbase`: last path component minus the
/// `th?id=OHR.` prefix (mirrors the reference's `toFilename`,
/// `utils.js:646-648`).
fn urlbase_name(urlbase: &str) -> &str {
    let last = urlbase.rsplit('/').next().unwrap_or(urlbase);
    last.strip_prefix("th?id=OHR.").unwrap_or(last)
}

/// Download filename compatible with the GNOME reference extension:
/// `<startdate>-<name>_UHD.jpg` (e.g.
/// `20260807-ColorfulCop_ROW6097405388_UHD.jpg`).
pub fn image_filename(startdate: &str, urlbase: &str) -> String {
    format!("{startdate}-{}_{RESOLUTION}.jpg", urlbase_name(urlbase))
}

/// Inverse of [`image_filename`] for catalogue rebuilds: parse
/// `"<8 digits>-<name>_<res>.jpg"` back into `(startdate, urlbase)` with
/// `urlbase = "/th?id=OHR." + name`. Any `_<res>` suffix is accepted
/// (not just `_UHD`) so folders written by the reference extension at any
/// resolution setting migrate cleanly. Returns `None` for filenames that
/// don't match the pattern.
pub fn parse_filename(filename: &str) -> Option<(String, String)> {
    let stem = filename.strip_suffix(".jpg")?;
    let (startdate, rest) = stem.split_at_checked(8)?;
    if !startdate.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = rest.strip_prefix('-')?;
    let (name, res) = rest.rsplit_once('_')?;
    if name.is_empty() || res.is_empty() {
        return None;
    }
    Some((startdate.to_owned(), format!("{URLBASE_PREFIX}{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/hpimagearchive.json");

    #[test]
    fn fixture_parses_with_real_fields() {
        let archive = parse_image_list(FIXTURE).expect("checked-in fixture must parse");
        assert_eq!(archive.images.len(), 8);

        let first = &archive.images[0];
        assert_eq!(first.urlbase, "/th?id=OHR.ColorfulCop_ROW6097405388");
        assert_eq!(first.startdate, "20260807");
        assert_eq!(first.fullstartdate, "202608070700");
        assert!(first.copyrightlink.starts_with("https://"));

        // Every entry has the fields the catalogue needs.
        for img in &archive.images {
            assert!(img.urlbase.starts_with("/th?id=OHR."), "{}", img.urlbase);
            assert_eq!(img.startdate.len(), 8);
            assert_eq!(img.fullstartdate.len(), 12);
            assert!(!img.copyright.is_empty());
        }
    }

    #[test]
    fn derived_title_is_a_real_title_not_info() {
        let archive = parse_image_list(FIXTURE).unwrap();
        let (title, copyright) = split_copyright(&archive.images[0].copyright);
        assert_eq!(
            title,
            "Colourful homes line Nyhavn Canal, Copenhagen, Denmark"
        );
        assert_eq!(copyright, "© emicristea/Getty Images");
        assert_ne!(title, "Info");

        for img in &archive.images {
            let (title, _) = split_copyright(&img.copyright);
            assert!(!title.is_empty());
            assert_ne!(title, "Info");
            assert!(!title.contains('('), "paren leaked into title: {title}");
        }
    }

    #[test]
    fn split_copyright_handles_full_width_parens() {
        let (title, copyright) = split_copyright("東京タワーの夜景（© Foo/Getty Images）");
        assert_eq!(title, "東京タワーの夜景");
        assert_eq!(copyright, "© Foo/Getty Images");
    }

    #[test]
    fn split_copyright_without_parens_does_not_crash() {
        // The reference crashes here (`.match(...)[1]` on null); we must not.
        let (title, copyright) = split_copyright("A caption with no copyright notice");
        assert_eq!(title, "A caption with no copyright notice");
        assert_eq!(copyright, "");
    }

    #[test]
    fn split_copyright_edge_cases() {
        // Empty input.
        assert_eq!(split_copyright(""), (String::new(), String::new()));
        // Group in the middle: surrounding whitespace collapses (reference regex semantics).
        assert_eq!(
            split_copyright("Foo (© Bar) baz"),
            ("Foobaz".to_owned(), "© Bar".to_owned())
        );
        // Only the first group feeds the copyright; all groups leave the title.
        assert_eq!(
            split_copyright("Foo (© Bar) (extra)"),
            ("Foo".to_owned(), "© Bar".to_owned())
        );
        // `**` markers are dropped like the reference does.
        assert_eq!(split_copyright("Foo (© Bar**)").1, "© Bar");
        // Unclosed paren stays in the title, copyright empty.
        assert_eq!(
            split_copyright("Foo (unclosed"),
            ("Foo (unclosed".to_owned(), String::new())
        );
    }

    #[test]
    fn image_url_matches_known_good_value() {
        assert_eq!(
            image_url("/th?id=OHR.ColorfulCop_ROW6097405388"),
            "https://www.bing.com/th?id=OHR.ColorfulCop_ROW6097405388_UHD.jpg&qlt=100"
        );
    }

    #[test]
    fn image_filename_matches_reference_naming() {
        assert_eq!(
            image_filename("20260807", "/th?id=OHR.ColorfulCop_ROW6097405388"),
            "20260807-ColorfulCop_ROW6097405388_UHD.jpg"
        );
    }

    #[test]
    fn parse_filename_roundtrips_image_filename() {
        let startdate = "20260807";
        let urlbase = "/th?id=OHR.ColorfulCop_ROW6097405388";
        let filename = image_filename(startdate, urlbase);
        assert_eq!(
            parse_filename(&filename),
            Some((startdate.to_owned(), urlbase.to_owned()))
        );

        // Roundtrip across the whole fixture.
        for img in parse_image_list(FIXTURE).unwrap().images {
            let filename = image_filename(&img.startdate, &img.urlbase);
            assert_eq!(
                parse_filename(&filename),
                Some((img.startdate.clone(), img.urlbase.clone()))
            );
        }
    }

    #[test]
    fn parse_filename_accepts_non_uhd_suffixes() {
        assert_eq!(
            parse_filename("20240101-FezMorocco_ROW6564333571_1920x1080.jpg"),
            Some((
                "20240101".to_owned(),
                "/th?id=OHR.FezMorocco_ROW6564333571".to_owned()
            ))
        );
        assert_eq!(
            parse_filename("20240101-Foo_1080p.jpg"),
            Some(("20240101".to_owned(), "/th?id=OHR.Foo".to_owned()))
        );
    }

    #[test]
    fn parse_filename_rejects_non_matching_names() {
        assert_eq!(parse_filename(""), None);
        assert_eq!(parse_filename("catalogue.json"), None);
        assert_eq!(parse_filename("20240101-Foo_UHD.png"), None); // wrong extension
        assert_eq!(parse_filename("2024010-Foo_UHD.jpg"), None); // 7 digits then '-' misaligned
        assert_eq!(parse_filename("2024010a-Foo_UHD.jpg"), None); // non-digit in date
        assert_eq!(parse_filename("20240101Foo_UHD.jpg"), None); // missing '-'
        assert_eq!(parse_filename("20240101-FooUHD.jpg"), None); // no '_<res>' suffix
        assert_eq!(parse_filename("20240101-_UHD.jpg"), None); // empty name
        assert_eq!(parse_filename("20240101-Foo_.jpg"), None); // empty resolution
        assert_eq!(parse_filename("短い-Foo_UHD.jpg"), None); // non-ASCII where digits belong
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse_image_list("").is_err());
        assert!(parse_image_list("not json").is_err());
        assert!(parse_image_list("{}").is_err()); // missing `images`
        assert!(parse_image_list("{\"images\": [{}]}").is_err()); // missing fields
        assert!(parse_image_list("{\"images\": 42}").is_err()); // wrong type
        assert!(parse_image_list("{\"images\": [").is_err()); // truncated
    }
}
