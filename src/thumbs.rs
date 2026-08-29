// Thumbnail cache: the popup must never decode a full ~5 MB UHD JPEG.
// Thumbnails are generated once per downloaded image (at fetch time) and
// cached under the applet's state dir; the UI only ever loads these.
//
// Every cache slot is described by one sidecar, `<name>.meta`, holding the
// *identity* of the source bytes it was built from (mtime + size) and how
// that attempt ended (`cached` / `failed` — the same two words [`Slot`]
// uses, so the file and the code read alike). Nothing is inferred from the
// cache files' own timestamps: an earlier design compared the slot's mtime
// against the source's, which cannot answer "unchanged since the attempt?" and
// "changed since the attempt?" with one ordering test — a marker written in
// the same filesystem tick as the file it describes is ambiguous either way.
// Identity is compared for exact equality, so a tie is not a case: same
// stamp means the same file, a different stamp means redo the work.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::ImageError;
use image::imageops::FilterType;

use crate::fsutil::{self, PART_SUFFIX};

/// Cached thumbnail size (16:9, popup width — matches the reference's
/// preview proportions).
const THUMB_WIDTH: u32 = 480;
const THUMB_HEIGHT: u32 = 270;

/// Subdirectory of the state dir holding cached thumbnails.
const THUMBS_SUBDIR: &str = "thumbs";

/// Suffix of the sidecar describing a cache slot (see the module comment);
/// `.meta` is the on-disk spelling of "sidecar".
const SIDECAR_SUFFIX: &str = ".meta";

/// The two outcomes a sidecar can record, spelled exactly as the [`Slot`]
/// they map to.
const OUTCOME_CACHED: &str = "cached";
const OUTCOME_FAILED: &str = "failed";

/// A sidecar is one short line; nothing longer can be one of ours, so the
/// read is capped rather than trusting a file in the state dir to be small.
const MAX_SIDECAR_BYTES: u64 = 128;

/// Where the cached thumbnail for `image_path` lives:
/// `<state_dir>/thumbs/<same filename>`. The wallpaper filename is unique
/// (`<startdate>-<name>_<res>.jpg`), so reusing it needs no hashing.
///
/// `None` for a path with no file name (`/`, `..`): treating that as the
/// empty name would alias the cache *directory* itself, which
/// [`ensure_thumbnail`] would then try to rename over and [`reconcile`] to
/// unlink.
pub fn thumbnail_path(image_path: &Path, state_dir: &Path) -> Option<PathBuf> {
    let name = image_path.file_name()?;
    Some(state_dir.join(THUMBS_SUBDIR).join(name))
}

/// Return the cached thumbnail path for `image_path`, (re)generating it if
/// the cache holds nothing built from exactly these source bytes. Decodes
/// the full image only when regeneration is needed; writes atomically
/// (`.part` + rename) so a crash never leaves a torn thumbnail.
///
/// A source that will not decode is remembered right here, at the
/// `image::open` call site — and *only* there, and only when the failure is
/// a verdict about the bytes ([`condemns_source`]). Everything after it
/// (creating the thumbs dir, encoding, the rename) is the state dir's
/// problem, not the image's: a transient ENOSPC/EIO must never
/// negative-cache a perfectly decodable wallpaper, which for an out-of-window
/// entry would mean the placeholder forever (the backfill would skip it for
/// free from then on).
pub fn ensure_thumbnail(image_path: &Path, state_dir: &Path) -> Result<PathBuf, ImageError> {
    let thumb = thumbnail_path(image_path, state_dir).ok_or_else(|| {
        ImageError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no file name", image_path.display()),
        ))
    })?;
    if slot(&thumb, image_path) == Slot::Cached {
        return Ok(thumb);
    }

    if let Some(parent) = thumb.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read the source's identity *before* touching it: if the file changes
    // while we decode or encode, the sidecar describes the bytes we actually
    // read, so the next call sees a mismatch and redoes the work.
    let stamp = source_stamp(image_path);

    let full = match image::open(image_path) {
        Ok(full) => full,
        Err(error) => {
            if condemns_source(&error) {
                write_sidecar(&thumb, OUTCOME_FAILED, stamp.as_deref());
            }
            return Err(error);
        }
    };
    let small = full.resize_to_fill(THUMB_WIDTH, THUMB_HEIGHT, FilterType::Triangle);

    fsutil::write_atomic(&thumb, PART_SUFFIX, |part| {
        small.save_with_format(part, image::ImageFormat::Jpeg)
    })?;
    write_sidecar(&thumb, OUTCOME_CACHED, stamp.as_deref());
    Ok(thumb)
}

