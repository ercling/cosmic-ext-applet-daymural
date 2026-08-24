// Bing HPImageArchive API: response types, parsing, and the pure
// URL/filename builders shared with the catalogue rebuild path.
//
// Reference behavior: examples/bing-wallpaper-gnome-extension
// (`utils.js` getImageTitle/toFilename, `extension.js:908-910` for the
// full-width-paren copyright handling that `utils.js:207` misses).

use std::fmt;
use std::io;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::fsutil::{self, PART_SUFFIX};

/// Base URL images are fetched from (`https://www.bing.com<urlbase>_<res>.jpg`).
pub const BING_BASE_URL: &str = "https://www.bing.com";

/// User-Agent sent with every request (Bing serves generic UAs fine; this
/// just identifies us honestly).
pub const USER_AGENT: &str = concat!("daymural/", env!("CARGO_PKG_VERSION"));

/// Prefix every Bing `urlbase` carries; stripped for filenames and
/// re-added when rebuilding a catalogue from a folder scan.
const URLBASE_PREFIX: &str = "/th?id=OHR.";

/// Hardcoded resolution for downloads (v1 scope decision).
pub const RESOLUTION: &str = "UHD";

/// Raw top-level HPImageArchive response. Only `images` is read; it is
/// partitioned into an [`ImageArchive`] by [`parse_image_list`].
#[derive(Debug, Deserialize)]
struct RawArchive {
    images: Vec<BingImage>,
}

/// One image entry as Bing returns it.
///
/// Deliberately omitted: `title` (the literal string `"Info"` — useless;
/// the display title is derived from `copyright`) and `url`/`hsh`/…
/// (unused; resolution is hardcoded to UHD).
///
/// `wp` is Bing's per-image wallpaper eligibility and is read three ways:
/// `Some(true)` = downloadable, `Some(false)` = explicitly ineligible (the
/// only value that authorizes removing an already-downloaded image),
/// `None` (field absent, as a remote payload change could make it) = not
/// downloadable, but nothing is ever removed on its account.
#[derive(Debug, Clone, Deserialize)]
pub struct BingImage {
    pub urlbase: String,
    pub startdate: String,
    pub fullstartdate: String,
    pub copyright: String,
    pub copyrightlink: String,
    #[serde(default)]
    pub wp: Option<bool>,
}

/// An eligible image together with its archive position (`0` = newest),
/// so the download horizon can be applied against Bing's own ordering even
/// after ineligible entries were partitioned out from between them.
#[derive(Debug, Clone)]
pub struct ArchiveImage {
    pub position: usize,
    pub image: BingImage,
}

/// The parsed and partitioned HPImageArchive response. Every structurally
/// valid entry lands in exactly one of the three eligibility buckets;
/// response order is preserved throughout.
#[derive(Debug, Clone, Default)]
pub struct ImageArchive {
    /// Entries with `wp: true` — the only ones an image GET may be issued
    /// for.
    pub eligible: Vec<ArchiveImage>,
    /// `urlbase`s of entries Bing explicitly marked `wp: false`: already
    /// downloaded copies of these are to be removed (entry and file).
    pub ineligible: Vec<String>,
    /// How many structurally valid entries carried no `wp` at all. Logged so
    /// an all-restricted day and a Bing payload change that dropped the
    /// field stay distinguishable.
    pub absent_wp: usize,
    /// The newest structurally valid `fullstartdate` in the response,
    /// regardless of eligibility: the scheduling anchor. Bing publishes on a
    /// 24 h cadence whether or not today's image is eligible, so scheduling
    /// off an older (eligible) entry would hit `next_refresh`'s out-of-range
    /// reset and poll every ~6 min.
    pub anchor: Option<String>,
}

impl ImageArchive {
    /// Whether the response carried no structurally valid entry at all —
    /// [`fetch_image_list`]'s [`FetchError::EmptyList`] condition. A valid
    /// but all-ineligible response is *not* empty: it is a successful no-op.
    ///
    /// Equivalent to `self.anchor.is_none()`, which is what production
    /// tests — every valid entry lands in a bucket *and* anchors; spelled
    /// out over the buckets here so the parser tests can pin that
    /// equivalence rather than assume it.
    #[cfg(test)]
    pub fn has_no_valid_entries(&self) -> bool {
        self.eligible.is_empty() && self.ineligible.is_empty() && self.absent_wp == 0
    }
}

/// A fetched [`ImageArchive`] that is known to hold at least one valid
/// entry, with its scheduling anchor resolved: [`fetch_image_list`] turns an
/// absent anchor into [`FetchError::EmptyList`] exactly once, so no caller
/// has to re-derive "no valid entry" from the buckets.
#[derive(Debug, Clone)]
pub struct FetchedArchive {
    pub archive: ImageArchive,
    /// [`ImageArchive::anchor`], present by construction.
    pub anchor: String,
}

