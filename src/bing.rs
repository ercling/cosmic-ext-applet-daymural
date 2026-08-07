// Bing HPImageArchive API: response types, parsing, and the pure
// URL/filename builders shared with the catalogue rebuild path.
//
// Reference behavior: examples/bing-wallpaper-gnome-extension
// (`utils.js` getImageTitle/toFilename, `extension.js:908-910` for the
// full-width-paren copyright handling that `utils.js:207` misses).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Base URL images are fetched from (`https://www.bing.com<urlbase>_<res>.jpg`).
pub const BING_BASE_URL: &str = "https://www.bing.com";

/// User-Agent sent with every request (Bing serves generic UAs fine; this
/// just identifies us honestly).
pub const USER_AGENT: &str = concat!("cosmic-bing-wallpaper/", env!("CARGO_PKG_VERSION"));

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

/// Parse an HPImageArchive JSON response and validate the field that feeds
/// filesystem paths: `startdate` must be exactly 8 ASCII digits — it is
/// embedded verbatim in the download filename, so a hostile value like
/// `../../.config/x` must never escape the download dir. Malformed input
/// is an error, never a panic.
pub fn parse_image_list(json: &str) -> Result<ImageArchive, serde_json::Error> {
    use serde::de::Error as _;

    let archive: ImageArchive = serde_json::from_str(json)?;
    for image in &archive.images {
        if image.startdate.len() != 8 || !image.startdate.bytes().all(|b| b.is_ascii_digit()) {
            return Err(serde_json::Error::custom(format!(
                "invalid startdate {:?} (expected 8 ASCII digits)",
                image.startdate
            )));
        }
    }
    Ok(archive)
}

/// Everything that can go wrong talking to Bing: transport failures,
/// non-2xx responses, unparseable bodies, and local filesystem errors
/// while persisting a download.
#[derive(Debug)]
pub enum FetchError {
    /// reqwest-level failure (DNS, TLS, timeout, invalid URL, …).
    Http(reqwest::Error),
    /// The server answered, but not with a success status.
    Status(reqwest::StatusCode),
    /// The body was not valid HPImageArchive JSON.
    Parse(serde_json::Error),
    /// The list endpoint succeeded but carried no images. An error so the
    /// caller backs off for an hour instead of rescheduling off an empty
    /// catalogue (whose cold-start delay would tight-loop against Bing).
    EmptyList,
    /// A downloaded 2xx body was not a JPEG (captive portal, error page).
    /// Persisting it would poison the catalogue permanently, since
    /// downloads skip files that already exist.
    NotJpeg,
    /// Local I/O failure writing the downloaded file.
    Io(io::Error),
}

impl FetchError {
    /// Whether this is a *local* failure (disk), as opposed to trouble
    /// reaching or understanding Bing — the popup footer distinguishes the
    /// two.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Io(_))
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP request failed: {e}"),
            Self::Status(s) => write!(f, "Bing returned HTTP {s}"),
            Self::Parse(e) => write!(f, "failed to parse Bing response: {e}"),
            Self::EmptyList => write!(f, "Bing returned an empty image list"),
            Self::NotJpeg => write!(f, "downloaded body is not a JPEG image"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl From<io::Error> for FetchError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// HPImageArchive endpoint URL for the latest `n` images. Empty `mkt`
/// means "auto" — must stay in sync with the checked-in fixture's URL.
/// `base_url` is [`BING_BASE_URL`] in production; injected so tests can
/// point at a loopback mock server.
pub fn api_url(base_url: &str, n: u8) -> String {
    format!("{base_url}/HPImageArchive.aspx?format=js&idx=0&n={n}&mbl=1&mkt=")
}

/// Shared HTTP client with our User-Agent and sane timeouts. Build once
/// and reuse (connection pooling).
pub fn http_client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(FetchError::Http)
}

/// Fetch and parse the image-of-the-day list for the latest `n` images
/// from `base_url` ([`BING_BASE_URL`] in production). A successful but
/// empty list is [`FetchError::EmptyList`].
pub async fn fetch_image_list(
    client: &reqwest::Client,
    base_url: &str,
    n: u8,
) -> Result<ImageArchive, FetchError> {
    let resp = client
        .get(api_url(base_url, n))
        .send()
        .await
        .map_err(FetchError::Http)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FetchError::Status(status));
    }
    let body = resp.text().await.map_err(FetchError::Http)?;
    let archive = parse_image_list(&body).map_err(FetchError::Parse)?;
    if archive.images.is_empty() {
        return Err(FetchError::EmptyList);
    }
    Ok(archive)
}

/// Where `image` lands on disk inside the download dir
/// (`<dir>/<startdate>-<name>_UHD.jpg`).
pub fn download_path(dir: &Path, image: &BingImage) -> PathBuf {
    dir.join(image_filename(&image.startdate, &image.urlbase))
}