/// Whether `error` is a verdict about the source *bytes* — something no
/// retry can fix — rather than a failure to read them.
///
/// `image::open` reports both through the same type: a file that is not an
/// image comes back as `Decoding`/`Unsupported`, but ENOENT, EACCES and EIO
/// on the source arrive as `IoError` and say nothing at all about whether
/// the image decodes. Negative-caching one of those would strand a perfectly
/// good wallpaper on the placeholder until its mtime or size changes — for a
/// file the applet downloaded once and never touches again, never.
///
/// `UnexpectedEof` is the one I/O error that *is* about the bytes (the
/// stream ends mid-image, and a truncated download never grows a tail).
/// Today's JPEG decoder reports that as `Decoding` instead, so the arm is
/// belt-and-braces: a decoder that ever reports it as I/O must not cost the
/// backfill a doomed `image::open` on every refresh, forever.
fn condemns_source(error: &ImageError) -> bool {
    match error {
        ImageError::IoError(io) => io.kind() == std::io::ErrorKind::UnexpectedEof,
        _ => true,
    }
}

/// Whether a usable cached thumbnail for `image_path` already exists —
/// i.e. whether [`ensure_thumbnail`] would return without decoding
/// anything. The backfill pass in `app.rs` uses this to spend its
/// per-refresh budget on real decodes only.
pub fn is_cached(image_path: &Path, state_dir: &Path) -> bool {
    slot_of(image_path, state_dir) == Slot::Cached
}

/// Whether decoding *these* bytes of `image_path` already failed.
///
/// A permanently undecodable file (truncated download, or a non-JPEG saved
/// under a wallpaper name in a folder migrated from the GNOME extension —
/// that extension had no magic-byte check) never becomes [`is_cached`], so
/// without this the backfill would re-`image::open` it forever, once per
/// refresh per file. A repaired or re-downloaded file has a different stamp
/// and is tried again.
pub fn decode_failed(image_path: &Path, state_dir: &Path) -> bool {
    slot_of(image_path, state_dir) == Slot::Failed
}

/// Bring the thumbs dir in line with `live`: everything that is not the
/// cached thumbnail of a live image (or that thumbnail's sidecar) is
/// deleted — pruned entries' leftovers, `.part` files a killed process left
/// mid-write, and artefacts an in-flight [`ensure_thumbnail`] wrote for a
/// path the caller pruned meanwhile.
///
/// A sweep rather than "delete what the prune just reported" on purpose: the
/// backfill decodes on a blocking pool while the UI thread prunes, so a
/// removal list is only correct if the two never overlap — and an artefact
/// that lands after its entry is gone is orphaned *permanently*, since no
/// later prune walks a path the catalogue no longer holds. Sweeping the
/// directory is correct whatever the ordering. Best-effort: failures are
/// logged and skipped.
pub fn reconcile<'a>(live: impl IntoIterator<Item = &'a Path>, state_dir: &Path) {
    let keep: HashSet<&OsStr> = live.into_iter().filter_map(Path::file_name).collect();
    let dir = state_dir.join(THUMBS_SUBDIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No cache dir yet (or it vanished) — nothing to reconcile.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!("cannot scan thumbnail cache {}: {error}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !is_live_slot_file(&name, &keep) {
            remove_quietly(&dir.join(&name));
        }
    }
}