/// Parse an HPImageArchive JSON response, dropping the images whose three
/// path/URL/schedule-forming fields do not validate and partitioning the
/// rest by eligibility (see [`ImageArchive`]). Malformed *JSON* is an error,
/// never a panic; a malformed *image* is skipped.
///
/// Per-image rather than whole-batch rejection on purpose: one anomalous or
/// newly-formatted entry among the eight must not take the applet offline
/// (`Parse` → `Network` → 1 h backoff, forever, with zero images). When
/// nothing survives, [`fetch_image_list`] reports [`FetchError::EmptyList`].
///
/// * `startdate` must be exactly 8 ASCII digits — it is embedded verbatim in
///   the download filename, so a hostile value like `../../.config/x` must
///   never escape the download dir.
/// * `fullstartdate` must be exactly 12 ASCII digits (`YYYYMMDDHHMM`) — it
///   is the scheduling anchor, and one malformed entry must not be able to
///   push the next refresh an hour out (`schedule::ERROR_RETRY_DELAY`).
/// * `urlbase` must carry Bing's own `/th?id=OHR.` prefix — it is
///   concatenated straight onto the base URL, so `"@evil.example/x"` would
///   otherwise turn `https://www.bing.com` into userinfo and send the
///   download to another host entirely.
///
/// Eligibility is never a reason to *reject*: an ineligible entry still
/// counts as structurally valid, still anchors the schedule, and is still
/// reported — it just never produces a download.
pub fn parse_image_list(json: &str) -> Result<ImageArchive, serde_json::Error> {
    let raw: RawArchive = serde_json::from_str(json)?;
    let mut archive = ImageArchive::default();
    for (position, image) in raw.images.into_iter().enumerate() {
        if let Some(reason) = rejection_reason(&image) {
            tracing::warn!("skipping Bing image: {reason}");
            continue;
        }
        if archive
            .anchor
            .as_deref()
            .is_none_or(|newest| image.fullstartdate.as_str() > newest)
        {
            archive.anchor = Some(image.fullstartdate.clone());
        }
        match image.wp {
            Some(true) => archive.eligible.push(ArchiveImage { position, image }),
            Some(false) => archive.ineligible.push(image.urlbase),
            None => archive.absent_wp += 1,
        }
    }
    Ok(archive)
}

/// Whether `value` is exactly `len` ASCII digits.
fn is_ascii_digits(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_digit())
}

/// Why `image` must not be used, if so (see [`parse_image_list`]).
fn rejection_reason(image: &BingImage) -> Option<String> {
    if !is_ascii_digits(&image.startdate, 8) {
        return Some(format!(
            "invalid startdate {:?} (expected 8 ASCII digits)",
            image.startdate
        ));
    }
    if !is_ascii_digits(&image.fullstartdate, 12) {
        return Some(format!(
            "invalid fullstartdate {:?} (expected 12 ASCII digits)",
            image.fullstartdate
        ));
    }
    if !image.urlbase.starts_with(URLBASE_PREFIX) {
        return Some(format!(
            "invalid urlbase {:?} (expected a leading {URLBASE_PREFIX:?})",
            image.urlbase
        ));
    }
    None
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
    /// A response body exceeded its budget ([`MAX_LIST_BYTES`] for the
    /// image list, [`MAX_IMAGE_BYTES`] for a download).
    TooLarge,
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
            Self::Http(error) => write!(f, "HTTP request failed: {error}"),
            Self::Status(status) => write!(f, "Bing returned HTTP {status}"),
            Self::Parse(error) => write!(f, "failed to parse Bing response: {error}"),
            Self::EmptyList => write!(f, "Bing returned an empty image list"),
            Self::NotJpeg => write!(f, "downloaded body is not a JPEG image"),
            Self::TooLarge => write!(f, "response body exceeds its size limit"),
            Self::Io(error) => write!(f, "I/O error: {error}"),
        }
    }
}

impl From<io::Error> for FetchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The archive window Bing supports: `idx=0&n=8` is the complete latest
/// eight; larger `n` is silently capped and `idx > 0` is historical
/// pagination the applet never attempts. The list request is always this
/// constant (a few KB of JSON) — the configured retention governs which of
/// these positions are *downloaded* (`schedule::download_horizon`), never
/// how many are asked for.
pub const ARCHIVE_WINDOW: u8 = 8;

/// HPImageArchive endpoint URL for the full [`ARCHIVE_WINDOW`]. Empty
/// `mkt` means "auto" — must stay in sync with the checked-in fixture's
/// URL. `base_url` is [`BING_BASE_URL`] in production; injected so tests
/// can point at a loopback mock server.
pub fn api_url(base_url: &str) -> String {
    format!("{base_url}/HPImageArchive.aspx?format=js&idx=0&n={ARCHIVE_WINDOW}&mbl=1&mkt=")
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

/// Fetch and parse the image-of-the-day list for the full
/// [`ARCHIVE_WINDOW`] from `base_url` ([`BING_BASE_URL`] in production). A successful but
/// empty list is [`FetchError::EmptyList`] — including a batch whose images
/// were all dropped by [`parse_image_list`]'s validation. A batch whose
/// images are all *ineligible* is not: it parses into an [`ImageArchive`]
/// with nothing to download, which the caller treats as a successful no-op.
/// The scheduling anchor comes back resolved ([`FetchedArchive`]): the
/// schedule must never see an absent one (it would reuse the 5 s cold-start
/// delay and tight-loop against Bing).
pub async fn fetch_image_list(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<FetchedArchive, FetchError> {
    let resp = client
        .get(api_url(base_url))
        .send()
        .await
        .map_err(FetchError::Http)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FetchError::Status(status));
    }
    // Same reasoning as the image download: `gzip` is enabled and reqwest
    // imposes no default response-size limit, so an uncapped read of *this*
    // body — the one contacted first on every refresh — would let a
    // compromised, MITM'd or redirected endpoint drive unbounded allocation
    // with a highly compressible reply.
    let body = read_capped(resp, MAX_LIST_BYTES).await?;
    let body = String::from_utf8_lossy(&body);
    let archive = parse_image_list(&body).map_err(FetchError::Parse)?;
    // Every structurally valid entry anchors, so an absent anchor is the
    // "no valid entry" condition ([`ImageArchive::has_no_valid_entries`]).
    let Some(anchor) = archive.anchor.clone() else {
        return Err(FetchError::EmptyList);
    };
    Ok(FetchedArchive { archive, anchor })
}