/// Download `image` at UHD into `dir` (created if missing). Skips the
/// network entirely when the file already exists; otherwise writes to a
/// `.part` sibling and renames, so a crashed download never leaves a
/// torn file behind at the final path. Returns the final path.
pub async fn download_image(
    client: &reqwest::Client,
    base_url: &str,
    image: &BingImage,
    dir: &Path,
) -> Result<PathBuf, FetchError> {
    let dest = download_path(dir, image);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(dir)?;

    let resp = client
        .get(image_url(base_url, &image.urlbase))
        .send()
        .await
        .map_err(FetchError::Http)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FetchError::Status(status));
    }
    let bytes = resp.bytes().await.map_err(FetchError::Http)?;
    // Captive portals and CDN error pages answer 2xx with HTML; a JPEG
    // always starts FF D8. Never persist anything else.
    if !bytes.starts_with(&JPEG_MAGIC) {
        return Err(FetchError::NotJpeg);
    }
    crate::fsutil::write_atomic(&dest, PART_SUFFIX, |part| std::fs::write(part, &bytes))?;
    Ok(dest)
}

/// The two bytes every JPEG stream starts with (SOI marker).
const JPEG_MAGIC: [u8; 2] = [0xFF, 0xD8];

/// Remove orphaned `*.part` files a crash mid-download may have left in
/// `dir` (the pipeline sweeps before downloading anew; downloads are
/// single-flight, so nothing here can be in active use). Best-effort: a
/// missing dir or a failed removal is only logged.
pub fn sweep_part_files(dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "part")
            && path.is_file()
            && let Err(e) = std::fs::remove_file(&path)
        {
            tracing::warn!("failed to remove orphaned {}: {e}", path.display());
        }
    }
}