/// Whether `name` is a cache file the live catalogue still needs: a
/// thumbnail whose image is in `keep`, or that thumbnail's sidecar.
/// Anything else — including a `.part` — is sweepable.
fn is_live_slot_file(name: &OsStr, keep: &HashSet<&OsStr>) -> bool {
    if keep.contains(name) {
        return true;
    }
    name.to_str()
        .and_then(|name| name.strip_suffix(SIDECAR_SUFFIX))
        .is_some_and(|base| keep.contains(OsStr::new(base)))
}

/// What the cache holds for a source image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A thumbnail built from exactly these source bytes is on disk.
    Cached,
    /// Decoding exactly these source bytes already failed.
    Failed,
    /// Nothing usable is recorded — do the work.
    Absent,
}

/// [`Slot`] for `image_path`'s cache slot; `Absent` when it has no slot.
fn slot_of(image_path: &Path, state_dir: &Path) -> Slot {
    thumbnail_path(image_path, state_dir).map_or(Slot::Absent, |thumb| slot(&thumb, image_path))
}

/// [`Slot`] for the already-derived cache slot `thumb` of `source`.
///
/// Unreadable or unparseable sidecar, or a stamp that does not match the
/// source's current identity → `Absent` (redo the work — cheap and safe).
fn slot(thumb: &Path, source: &Path) -> Slot {
    let (Some(current), Some(recorded)) =
        (source_stamp(source), read_sidecar(&sidecar_path(thumb)))
    else {
        return Slot::Absent;
    };
    match recorded.trim_end().split_once(' ') {
        // The thumbnail itself must still be there: the sidecar describes
        // the slot, it does not stand in for it.
        Some((OUTCOME_CACHED, stamp)) if stamp == current && thumb.is_file() => Slot::Cached,
        Some((OUTCOME_FAILED, stamp)) if stamp == current => Slot::Failed,
        _ => Slot::Absent,
    }
}

/// Path of the sidecar describing the cache slot `thumb`.
///
/// Shares [`fsutil::temp_sibling`]'s "same path plus a suffix" mechanic —
/// this sibling is permanent rather than temporary, but the crate keeps one
/// implementation of it (see the file and thumbnail rules in `AGENTS.md`).
fn sidecar_path(thumb: &Path) -> PathBuf {
    fsutil::temp_sibling(thumb, SIDECAR_SUFFIX)
}

/// The sidecar's contents, capped at [`MAX_SIDECAR_BYTES`]; `None` when it
/// is missing, unreadable, or not text.
fn read_sidecar(path: &Path) -> Option<String> {
    let mut buf = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_SIDECAR_BYTES)
        .read_to_string(&mut buf)
        .ok()?;
    Some(buf)
}

/// Write the sidecar for cache slot `thumb`. Best-effort — a sidecar that
/// cannot be written only costs a repeat of the work it describes. A `None`
/// stamp (source metadata unreadable, e.g. the file vanished mid-refresh)
/// writes nothing: there is no identity to key the outcome on.
fn write_sidecar(thumb: &Path, outcome: &str, stamp: Option<&str>) {
    let Some(stamp) = stamp else {
        return;
    };
    let sidecar = sidecar_path(thumb);
    if let Err(error) = fs::write(&sidecar, format!("{outcome} {stamp}\n")) {
        tracing::warn!(
            "failed to write thumbnail sidecar {}: {error}",
            sidecar.display()
        );
    }
}

/// Identity of `path`'s current contents: modification time and size.
///
/// Not a hash — hashing means reading the whole ~5 MB file, which is the
/// cost this cache exists to avoid. Two writes that land in the same
/// filesystem timestamp tick *and* produce the same byte count are
/// indistinguishable; for wallpapers re-downloaded from the same URL that is
/// the same image anyway.
fn source_stamp(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let since = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    Some(format!(
        "{}.{:09} {}",
        since.as_secs(),
        since.subsec_nanos(),
        meta.len()
    ))
}