/// Where `image` lands on disk inside the download dir
/// (`<dir>/<startdate>-<name>_UHD.jpg`).
pub fn download_path(dir: &Path, image: &BingImage) -> PathBuf {
    dir.join(image_filename(&image.startdate, &image.urlbase))
}

/// Download `image` at UHD into `dir` (created if missing). Skips the
/// network entirely when the file is already there *and still looks like a
/// JPEG*; otherwise writes to a `.part` sibling and renames, so a crashed
/// download never leaves a torn file behind at the final path. Returns the
/// final path.
pub async fn download_image(
    client: &reqwest::Client,
    base_url: &str,
    image: &BingImage,
    dir: &Path,
) -> Result<PathBuf, FetchError> {
    let dest = download_path(dir, image);
    // "Already downloaded" has to mean more than "a file sits at that path":
    // the skip is permanent, so anything else under this name — a captive
    // portal's HTML saved by another tool, a zero-byte stub from a full disk,
    // a folder migrated from the reference GNOME extension, which had no
    // magic-byte check — would be skipped by *every* later refresh, sit in
    // the catalogue as a normal entry, and be applied as the wallpaper for
    // good. The same two bytes a fresh body is checked against decide it;
    // falling through re-downloads over the bad file.
    if dest.exists() {
        if is_jpeg_file(&dest) {
            return Ok(dest);
        }
        tracing::warn!("{} is not a JPEG: re-downloading it", dest.display());
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
    let bytes = read_capped(resp, MAX_IMAGE_BYTES).await?;
    // Captive portals and CDN error pages answer 2xx with HTML; a JPEG
    // always starts FF D8. Never persist anything else.
    if !bytes.starts_with(&JPEG_MAGIC) {
        return Err(FetchError::NotJpeg);
    }
    fsutil::write_atomic(&dest, PART_SUFFIX, |part| std::fs::write(part, &bytes))?;
    Ok(dest)
}

/// The two bytes every JPEG stream starts with (SOI marker).
const JPEG_MAGIC: [u8; 2] = [0xFF, 0xD8];

/// Whether the file at `path` starts with [`JPEG_MAGIC`] — the download's
/// own body check, applied to bytes already on disk.
///
/// Two bytes rather than a decode on purpose: proving an image *whole* means
/// reading all ~5 MB of it, which is the cost the thumbnail cache exists to
/// avoid (and where a decodable-but-truncated file is caught instead, once,
/// and remembered). What this rules out is the file that was never an image
/// at all — the one case no later refresh would ever repair on its own.
/// Unreadable counts as "not a JPEG": a file that cannot be read is no use
/// as a wallpaper either, and re-downloading is the cheapest way to find out.
pub fn is_jpeg_file(path: &Path) -> bool {
    let mut head = [0u8; JPEG_MAGIC.len()];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| head == JPEG_MAGIC)
}

/// Hard ceiling on a downloaded image. Bing's UHD JPEGs run ~5 MB and have
/// never approached this; the point is that `gzip` is enabled, so an
/// unbounded `resp.bytes()` lets a compromised or redirected endpoint drive
/// allocation with a highly compressible body.
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Hard ceiling on the HPImageArchive reply. The real JSON is a few KB for
/// the largest batch the applet ever asks for; 1 MiB leaves room for Bing
/// growing the payload while still bounding the very first response of every
/// refresh.
const MAX_LIST_BYTES: u64 = 1024 * 1024;