/// Suffix of the temporary sibling a download is written to before the
/// atomic rename (`sweep_part_files` cleans up orphans carrying it).
const PART_SUFFIX: &str = ".part";

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
/// `<base_url><urlbase>_UHD.jpg&qlt=100`. `base_url` is
/// [`BING_BASE_URL`] in production; injected so tests can point at a
/// loopback mock server.
pub fn image_url(base_url: &str, urlbase: &str) -> String {
    format!("{base_url}{urlbase}_{RESOLUTION}.jpg&qlt=100")
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
    fn split_copyright_nested_parens_behavior_is_locked() {
        // Nested groups are not paired: the capture ends at the *first*
        // closing paren (so the inner open leaks into the notice) and the
        // stray outer close stays in the title. Not pretty, but Bing never
        // nests parens in practice — this test pins the behavior so any
        // change to it is deliberate.
        assert_eq!(
            split_copyright("Foo ((© Bar))"),
            ("Foo)".to_owned(), "(© Bar".to_owned())
        );
    }

    #[test]
    fn image_url_matches_known_good_value() {
        assert_eq!(
            image_url(BING_BASE_URL, "/th?id=OHR.ColorfulCop_ROW6097405388"),
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
    fn api_url_matches_reference_query() {
        assert_eq!(
            api_url(BING_BASE_URL, 8),
            "https://www.bing.com/HPImageArchive.aspx?format=js&idx=0&n=8&mbl=1&mkt="
        );
        assert_eq!(
            api_url(BING_BASE_URL, 3),
            "https://www.bing.com/HPImageArchive.aspx?format=js&idx=0&n=3&mbl=1&mkt="
        );
    }

    fn fixture_image() -> BingImage {
        parse_image_list(FIXTURE).unwrap().images[0].clone()
    }

    /// A base URL nothing listens on: any accidental network attempt fails
    /// fast with a connection error instead of reaching the real Bing.
    const DEAD_BASE: &str = "http://127.0.0.1:9";

    #[tokio::test]
    async fn download_image_skips_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let dest = download_path(dir.path(), &image);
        std::fs::write(&dest, b"pre-existing bytes").unwrap();

        // Dead base URL: if the skip check failed, the request would error
        // out (connection refused) and fail the test — the real network is
        // never touched either way.
        let client = http_client().unwrap();
        let got = download_image(&client, DEAD_BASE, &image, dir.path())
            .await
            .unwrap();

        assert_eq!(got, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), b"pre-existing bytes");
    }

    #[tokio::test]
    async fn fetch_image_list_parses_a_mocked_response() {
        let base = crate::testutil::spawn_mock(|path| {
            if path.starts_with("/HPImageArchive.aspx?format=js&idx=0&n=8&") {
                (200, FIXTURE.as_bytes().to_vec())
            } else {
                (404, Vec::new())
            }
        });
        let archive = fetch_image_list(&http_client().unwrap(), &base, 8)
            .await
            .unwrap();
        assert_eq!(archive.images.len(), 8);
        assert_eq!(
            archive.images[0].urlbase,
            "/th?id=OHR.ColorfulCop_ROW6097405388"
        );
    }

    #[tokio::test]
    async fn fetch_image_list_maps_error_responses() {
        let base = crate::testutil::spawn_mock(|_| (500, b"oops".to_vec()));
        let err = fetch_image_list(&http_client().unwrap(), &base, 8)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Status(s) if s.as_u16() == 500));

        let base = crate::testutil::spawn_mock(|_| (200, b"not json".to_vec()));
        let err = fetch_image_list(&http_client().unwrap(), &base, 8)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Parse(_)));
    }

    #[tokio::test]
    async fn fetch_image_list_treats_an_empty_list_as_an_error() {
        // {"images":[]} parses fine but must not count as success — the
        // caller would otherwise reschedule off an empty catalogue (5 s
        // cold-start delay → tight loop against Bing).
        let base = crate::testutil::spawn_mock(|_| (200, br#"{"images":[]}"#.to_vec()));
        let err = fetch_image_list(&http_client().unwrap(), &base, 8)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::EmptyList));
    }

    #[tokio::test]
    async fn download_image_fetches_and_writes_a_jpeg() {
        let jpeg = crate::testutil::tiny_jpeg(32, 18);
        let expected = jpeg.clone();
        let base = crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/th?id=OHR.") {
                (200, jpeg.clone())
            } else {
                (404, Vec::new())
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let client = http_client().unwrap();
        let dest = download_image(&client, &base, &image, dir.path())
            .await
            .unwrap();

        assert_eq!(dest, download_path(dir.path(), &image));
        assert_eq!(std::fs::read(&dest).unwrap(), expected);
        assert!(!crate::fsutil::temp_sibling(&dest, PART_SUFFIX).exists());
    }

    #[tokio::test]
    async fn download_image_rejects_a_non_jpeg_body() {
        // Captive-portal style: 2xx with an HTML body. Persisting it would
        // poison the catalogue permanently (downloads skip existing files).
        let base = crate::testutil::spawn_mock(|_| (200, b"<html>login here</html>".to_vec()));

        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let client = http_client().unwrap();
        let err = download_image(&client, &base, &image, dir.path())
            .await
            .unwrap_err();

        assert!(matches!(err, FetchError::NotJpeg));
        // Nothing persisted — neither the final file nor a .part.
        let dest = download_path(dir.path(), &image);
        assert!(!dest.exists());
        assert!(!crate::fsutil::temp_sibling(&dest, PART_SUFFIX).exists());
    }

    #[test]
    fn sweep_part_files_removes_only_orphaned_parts() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("20260807-Foo_UHD.jpg.part");
        let real = dir.path().join("20260807-Foo_UHD.jpg");
        std::fs::write(&part, b"torn download").unwrap();
        std::fs::write(&real, b"jpeg bytes").unwrap();

        sweep_part_files(dir.path());

        assert!(!part.exists(), "orphaned .part must be swept");
        assert!(real.exists(), "finished downloads must survive");
        // A missing dir is a quiet no-op, not a panic.
        sweep_part_files(&dir.path().join("nope"));
    }

    #[tokio::test]
    async fn fetch_error_display_covers_all_variants() {
        let status = FetchError::Status(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            status.to_string(),
            "Bing returned HTTP 500 Internal Server Error"
        );

        let parse = FetchError::Parse(parse_image_list("not json").unwrap_err());
        assert!(
            parse
                .to_string()
                .starts_with("failed to parse Bing response:")
        );

        assert_eq!(
            FetchError::EmptyList.to_string(),
            "Bing returned an empty image list"
        );
        assert_eq!(
            FetchError::NotJpeg.to_string(),
            "downloaded body is not a JPEG image"
        );

        let io_err: FetchError = io::Error::new(io::ErrorKind::PermissionDenied, "denied").into();
        assert_eq!(io_err.to_string(), "I/O error: denied");

        // Invalid URL yields a reqwest builder error without touching the network.
        let http = FetchError::Http(
            http_client()
                .unwrap()
                .get("not a url")
                .send()
                .await
                .unwrap_err(),
        );
        assert!(http.to_string().starts_with("HTTP request failed:"));
    }

    #[test]
    fn fetch_error_is_local_only_for_io() {
        assert!(FetchError::from(io::Error::other("disk full")).is_local());
        assert!(!FetchError::EmptyList.is_local());
        assert!(!FetchError::NotJpeg.is_local());
        assert!(!FetchError::Status(reqwest::StatusCode::BAD_GATEWAY).is_local());
        assert!(!FetchError::Parse(parse_image_list("x").unwrap_err()).is_local());
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

    #[test]
    fn parse_rejects_startdates_that_could_escape_the_download_dir() {
        // `startdate` feeds the download filename verbatim; anything but
        // 8 ASCII digits is refused at parse time.
        let with_startdate = |startdate: &str| {
            format!(
                r#"{{"images":[{{"urlbase":"/th?id=OHR.Foo_ROW1","startdate":{},
                    "fullstartdate":"202608070700","copyright":"Foo (© Bar)",
                    "copyrightlink":"https://example.com"}}]}}"#,
                serde_json::to_string(startdate).unwrap()
            )
        };
        for hostile in [
            "../../.config/x",
            "2026080",   // 7 digits
            "202608071", // 9 digits
            "2026080a",
            "２０２６０８０７", // full-width digits
            "",
        ] {
            assert!(
                parse_image_list(&with_startdate(hostile)).is_err(),
                "{hostile}"
            );
        }
        assert!(parse_image_list(&with_startdate("20260807")).is_ok());
    }
}