/// Remove `path`, ignoring a missing file and logging anything else.
fn remove_quietly(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!("failed to remove {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Write a tiny valid JPEG (not a real UHD asset) for decode tests.
    fn write_test_jpeg(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        img.save_with_format(path, image::ImageFormat::Jpeg)
            .unwrap();
    }

    /// Pin `path`'s mtime, so a test can stage an exact timestamp tie.
    fn set_mtime(path: &Path, when: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn thumbnail_path_mirrors_filename_under_thumbs() {
        assert_eq!(
            thumbnail_path(
                Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg"),
                Path::new("/home/u/.local/state/app"),
            )
            .as_deref(),
            Some(Path::new(
                "/home/u/.local/state/app/thumbs/20260807-Foo_UHD.jpg"
            ))
        );
        // A path with no file name must never alias the cache directory.
        assert_eq!(thumbnail_path(Path::new("/"), Path::new("/state")), None);
        assert_eq!(thumbnail_path(Path::new(".."), Path::new("/state")), None);
    }

    #[test]
    fn ensure_thumbnail_generates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        let thumb = ensure_thumbnail(&source, &state).unwrap();

        assert_eq!(Some(thumb.clone()), thumbnail_path(&source, &state));
        // Correct size, and readable as an image.
        assert_eq!(
            image::image_dimensions(&thumb).unwrap(),
            (THUMB_WIDTH, THUMB_HEIGHT)
        );
        // The thumbnail and its sidecar, no .part leftover.
        assert_eq!(fs::read_dir(state.join(THUMBS_SUBDIR)).unwrap().count(), 2);
        assert!(sidecar_path(&thumb).is_file());
    }

    #[test]
    fn ensure_thumbnail_skips_when_the_source_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        let thumb = ensure_thumbnail(&source, &state).unwrap();
        // A cached slot is trusted without decoding: garbage written into the
        // thumbnail afterwards must survive untouched.
        fs::write(&thumb, b"sentinel, not a jpeg").unwrap();

        assert_eq!(ensure_thumbnail(&source, &state).unwrap(), thumb);
        assert_eq!(fs::read(&thumb).unwrap(), b"sentinel, not a jpeg");
    }

    #[test]
    fn ensure_thumbnail_regenerates_when_the_source_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        let thumb = ensure_thumbnail(&source, &state).unwrap();
        fs::write(&thumb, b"sentinel, not a jpeg").unwrap();

        // A different image at the same path invalidates the slot.
        write_test_jpeg(&source, 32, 18);
        let got = ensure_thumbnail(&source, &state).unwrap();

        assert_eq!(got, thumb);
        assert_eq!(
            image::image_dimensions(&got).unwrap(),
            (THUMB_WIDTH, THUMB_HEIGHT)
        );
    }

    #[test]
    fn a_deleted_thumbnail_is_regenerated_even_with_its_sidecar_intact() {
        // The sidecar records a verdict about the source; it must never be
        // taken as evidence that the thumbnail itself is still there.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        let thumb = ensure_thumbnail(&source, &state).unwrap();
        fs::remove_file(&thumb).unwrap();

        assert!(!is_cached(&source, &state));
        ensure_thumbnail(&source, &state).unwrap();
        assert!(thumb.is_file());
    }

    #[test]
    fn is_cached_reports_what_ensure_thumbnail_would_skip() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        assert!(!is_cached(&source, &state), "nothing cached yet");
        ensure_thumbnail(&source, &state).unwrap();
        assert!(is_cached(&source, &state), "fresh thumb needs no decode");

        // A changed source is work again.
        write_test_jpeg(&source, 32, 18);
        assert!(!is_cached(&source, &state));

        // No file name → no cache slot, so never "cached".
        assert!(!is_cached(Path::new("/"), &state));
    }

    #[test]
    fn decode_failures_are_remembered_until_the_file_changes() {
        // The backfill pass pays for one `image::open` per file; without the
        // record a permanently undecodable image is re-opened on every
        // refresh, forever.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        fs::write(&source, b"not actually a jpeg").unwrap();
        let state = dir.path().join("state");

        assert!(!decode_failed(&source, &state), "nothing recorded yet");
        assert!(ensure_thumbnail(&source, &state).is_err());
        assert!(
            decode_failed(&source, &state),
            "recorded at the decode site"
        );
        // The record is not a thumbnail.
        assert!(!thumbnail_path(&source, &state).unwrap().exists());
        assert!(!is_cached(&source, &state));

        // Repairing the file invalidates it…
        write_test_jpeg(&source, 64, 36);
        assert!(!decode_failed(&source, &state), "a rewrite means retry");

        // …and a successful generation replaces the verdict outright.
        ensure_thumbnail(&source, &state).unwrap();
        assert!(!decode_failed(&source, &state));
        assert!(is_cached(&source, &state));
    }

    #[test]
    fn a_rewrite_in_the_same_timestamp_tick_still_invalidates_the_failure() {
        // The whole reason the cache stores the source's identity instead of
        // comparing timestamps: a file repaired inside the same filesystem
        // tick the failure was recorded in is a *different file*, and no
        // ordering test (`>=` keeps it marked, `>` unmarks an unchanged one)
        // can say so. Equality can.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        let state = dir.path().join("state");
        let tick = SystemTime::now() - Duration::from_secs(60);

        fs::write(&source, b"not actually a jpeg").unwrap();
        set_mtime(&source, tick);
        assert!(ensure_thumbnail(&source, &state).is_err());
        assert!(decode_failed(&source, &state));

        // Same mtime to the nanosecond, different bytes.
        write_test_jpeg(&source, 64, 36);
        set_mtime(&source, tick);
        assert!(
            !decode_failed(&source, &state),
            "different bytes must be retried, tie or no tie"
        );

        // And the unchanged file stays marked — the other half of the tie.
        fs::write(&source, b"not actually a jpeg").unwrap();
        set_mtime(&source, tick);
        assert!(ensure_thumbnail(&source, &state).is_err());
        assert!(
            decode_failed(&source, &state),
            "an unchanged file stays marked"
        );
    }

    #[test]
    fn a_cache_write_failure_never_negative_caches_a_decodable_image() {
        // Only `image::open` may condemn a file. A state-dir write failure
        // (ENOSPC, EIO — here a directory squatting on the `.part` path) says
        // nothing about the image, and marking it would leave a perfectly
        // decodable wallpaper on the placeholder forever: the backfill skips
        // a marked entry for free and the source's mtime never changes again.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");
        let thumb = thumbnail_path(&source, &state).unwrap();
        fs::create_dir_all(thumb.parent().unwrap()).unwrap();
        fs::create_dir(fsutil::temp_sibling(&thumb, PART_SUFFIX)).unwrap();

        assert!(ensure_thumbnail(&source, &state).is_err());
        assert!(
            !decode_failed(&source, &state),
            "an I/O failure must not condemn a decodable image"
        );
        assert!(!is_cached(&source, &state));

        // With the state dir healthy again the thumbnail is produced.
        fs::remove_dir(fsutil::temp_sibling(&thumb, PART_SUFFIX)).unwrap();
        ensure_thumbnail(&source, &state).unwrap();
        assert!(is_cached(&source, &state));
    }

    #[test]
    fn a_source_that_cannot_be_read_is_never_negative_cached() {
        // `image::open` fails the same way for "these bytes are not an image"
        // and for "the bytes could not be read at all" (EACCES on a file
        // restored with odd modes, EIO off a failing disk). Only the former
        // is a verdict: marking a *read* failure would skip the file for free
        // on every later refresh, so a wallpaper that is perfectly decodable
        // once the transient clears would sit on the placeholder forever —
        // its mtime and size never change again.
        //
        // A directory standing in for the source produces exactly that shape
        // of failure (`IoError`, kind `IsADirectory`) with the metadata — and
        // therefore the identity stamp — still readable, which is what makes
        // a wrong verdict recordable in the first place.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        fs::create_dir(&source).unwrap();
        let state = dir.path().join("state");

        assert!(ensure_thumbnail(&source, &state).is_err());
        assert!(
            !decode_failed(&source, &state),
            "an unreadable source is not an undecodable one"
        );
        assert!(
            !sidecar_path(&thumbnail_path(&source, &state).unwrap()).exists(),
            "no verdict at all was reached"
        );

        // Once the source is readable the thumbnail is produced — no stale
        // verdict stands in the way.
        fs::remove_dir(&source).unwrap();
        write_test_jpeg(&source, 64, 36);
        ensure_thumbnail(&source, &state).unwrap();
        assert!(is_cached(&source, &state));
    }

    #[test]
    fn a_truncated_image_is_still_remembered_as_undecodable() {
        // The other half of the split: a half-written JPEG *is* a verdict
        // about the bytes (the file never grows a tail), so it must keep
        // costing the backfill exactly one decode, not one per refresh.
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.jpg");
        write_test_jpeg(&whole, 64, 36);
        let bytes = fs::read(&whole).unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        fs::write(&source, &bytes[..bytes.len() / 2]).unwrap();
        let state = dir.path().join("state");

        assert!(ensure_thumbnail(&source, &state).is_err());
        assert!(decode_failed(&source, &state));
    }

    #[test]
    fn a_vanished_source_records_nothing() {
        // No identity to key a verdict on: the next refresh must simply retry.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jpg");
        let state = dir.path().join("state");

        assert!(ensure_thumbnail(&missing, &state).is_err());
        assert!(!decode_failed(&missing, &state));
        assert!(
            !sidecar_path(&thumbnail_path(&missing, &state).unwrap()).exists(),
            "no sidecar for a file we never read"
        );
    }

    #[test]
    fn reconcile_keeps_live_slots_and_sweeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let thumbs = state.join(THUMBS_SUBDIR);
        fs::create_dir_all(&thumbs).unwrap();

        let kept = dir.path().join("20260807-Kept_ROW2_UHD.jpg");
        let pruned = dir.path().join("20260801-Gone_ROW1_UHD.jpg");
        for image in [&kept, &pruned] {
            let thumb = thumbnail_path(image, &state).unwrap();
            fs::write(&thumb, b"thumb").unwrap();
            fs::write(sidecar_path(&thumb), b"cached 1 2\n").unwrap();
        }
        // A torn write of a *live* entry, and an unrelated file someone
        // dropped into the cache dir: both sweepable, neither is a live
        // slot file.
        let live_thumb = thumbnail_path(&kept, &state).unwrap();
        fs::write(fsutil::temp_sibling(&live_thumb, PART_SUFFIX), b"torn").unwrap();
        fs::write(thumbs.join("stray-note.txt"), b"").unwrap();

        reconcile([kept.as_path()], &state);

        let survivors: Vec<_> = fs::read_dir(&thumbs)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            survivors.len(),
            2,
            "only the live slot survives: {survivors:?}"
        );
        assert!(live_thumb.is_file());
        assert!(sidecar_path(&live_thumb).is_file());
    }

    #[test]
    fn reconcile_sweeps_what_an_in_flight_generation_wrote_after_the_prune() {
        // The backfill decodes on a blocking pool while the UI thread prunes,
        // so an artefact can land *after* its entry left the catalogue. A
        // removal list built from the prune's own output misses it forever;
        // a directory sweep does not care about the ordering.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260801-Gone_ROW1_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        reconcile(std::iter::empty(), &state);
        // …and only now does the racing generation finish.
        ensure_thumbnail(&source, &state).unwrap();
        reconcile(std::iter::empty(), &state);

        assert_eq!(
            fs::read_dir(state.join(THUMBS_SUBDIR)).unwrap().count(),
            0,
            "a later sweep still collects the orphan"
        );
    }

    #[test]
    fn reconcile_tolerates_a_missing_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("20260807-Kept_ROW2_UHD.jpg");
        // Must not panic, and must not create the dir either.
        reconcile([image.as_path()], dir.path());
        assert!(!dir.path().join(THUMBS_SUBDIR).exists());
    }
}