/// Read the whole body, refusing anything over `limit` — both by the
/// advertised `Content-Length` (cheap, catches the honest case) and by a
/// running budget over the decompressed stream (the one that actually bounds
/// memory).
async fn read_capped(mut resp: reqwest::Response, limit: u64) -> Result<Vec<u8>, FetchError> {
    if resp.content_length().is_some_and(|n| n > limit) {
        return Err(FetchError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(FetchError::Http)? {
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Remove orphaned `.part` temps a crash mid-download may have left in
/// `dir` (the pipeline sweeps before downloading anew; downloads are
/// single-flight, so nothing here can be in active use). Only *our own*
/// temps qualify: a `.part` whose stem is a Bing wallpaper filename
/// (`parse_filename`). The download folder is shared — the GNOME
/// reference extension uses the same location and the user may keep
/// arbitrary files there — so any other `*.part` must survive.
/// Best-effort: a missing dir or a failed removal is only logged.
pub fn sweep_part_files(dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(PART_SUFFIX)) else {
            continue;
        };
        if parse_filename(stem).is_none() {
            continue; // not one of our temps — leave it alone
        }
        let path = entry.path();
        if path.is_file()
            && let Err(error) = std::fs::remove_file(&path)
        {
            tracing::warn!("failed to remove orphaned {}: {error}", path.display());
        }
    }
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

/// Whether `filename` is a wallpaper filename ([`parse_filename`]) that
/// names `urlbase`'s image — i.e. its `<name>` part matches. The
/// comparison goes through [`urlbase_name`] on both sides (rather than
/// requiring the parse to reconstruct `urlbase` verbatim) so a
/// non-canonical urlbase (extra path components, missing `th?id=OHR.`
/// prefix) still matches the filename [`image_filename`] would build for
/// it. The date part is deliberately *not* compared: an entry may
/// legitimately point at the same image downloaded under a different date
/// (Bing repeats images; `Catalogue::merge` adopts a fresh download when
/// an entry's file vanished).
pub fn filename_names_urlbase(filename: &str, urlbase: &str) -> bool {
    parse_filename(filename)
        .is_some_and(|(_, parsed)| urlbase_name(&parsed) == urlbase_name(urlbase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_uses_the_daymural_identity() {
        assert_eq!(USER_AGENT, concat!("daymural/", env!("CARGO_PKG_VERSION")));
        assert!(!USER_AGENT.contains("cosmic-bing-wallpaper"));
    }

    const FIXTURE: &str = include_str!("../tests/fixtures/hpimagearchive.json");

    #[test]
    fn fixture_parses_with_real_fields() {
        let archive = parse_image_list(FIXTURE).expect("checked-in fixture must parse");
        assert_eq!(archive.eligible.len(), 8);
        assert!(archive.ineligible.is_empty());
        assert_eq!(archive.absent_wp, 0);
        assert_eq!(archive.anchor.as_deref(), Some("202608070700"));

        let first = &archive.eligible[0].image;
        assert_eq!(archive.eligible[0].position, 0);
        assert_eq!(first.urlbase, "/th?id=OHR.ColorfulCop_ROW6097405388");
        assert_eq!(first.startdate, "20260807");
        assert_eq!(first.fullstartdate, "202608070700");
        assert!(first.copyrightlink.starts_with("https://"));

        // Every entry has the fields the catalogue needs, and keeps its
        // archive position.
        for (i, slot) in archive.eligible.iter().enumerate() {
            let img = &slot.image;
            assert_eq!(slot.position, i);
            assert_eq!(img.wp, Some(true));
            assert!(img.urlbase.starts_with("/th?id=OHR."), "{}", img.urlbase);
            assert_eq!(img.startdate.len(), 8);
            assert_eq!(img.fullstartdate.len(), 12);
            assert!(!img.copyright.is_empty());
        }
    }

    #[test]
    fn derived_title_is_a_real_title_not_info() {
        let archive = parse_image_list(FIXTURE).unwrap();
        let (title, copyright) = split_copyright(&archive.eligible[0].image.copyright);
        assert_eq!(
            title,
            "Colourful homes line Nyhavn Canal, Copenhagen, Denmark"
        );
        assert_eq!(copyright, "© emicristea/Getty Images");
        assert_ne!(title, "Info");

        for slot in &archive.eligible {
            let (title, _) = split_copyright(&slot.image.copyright);
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
        for slot in parse_image_list(FIXTURE).unwrap().eligible {
            let img = slot.image;
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
    fn filename_names_urlbase_matches_only_its_own_image() {
        let foo = "/th?id=OHR.Foo_ROW1";
        assert!(filename_names_urlbase("20260807-Foo_ROW1_UHD.jpg", foo));
        // Any date/resolution still names the same image.
        assert!(filename_names_urlbase(
            "20240101-Foo_ROW1_1920x1080.jpg",
            foo
        ));
        // A different image's (perfectly valid) file does not.
        assert!(!filename_names_urlbase("20260807-Bar_ROW2_UHD.jpg", foo));
        // Non-wallpaper names never match.
        assert!(!filename_names_urlbase("vacation.jpg", foo));
        assert!(!filename_names_urlbase("catalogue.json", foo));
        // A non-canonical urlbase still matches the filename built for it
        // (the check must not scrub legitimate entries).
        let odd = "OHR.Odd_ROW3"; // no "/th?id=" prefix
        assert!(filename_names_urlbase(
            &image_filename("20260807", odd),
            odd
        ));
    }

    #[test]
    fn api_url_matches_reference_query() {
        // Always the full supported window — retention never shrinks the
        // request, and `idx` never moves (no historical pagination).
        assert_eq!(
            api_url(BING_BASE_URL),
            "https://www.bing.com/HPImageArchive.aspx?format=js&idx=0&n=8&mbl=1&mkt="
        );
        assert_eq!(ARCHIVE_WINDOW, 8);
    }

    fn fixture_image() -> BingImage {
        parse_image_list(FIXTURE).unwrap().eligible[0].image.clone()
    }

    /// A base URL nothing listens on: any accidental network attempt fails
    /// fast with a connection error instead of reaching the real Bing.
    const DEAD_BASE: &str = "http://127.0.0.1:9";

    #[tokio::test]
    async fn download_image_skips_when_the_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let dest = download_path(dir.path(), &image);
        let existing = crate::testutil::tiny_jpeg(32, 18);
        std::fs::write(&dest, &existing).unwrap();

        // Dead base URL: if the skip check failed, the request would error
        // out (connection refused) and fail the test — the real network is
        // never touched either way.
        let client = http_client().unwrap();
        let got = download_image(&client, DEAD_BASE, &image, dir.path())
            .await
            .unwrap();

        assert_eq!(got, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), existing);
    }

    #[tokio::test]
    async fn download_image_replaces_an_existing_file_that_is_not_a_jpeg() {
        // The skip is permanent: whatever sits at the download path is what
        // the catalogue carries and what gets applied as the wallpaper, for
        // every refresh from now on. A file under a valid Bing name that
        // never was an image (a saved error page, a zero-byte stub, a folder
        // migrated from the reference extension, which checked no magic
        // bytes) must therefore be re-downloaded, not adopted.
        let jpeg = crate::testutil::tiny_jpeg(32, 18);
        let expected = jpeg.clone();
        let base = crate::testutil::spawn_mock(move |_| (200, jpeg.clone()));

        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let dest = download_path(dir.path(), &image);
        let client = http_client().unwrap();

        for corrupt in [b"<html>login here</html>".to_vec(), Vec::new()] {
            std::fs::write(&dest, &corrupt).unwrap();
            let got = download_image(&client, &base, &image, dir.path())
                .await
                .unwrap();
            assert_eq!(got, dest);
            assert_eq!(std::fs::read(&dest).unwrap(), expected);
        }
    }

    #[test]
    fn is_jpeg_file_reads_only_the_magic_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = dir.path().join("real.jpg");
        std::fs::write(&jpeg, crate::testutil::tiny_jpeg(32, 18)).unwrap();
        assert!(is_jpeg_file(&jpeg));

        // Truncated to the marker alone: still "a JPEG" here — proving an
        // image *whole* is the thumbnail cache's job, not a 5 MB re-read on
        // every refresh.
        let stub = dir.path().join("stub.jpg");
        std::fs::write(&stub, JPEG_MAGIC).unwrap();
        assert!(is_jpeg_file(&stub));

        for (name, bytes) in [
            ("html.jpg", b"<html>".as_slice()),
            ("empty.jpg", b""),
            ("short.jpg", &JPEG_MAGIC[..1]),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(!is_jpeg_file(&path), "{name}");
        }
        // Unreadable (here: missing) is not a JPEG either.
        assert!(!is_jpeg_file(&dir.path().join("nope.jpg")));
        assert!(!is_jpeg_file(dir.path()));
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
        let archive = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .unwrap()
            .archive;
        assert_eq!(archive.eligible.len(), 8);
        assert_eq!(
            archive.eligible[0].image.urlbase,
            "/th?id=OHR.ColorfulCop_ROW6097405388"
        );
    }

    #[tokio::test]
    async fn fetch_image_list_maps_error_responses() {
        let base = crate::testutil::spawn_mock(|_| (500, b"oops".to_vec()));
        let err = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Status(s) if s.as_u16() == 500));

        let base = crate::testutil::spawn_mock(|_| (200, b"not json".to_vec()));
        let err = fetch_image_list(&http_client().unwrap(), &base)
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
        let err = fetch_image_list(&http_client().unwrap(), &base)
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
        assert!(!fsutil::temp_sibling(&dest, PART_SUFFIX).exists());
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
        assert!(!fsutil::temp_sibling(&dest, PART_SUFFIX).exists());
    }

    #[test]
    fn sweep_part_files_removes_only_our_orphaned_parts() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("20260807-Foo_UHD.jpg.part");
        let real = dir.path().join("20260807-Foo_UHD.jpg");
        // Foreign partials in the shared folder — a user download and a
        // temp whose stem is not a Bing wallpaper filename. Never touched.
        let foreign = dir.path().join("vacation-video.mp4.part");
        let not_bing = dir.path().join("holiday.jpg.part");
        std::fs::write(&part, b"torn download").unwrap();
        std::fs::write(&real, b"jpeg bytes").unwrap();
        std::fs::write(&foreign, b"someone else's").unwrap();
        std::fs::write(&not_bing, b"someone else's").unwrap();

        sweep_part_files(dir.path());

        assert!(!part.exists(), "our orphaned .part must be swept");
        assert!(real.exists(), "finished downloads must survive");
        assert!(foreign.exists(), "unrelated .part files must survive");
        assert!(not_bing.exists(), "non-Bing .jpg.part files must survive");
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
        assert_eq!(
            FetchError::TooLarge.to_string(),
            "response body exceeds its size limit"
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
        assert!(!FetchError::TooLarge.is_local());
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

    /// One eligible (`wp: true`) image object with the given
    /// `startdate`/`urlbase`; every other field valid.
    fn image_json(urlbase: &str, startdate: &str) -> String {
        image_json_wp(urlbase, startdate, "202608070700", "true")
    }

    /// One image object with every field chosen: `wp` is spliced in as raw
    /// JSON (`"true"`, `"false"`, `"null"`) or omitted entirely for `""`.
    fn image_json_wp(urlbase: &str, startdate: &str, fullstartdate: &str, wp: &str) -> String {
        let wp = if wp.is_empty() {
            String::new()
        } else {
            format!(",\"wp\":{wp}")
        };
        format!(
            r#"{{"urlbase":{},"startdate":{},"fullstartdate":{},
                "copyright":"Foo (© Bar)","copyrightlink":"https://example.com"{wp}}}"#,
            serde_json::to_string(urlbase).unwrap(),
            serde_json::to_string(startdate).unwrap(),
            serde_json::to_string(fullstartdate).unwrap(),
        )
    }

    #[test]
    fn parse_drops_startdates_that_could_escape_the_download_dir() {
        // `startdate` feeds the download filename verbatim; anything but
        // 8 ASCII digits is dropped at parse time.
        let with_startdate = |startdate: &str| {
            format!(
                "{{\"images\":[{}]}}",
                image_json("/th?id=OHR.Foo_ROW1", startdate)
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
                parse_image_list(&with_startdate(hostile))
                    .unwrap()
                    .has_no_valid_entries(),
                "{hostile}"
            );
        }
        assert_eq!(
            parse_image_list(&with_startdate("20260807"))
                .unwrap()
                .eligible
                .len(),
            1
        );
    }

    #[test]
    fn parse_drops_urlbases_that_could_redirect_the_download() {
        // `urlbase` is concatenated straight onto the base URL, so a value
        // starting with `@` turns `https://www.bing.com` into userinfo and
        // the download goes to a host of the attacker's choosing.
        let with_urlbase =
            |urlbase: &str| format!("{{\"images\":[{}]}}", image_json(urlbase, "20260807"));
        for hostile in [
            "@evil.example/x",
            "https://evil.example/x",
            "//evil.example/x",
            "/th?id=Other.Foo_ROW1",
            "",
        ] {
            assert!(
                parse_image_list(&with_urlbase(hostile))
                    .unwrap()
                    .has_no_valid_entries(),
                "{hostile}"
            );
        }
        assert_eq!(
            parse_image_list(&with_urlbase("/th?id=OHR.Foo_ROW1"))
                .unwrap()
                .eligible
                .len(),
            1
        );
        // The URL built from an accepted value never leaves bing.com.
        assert!(
            image_url(BING_BASE_URL, "/th?id=OHR.Foo_ROW1")
                .starts_with("https://www.bing.com/th?id=OHR.")
        );
    }

    #[test]
    fn parse_keeps_the_sound_images_around_a_rejected_one() {
        // The blast radius of one anomalous entry is that entry alone: a
        // whole-batch rejection would surface as `Parse` → 1 h backoff with
        // zero images, forever, for as long as Bing serves it.
        let json = format!(
            "{{\"images\":[{},{},{}]}}",
            image_json("/th?id=OHR.Good_ROW1", "20260806"),
            image_json("@evil.example/x", "20260807"),
            image_json("/th?id=OHR.Good_ROW2", "../../x"),
        );
        let kept = parse_image_list(&json).unwrap().eligible;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].image.urlbase, "/th?id=OHR.Good_ROW1");
        // Position is the *response* index, not the index among survivors.
        assert_eq!(kept[0].position, 0);
    }

    #[tokio::test]
    async fn fetch_image_list_treats_an_all_rejected_batch_as_empty() {
        // Nothing survived validation: `EmptyList` (the documented "back off
        // for an hour" path), not a parse error misreported as unreachable.
        let json = format!("{{\"images\":[{}]}}", image_json("@evil.example/x", "x"));
        let base = crate::testutil::spawn_mock(move |_| (200, json.clone().into_bytes()));
        let err = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .expect_err("an all-rejected batch must not look like a success");
        assert!(matches!(err, FetchError::EmptyList));
    }

    #[tokio::test]
    async fn download_image_refuses_an_oversized_body() {
        // gzip is enabled, so an unbounded read lets a compromised endpoint
        // drive allocation with a highly compressible body.
        let oversized = (MAX_IMAGE_BYTES + 1) as usize;
        let base = crate::testutil::spawn_mock(move |_| {
            let mut body = vec![0u8; oversized];
            body[0] = JPEG_MAGIC[0];
            body[1] = JPEG_MAGIC[1];
            (200, body)
        });

        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let client = http_client().unwrap();
        let err = download_image(&client, &base, &image, dir.path())
            .await
            .unwrap_err();

        assert!(matches!(err, FetchError::TooLarge));
        assert!(!download_path(dir.path(), &image).exists());
    }

    #[tokio::test]
    async fn fetch_image_list_refuses_an_oversized_body() {
        // The list endpoint runs first on every refresh and is the URL the
        // applet always contacts, so it needs the same budget the image
        // download has — an uncapped `text()` there is an OOM handed to
        // whoever answers.
        let oversized = (MAX_LIST_BYTES + 1) as usize;
        let base = crate::testutil::spawn_mock(move |_| (200, vec![b'{'; oversized]));

        let err = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .expect_err("an oversized list body must not be buffered whole");

        assert!(matches!(err, FetchError::TooLarge));
    }

    #[tokio::test]
    async fn download_image_refuses_an_oversized_streamed_body() {
        // The threat model is a *compressed* reply: with gzip enabled reqwest
        // wraps the body in a decoder of unknown size, so `content_length()`
        // is `None` and the cheap pre-check above never fires — the running
        // budget over the stream is then the only thing bounding memory. The
        // EOF-framed mock reproduces exactly that shape.
        let oversized = (MAX_IMAGE_BYTES + 1) as usize;
        let base = crate::testutil::spawn_mock_framed(
            move |_| {
                let mut body = vec![0u8; oversized];
                body[0] = JPEG_MAGIC[0];
                body[1] = JPEG_MAGIC[1];
                (200, body)
            },
            crate::testutil::Framing::UntilEof,
        );

        let dir = tempfile::tempdir().unwrap();
        let image = fixture_image();
        let client = http_client().unwrap();
        let err = download_image(&client, &base, &image, dir.path())
            .await
            .expect_err("an unsized oversized body must not be buffered whole");

        assert!(matches!(err, FetchError::TooLarge));
        assert!(!download_path(dir.path(), &image).exists());
    }

    #[tokio::test]
    async fn fetch_image_list_refuses_an_oversized_streamed_body() {
        let oversized = (MAX_LIST_BYTES + 1) as usize;
        let base = crate::testutil::spawn_mock_framed(
            move |_| (200, vec![b'{'; oversized]),
            crate::testutil::Framing::UntilEof,
        );

        let err = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .expect_err("an unsized oversized list body must not be buffered whole");

        assert!(matches!(err, FetchError::TooLarge));
    }

    #[tokio::test]
    async fn an_unsized_body_within_the_budget_still_arrives_whole() {
        // Guard against "fixing" the streamed budget by refusing every
        // unsized body: a chunked/compressed reply of a normal size is the
        // ordinary case and must parse.
        let base = crate::testutil::spawn_mock_framed(
            |_| (200, FIXTURE.as_bytes().to_vec()),
            crate::testutil::Framing::UntilEof,
        );

        let archive = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .expect("an unsized body under the budget must be read to EOF")
            .archive;
        assert_eq!(archive.eligible.len(), 8);
    }

    #[test]
    fn parse_partitions_entries_by_eligibility_in_response_order() {
        // Three-way `wp`: true → eligible, false → explicitly ineligible,
        // absent → counted only. Each bucket keeps Bing's order, and an
        // eligible entry remembers its *response* position.
        let json = format!(
            "{{\"images\":[{},{},{},{},{}]}}",
            image_json_wp("/th?id=OHR.A_ROW1", "20260807", "202608070700", "true"),
            image_json_wp("/th?id=OHR.B_ROW2", "20260806", "202608060700", "false"),
            image_json_wp("/th?id=OHR.C_ROW3", "20260805", "202608050700", ""),
            image_json_wp("/th?id=OHR.D_ROW4", "20260804", "202608040700", "true"),
            image_json_wp("/th?id=OHR.E_ROW5", "20260803", "202608030700", "false"),
        );
        let archive = parse_image_list(&json).unwrap();

        let eligible: Vec<(usize, &str)> = archive
            .eligible
            .iter()
            .map(|slot| (slot.position, slot.image.urlbase.as_str()))
            .collect();
        assert_eq!(
            eligible,
            vec![(0, "/th?id=OHR.A_ROW1"), (3, "/th?id=OHR.D_ROW4")]
        );
        assert_eq!(
            archive.ineligible,
            vec![
                "/th?id=OHR.B_ROW2".to_owned(),
                "/th?id=OHR.E_ROW5".to_owned()
            ]
        );
        assert_eq!(archive.absent_wp, 1);
        assert_eq!(archive.anchor.as_deref(), Some("202608070700"));
        assert!(!archive.has_no_valid_entries());
    }

    #[test]
    fn parse_treats_absent_wp_as_not_downloadable_but_never_ineligible() {
        // A remote payload change that drops the field must block downloads
        // without ever authorizing a deletion: the entry lands in neither
        // the eligible nor the ineligible bucket.
        let json = format!(
            "{{\"images\":[{}]}}",
            image_json_wp("/th?id=OHR.A_ROW1", "20260807", "202608070700", "")
        );
        let archive = parse_image_list(&json).unwrap();
        assert!(archive.eligible.is_empty());
        assert!(archive.ineligible.is_empty());
        assert_eq!(archive.absent_wp, 1);
        // Still structurally valid: it anchors the schedule and is not an
        // empty list.
        assert_eq!(archive.anchor.as_deref(), Some("202608070700"));
        assert!(!archive.has_no_valid_entries());

        // JSON `null` is the same as absent (serde's `Option` default).
        let json = format!(
            "{{\"images\":[{}]}}",
            image_json_wp("/th?id=OHR.A_ROW1", "20260807", "202608070700", "null")
        );
        let archive = parse_image_list(&json).unwrap();
        assert!(archive.eligible.is_empty() && archive.ineligible.is_empty());
        assert_eq!(archive.absent_wp, 1);
    }

    #[test]
    fn parse_anchors_the_schedule_on_the_newest_valid_entry_regardless_of_eligibility() {
        // Today's image is restricted; the schedule still has to run off
        // *its* fullstartdate, or the next refresh lands in the ~6-minute
        // out-of-range reset instead of tomorrow's publication.
        let json = format!(
            "{{\"images\":[{},{}]}}",
            image_json_wp("/th?id=OHR.New_ROW1", "20260807", "202608070700", "false"),
            image_json_wp("/th?id=OHR.Old_ROW2", "20260806", "202608060700", "true"),
        );
        let archive = parse_image_list(&json).unwrap();
        assert_eq!(archive.anchor.as_deref(), Some("202608070700"));
        assert_eq!(archive.eligible.len(), 1);
        assert_eq!(archive.eligible[0].position, 1);

        // Newest is newest by value, not by position (Bing orders newest
        // first, but the anchor does not rely on it).
        let json = format!(
            "{{\"images\":[{},{}]}}",
            image_json_wp("/th?id=OHR.Old_ROW2", "20260806", "202608060700", "true"),
            image_json_wp("/th?id=OHR.New_ROW1", "20260807", "202608070700", "true"),
        );
        assert_eq!(
            parse_image_list(&json).unwrap().anchor.as_deref(),
            Some("202608070700")
        );
    }

    #[test]
    fn parse_drops_malformed_fullstartdates_before_they_can_anchor_the_schedule() {
        // A structurally bad `fullstartdate` rejects the whole entry: it
        // must neither download nor become the anchor (which would send
        // `next_refresh` down its 1 h error branch).
        for hostile in ["", "2026080707", "20260807070a", "２０２６０８０７０７００"] {
            let json = format!(
                "{{\"images\":[{},{}]}}",
                image_json_wp("/th?id=OHR.Bad_ROW1", "20260807", hostile, "true"),
                image_json_wp("/th?id=OHR.Good_ROW2", "20260806", "202608060700", "true"),
            );
            let archive = parse_image_list(&json).unwrap();
            assert_eq!(archive.eligible.len(), 1, "{hostile:?}");
            assert_eq!(archive.eligible[0].image.urlbase, "/th?id=OHR.Good_ROW2");
            assert_eq!(
                archive.anchor.as_deref(),
                Some("202608060700"),
                "{hostile:?}"
            );
        }
        // A malformed entry that is also ineligible is dropped, not listed
        // for removal — deletion is authorized by a *valid* `wp: false`.
        let json = format!(
            "{{\"images\":[{}]}}",
            image_json_wp("/th?id=OHR.Bad_ROW1", "20260807", "nope", "false")
        );
        let archive = parse_image_list(&json).unwrap();
        assert!(archive.ineligible.is_empty());
        assert!(archive.has_no_valid_entries());
    }

    #[test]
    fn parse_rejects_a_non_boolean_wp_as_malformed_json() {
        // `wp` is typed: a string where a bool belongs is a parse error, the
        // same as any other schema violation, rather than silently "absent".
        let json = format!(
            "{{\"images\":[{}]}}",
            image_json_wp("/th?id=OHR.A_ROW1", "20260807", "202608070700", "\"yes\"")
        );
        assert!(parse_image_list(&json).is_err());
    }

    #[test]
    fn parse_of_an_empty_or_all_invalid_list_has_no_valid_entries() {
        let archive = parse_image_list(r#"{"images":[]}"#).unwrap();
        assert!(archive.has_no_valid_entries());
        assert_eq!(archive.anchor, None);

        let json = format!(
            "{{\"images\":[{}]}}",
            image_json_wp("@evil.example/x", "20260807", "202608070700", "true")
        );
        let archive = parse_image_list(&json).unwrap();
        assert!(archive.has_no_valid_entries());
        assert_eq!(archive.anchor, None);
    }

    #[tokio::test]
    async fn fetch_image_list_accepts_an_all_ineligible_batch_as_a_non_empty_success() {
        // All valid, none downloadable: *not* `EmptyList` (which would back
        // off for an hour and surface an error) — a successful no-op whose
        // anchor schedules the next refresh normally.
        let json = format!(
            "{{\"images\":[{},{}]}}",
            image_json_wp("/th?id=OHR.A_ROW1", "20260807", "202608070700", "false"),
            image_json_wp("/th?id=OHR.B_ROW2", "20260806", "202608060700", ""),
        );
        let base = crate::testutil::spawn_mock(move |_| (200, json.clone().into_bytes()));
        let fetched = fetch_image_list(&http_client().unwrap(), &base)
            .await
            .expect("an all-ineligible batch is a success with nothing to download");
        assert!(fetched.archive.eligible.is_empty());
        assert_eq!(
            fetched.archive.ineligible,
            vec!["/th?id=OHR.A_ROW1".to_owned()]
        );
        assert_eq!(fetched.archive.absent_wp, 1);
        assert_eq!(fetched.anchor, "202608070700");
    }

    #[tokio::test]
    async fn only_eligible_entries_produce_image_requests() {
        // The fetch boundary's contract, observed at the wire: downloading
        // every eligible entry of a mixed batch issues exactly one image GET
        // per `wp: true` entry and none for the `false` or absent ones.
        let json = format!(
            "{{\"images\":[{},{},{}]}}",
            image_json_wp("/th?id=OHR.Yes_ROW1", "20260807", "202608070700", "true"),
            image_json_wp("/th?id=OHR.No_ROW2", "20260806", "202608060700", "false"),
            image_json_wp("/th?id=OHR.Unknown_ROW3", "20260805", "202608050700", ""),
        );
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = std::sync::Arc::clone(&requests);
        let jpeg = crate::testutil::tiny_jpeg(32, 18);
        let base = crate::testutil::spawn_mock(move |path| {
            seen.lock().unwrap().push(path.to_owned());
            if path.starts_with("/HPImageArchive.aspx") {
                (200, json.clone().into_bytes())
            } else {
                (200, jpeg.clone())
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let client = http_client().unwrap();
        let archive = fetch_image_list(&client, &base).await.unwrap().archive;
        for slot in &archive.eligible {
            download_image(&client, &base, &slot.image, dir.path())
                .await
                .unwrap();
        }

        let image_gets: Vec<String> = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.starts_with("/th?id=OHR."))
            .cloned()
            .collect();
        assert_eq!(image_gets, vec![image_url("", "/th?id=OHR.Yes_ROW1")]);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "exactly the eligible image lands on disk"
        );
    }
}
